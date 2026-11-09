// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! The receiver against a model of its source, the relay chain and its own chain, under random
//! sends, source forks, relay reorgs (back to older roots too), runtime cuts, rounds that fail, a
//! lying peer, node restarts, finality that lands between building a block and noting it, and the
//! source offboarding.
//!
//! The relay chain includes a prefix of the source's blocks; a receiver block counts only while
//! its `Requires` is among the included roots, and is reverted when a relay reorg drops its root.
//! Checked at every step:
//! - every block built from the pool's material assembles, and its `Requires`, synthesized as the
//!   PVF does, is a root the source committed;
//! - the receiver's chain has consumed a prefix of the stream the relay chain includes;
//! - once the peers answer again, the receiver consumes up to the newest included root.

use crate::{
	fetch::Transport,
	protocol,
	tests::{ack, channel, import, Model, TestArchive},
	Archive, Budget, ChainView, MemoryStore, Receiver,
};
use codec::{Decode, Encode};
use cumulus_primitives_core::ParaId;
use cumulus_primitives_spec_messaging::{
	build_requires, leaf_hash, ConsumedStreams, ConsumptionRecord, FetchError, FetchResponse,
	InChannelId, Interval, MessagePosition, MessagingInherentData, MmrFrontier, OutChannelId,
	Register, RegisterStatus, StreamsRoot,
};
use futures::executor::block_on;
use parking_lot::RwLock;
use sc_network::PeerId;
use std::{
	collections::BTreeMap,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
};

const STEPS: usize = 800;
const SEEDS: [u64; 4] = [0x5EC_3A55, 0xA4C_41BE, 0x0DD_BA11, 0x7E1_A7ED];
const SOURCE: u32 = 1000;
const US: u32 = 2000;

/// xorshift64*: deterministic, so a failure replays.
struct Rng(u64);

impl Rng {
	fn next(&mut self) -> u64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}

	/// Uniform in `lo..=hi`.
	fn range(&mut self, lo: u64, hi: u64) -> u64 {
		lo + self.next() % (hi - lo + 1)
	}

	fn chance(&mut self, percent: u64) -> bool {
		self.next() % 100 < percent
	}
}

fn source() -> ParaId {
	SOURCE.into()
}

fn in_id() -> InChannelId {
	InChannelId { sender: source(), domain: 0, num: 0 }
}

fn out_id() -> OutChannelId {
	OutChannelId { recipient: source(), domain: 0, num: 0 }
}

/// The source's collators: an honest peer answering from the archive, and a peer that refuses
/// every request with a made-up `FetchError`.
struct Peers {
	archive: RwLock<TestArchive>,
	down: RwLock<bool>,
	honest: PeerId,
	liar: PeerId,
	lies: AtomicU64,
}

#[async_trait::async_trait]
impl Transport for Peers {
	fn peers(&self, para: ParaId) -> Vec<PeerId> {
		if para == source() {
			vec![self.liar, self.honest]
		} else {
			Vec::new()
		}
	}

	async fn request(&self, peer: PeerId, request: Vec<u8>) -> Result<Vec<u8>, String> {
		if *self.down.read() {
			return Err("connection closed".into());
		}
		if peer == self.liar {
			let lie = match self.lies.fetch_add(1, Ordering::Relaxed) % 4 {
				0 => FetchError::UnknownRoot,
				1 => FetchError::UnknownStream,
				2 => FetchError::BeyondHead,
				_ => FetchError::OutsideHorizon { lowest: MessagePosition(u64::MAX) },
			};
			return Ok(FetchResponse::Err(lie).encode());
		}
		protocol::answer(&self.archive.read(), &request).ok_or_else(|| "refused".into())
	}
}

/// One source block, with the state after it.
struct SourceBlock {
	root: StreamsRoot,
	model: Model,
	/// The channel's payloads so far.
	channel: Vec<Vec<u8>>,
	/// The `Ack` stream's leaf count so far.
	acks: u64,
}

/// The receiving chain's consumption state, as the pallet keeps it.
#[derive(Clone, Default)]
struct Chain {
	frontier: MmrFrontier,
	consumed: Vec<Vec<u8>>,
	read_at: Option<u64>,
	number: u64,
	hash: u64,
}

impl Chain {
	fn cursor(&self) -> u64 {
		self.read_at.map_or(0, |position| position + 1)
	}

	fn view(&self) -> ChainView {
		ChainView::new(
			ConsumedStreams {
				channels: BTreeMap::from([(in_id(), self.frontier.clone())]),
				registers: BTreeMap::from([(out_id(), MessagePosition(self.cursor()))]),
				events: BTreeMap::new(),
			},
			16,
		)
	}

	/// Enact `data` as `enact_messages` does: the chain after it, and the block's record.
	fn enact(&self, data: &MessagingInherentData) -> (Chain, ConsumptionRecord) {
		let mut next = self.clone();
		let mut entries = Vec::new();
		if let Some(payloads) = data.channels.get(&in_id()) {
			let start = next.frontier.root();
			for payload in payloads {
				next.frontier.append(leaf_hash(payload));
				next.consumed.push(payload.clone());
			}
			entries.push((
				in_id().sender_data_stream(US.into()).1,
				Interval { start, end: next.frontier.clone() },
			));
		}
		if let Some(read) = data.registers.get(&out_id()) {
			let position = read.frontier.leaf_count();
			assert!(position >= self.cursor(), "the planner offered a read the pallet rejects");
			Register::decode(&mut &read.payloads[0][..]).expect("the planner checks it decodes");
			let mut end = read.frontier.clone();
			end.append(leaf_hash(&read.payloads[0]));
			next.read_at = Some(position);
			entries.push((
				out_id().peer_ack_stream(US.into()).1,
				Interval { start: read.frontier.root(), end },
			));
		}
		entries.sort_by_key(|(stream, _)| *stream);
		let mut record = ConsumptionRecord::default();
		if !entries.is_empty() {
			record.entries.insert(source(), entries);
		}
		(next, record)
	}
}

/// A receiver block the relay chain included, with the root its `Requires` named.
struct Included {
	chain: Chain,
	/// The source block whose root it named, counted from 1.
	root_block: usize,
}

struct World {
	rng: Rng,
	peers: Arc<Peers>,
	receiver: Receiver<u64, Peers>,
	/// The source's current fork.
	blocks: Vec<SourceBlock>,
	/// Every root the source ever committed, on any fork.
	every_root: Vec<StreamsRoot>,
	/// Source blocks the relay chain includes, and of those the finalized ones.
	included: usize,
	relay_final: usize,
	/// Our finalized chain, and the blocks above it.
	finalized: Chain,
	unfinalized: Vec<Included>,
	next_hash: u64,
	next_payload: u32,
	/// No new source blocks get included: a catch-up runs.
	quiet: bool,
	budget: Budget,
	/// The relay chain answers that it has no root for the source: it is offboarding.
	offboarded: bool,
	/// The relay chain answers so between building the next block and noting it.
	offboard_next: bool,
	built: usize,
	consuming: usize,
	reverted: usize,
}

impl World {
	fn new(seed: u64) -> Self {
		let peers = Arc::new(Peers {
			archive: RwLock::new(Archive::open(Arc::new(MemoryStore::default()))),
			down: RwLock::new(false),
			honest: PeerId::random(),
			liar: PeerId::random(),
			lies: AtomicU64::new(0),
		});
		let budget = Budget { max_payloads: 3, ..Budget::default() };
		Self {
			rng: Rng(seed),
			receiver: Receiver::new(US.into(), budget, peers.clone()),
			budget,
			offboarded: false,
			offboard_next: false,
			peers,
			blocks: Vec::new(),
			every_root: Vec::new(),
			included: 0,
			relay_final: 0,
			finalized: Chain::default(),
			unfinalized: Vec::new(),
			next_hash: 1,
			next_payload: 0,
			quiet: false,
			built: 0,
			consuming: 0,
			reverted: 0,
		}
	}

	fn tip(&self) -> &Chain {
		self.unfinalized.last().map_or(&self.finalized, |block| &block.chain)
	}

	/// The source authors a block on its current fork.
	fn source_block(&mut self) {
		let count = self.rng.range(0, 4);
		let mut sends = Vec::new();
		if count > 0 {
			let payloads = (0..count)
				.map(|_| {
					self.next_payload += 1;
					self.next_payload.encode()
				})
				.collect();
			sends.push((channel(US), payloads));
		}
		if count == 0 || self.rng.chance(25) {
			let register = Register {
				up_to: MessagePosition(self.rng.range(0, 8)),
				status: RegisterStatus::Opened,
			};
			sends.push((ack(US), vec![register.encode()]));
		}
		let (mut model, mut payloads, mut acks) = match self.blocks.last() {
			Some(block) => (block.model.clone(), block.channel.clone(), block.acks),
			None => (Model::default(), Vec::new(), 0),
		};
		for (stream, sent) in &sends {
			if *stream == channel(US) {
				payloads.extend(sent.iter().cloned());
			} else {
				acks += sent.len() as u64;
			}
		}
		let number = self.blocks.len() as u64 + 1;
		let root = import(&mut self.peers.archive.write(), &mut model, number, sends);
		self.every_root.push(root);
		self.blocks.push(SourceBlock { root, model, channel: payloads, acks });
	}

	/// The source drops its blocks above the included ones and authors another.
	fn source_fork(&mut self) {
		if self.blocks.len() <= self.included {
			return;
		}
		self.blocks.truncate(self.included);
		let number = self.included as u64;
		self.peers.archive.write().rewind_to(number, number).unwrap();
		self.source_block();
	}

	/// A relay block: the receiver fetches under the newest included root.
	fn round(&mut self) {
		if self.offboarded {
			self.receiver.forget(&source());
			return;
		}
		let Some(block) = self.included.checked_sub(1).map(|i| &self.blocks[i]) else { return };
		let root = block.root;
		let wants = self.tip().view().sources.remove(&source()).unwrap_or_default();
		block_on(self.receiver.on_root(source(), root, &wants));
	}

	fn include(&mut self) {
		let more = self.rng.range(1, 2) as usize;
		self.included = (self.included + more).min(self.blocks.len());
		self.round();
	}

	/// The relay chain reorgs back to fewer included source blocks, reverting our blocks that
	/// named a dropped root.
	fn relay_reorg(&mut self) {
		let lowest = self.relay_final.max(1);
		if self.included <= lowest {
			return;
		}
		self.included = self.rng.range(lowest as u64, self.included as u64 - 1) as usize;
		let keep = self
			.unfinalized
			.iter()
			.position(|block| block.root_block > self.included)
			.unwrap_or(self.unfinalized.len());
		self.reverted += self.unfinalized.len() - keep;
		self.unfinalized.truncate(keep);
		self.round();
	}

	/// The relay chain finalizes what it includes; our blocks naming those roots become final.
	fn relay_finalize(&mut self) {
		self.relay_final = self.included;
		let last = self.unfinalized.iter().take_while(|b| b.root_block <= self.relay_final).count();
		if let Some(block) = self.unfinalized.drain(..last).last() {
			self.finalized = block.chain;
		}
		self.receiver.on_finalized(self.finalized.number, &self.finalized.view());
	}

	/// Build, maybe cut, note and assemble one block; keep it if the relay chain includes its
	/// root.
	fn build(&mut self) {
		let parent = self.tip().clone();
		let mut data = block_on(self.receiver.inherent_data(&parent.view(), &[]));
		// `create_inherent` may admit a prefix of a channel item, or drop a register read.
		if let Some(payloads) = data.channels.get_mut(&in_id()) {
			if payloads.len() > 1 && self.rng.chance(40) {
				let admitted = self.rng.range(1, payloads.len() as u64 - 1) as usize;
				payloads.truncate(admitted);
			}
		}
		if self.rng.chance(10) {
			data.registers.clear();
		}
		let (mut chain, record) = parent.enact(&data);
		chain.number = parent.number + 1;
		chain.hash = self.next_hash;
		self.next_hash += 1;
		// A relay block and a finality notification may arrive before the import is noted.
		if self.offboard_next {
			self.offboard_next = false;
			self.receiver.forget(&source());
		} else if self.rng.chance(30) {
			if self.quiet {
				self.round();
			} else {
				self.include();
			}
			self.receiver.on_finalized(self.finalized.number, &self.finalized.view());
		}
		block_on(self.receiver.on_block(
			chain.hash,
			parent.hash,
			chain.number,
			false,
			record.clone(),
		));
		self.built += 1;

		assert!(
			!self.offboarded || record.entries.is_empty(),
			"consumed from a source the relay chain has no root for",
		);
		let Some(streams) = record.entries.get(&source()) else {
			self.unfinalized.push(Included { chain, root_block: 0 });
			return;
		};
		self.consuming += 1;
		let lifts = self.receiver.assemble(std::slice::from_ref(&record)).unwrap_or_else(|e| {
			panic!("a block built from the pool's material does not assemble: {e}; {streams:?}")
		});
		let requires = build_requires(std::slice::from_ref(&record), &lifts)
			.expect("the lifts verify as the PVF checks them")
			.expect("a consuming block requires a root");
		let root = *requires.get(source()).expect("one source");
		assert!(self.every_root.contains(&root), "requires a root the source never committed");
		match self.blocks[..self.included].iter().position(|block| block.root == root) {
			Some(i) => self.unfinalized.push(Included { chain, root_block: i + 1 }),
			// Its root is not included on this relay fork: the candidate is dropped.
			None => self.reverted += 1,
		}
	}

	/// Our chain consumed a prefix of the included stream, and read no register past it.
	fn check(&self) {
		let tip = self.tip();
		let included = self.included.checked_sub(1).map(|i| &self.blocks[i]);
		let stream = included.map_or(&[][..], |block| &block.channel[..]);
		assert!(
			tip.consumed.len() <= stream.len() && tip.consumed[..] == stream[..tip.consumed.len()],
			"consumed {} payloads that are not the included stream's first {}",
			tip.consumed.len(),
			stream.len(),
		);
		let acks = included.map_or(0, |block| block.acks);
		assert!(tip.cursor() <= acks, "read a register the included stream does not have");
	}

	/// The node restarts: a fresh receiver, with nothing but what the chain and its peers hold.
	fn restart(&mut self) {
		self.receiver = Receiver::new(US.into(), self.budget, self.peers.clone());
	}

	/// The source offboards with a backlog the receiver holds: a block planned just before the
	/// relay chain answers that it has no root still assembles, and the receiver then builds
	/// blocks that consume nothing from the source.
	fn offboard(&mut self) {
		*self.peers.down.write() = false;
		for _ in 0..3 {
			self.source_block();
		}
		self.included = self.blocks.len();
		self.round();
		self.offboard_next = true;
		self.build();
		self.offboarded = true;
		for _ in 0..20 {
			self.round();
			self.build();
			self.relay_finalize();
			self.check();
		}
	}

	/// With what the relay chain includes final and no new source blocks, the receiver consumes
	/// up to the newest included root. With `outage`, from what two rounds fetched, the peers then
	/// down.
	fn catch_up(&mut self, outage: bool) {
		*self.peers.down.write() = false;
		self.relay_finalize();
		let Some(block) = self.included.checked_sub(1).map(|i| &self.blocks[i]) else { return };
		let (want, acks) = (block.channel.len(), block.acks);
		self.round();
		self.round();
		*self.peers.down.write() = outage;
		self.quiet = true;
		for _ in 0..want + 40 {
			self.round();
			self.build();
			self.relay_finalize();
			self.check();
			let tip = self.tip();
			if tip.consumed.len() == want && (acks == 0 || tip.read_at == Some(acks - 1)) {
				self.quiet = false;
				return;
			}
		}
		let tip = self.tip();
		panic!(
			"stuck (outage: {outage}): consumed {} of {want} payloads, read the register at {:?} \
			 of {acks}",
			tip.consumed.len(),
			tip.read_at,
		);
	}
}

#[test]
fn the_receiver_matches_a_model_under_reorgs_cuts_and_failing_peers() {
	for seed in SEEDS {
		let mut world = World::new(seed);
		for _ in 0..STEPS {
			match world.rng.range(0, 99) {
				0..=23 => world.source_block(),
				24..=43 => world.include(),
				44..=48 => world.relay_reorg(),
				49..=52 => world.source_fork(),
				53..=58 => world.relay_finalize(),
				59..=63 => {
					let down = !*world.peers.down.read();
					*world.peers.down.write() = down;
				},
				64..=66 => {
					let outage = world.rng.chance(50);
					world.catch_up(outage);
				},
				67..=68 => world.restart(),
				_ => world.build(),
			}
			world.check();
		}
		world.catch_up(true);
		world.include();
		world.catch_up(false);
		world.offboard();
		assert!(world.consuming > 50, "seed {seed:#x}: only {} consuming blocks", world.consuming);
		eprintln!(
			"seed {seed:#x}: {} blocks built, {} consuming, {} reverted, {} source blocks",
			world.built,
			world.consuming,
			world.reverted,
			world.blocks.len(),
		);
	}
}
