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

//! The archiver: keeps the [`Archive`] current.
//!
//! - **Every imported block:** archive its sends against its parent, during major sync too.
//! - **First start:** replay from the first block whose runtime has `SpecMsgApi`. If that block's
//!   state is pruned (warp sync, or spec-msg enabled late), seed the archive at the newest
//!   finalized block instead: each stream is then served from its leaf count there, until
//!   [`run_backfill`] backfills what it owes below from the own chain's peers.
//! - **Finality:** apply retention with the sender runtime's applied ACKs at the finalized block.

use crate::{
	archive::Archive,
	fetch::{fetch, RequestError, Rotating, Transport},
	protocol::MAX_PAYLOAD_BYTES,
	store::ArchiveStore,
	LOG_TARGET,
};
use codec::{Decode, Encode};
use cumulus_primitives_core::ParaId;
use cumulus_primitives_spec_messaging::{
	streams_root::read_streams_root, FetchError, MessagePosition, MessagesRequest, MmrFrontier,
	SpecMsgApi, StreamId, StreamsRoot,
};
use futures::{select, FutureExt, StreamExt};
use parking_lot::RwLock;
use sc_client_api::{Backend, BlockchainEvents, FinalityNotifications, ImportNotifications};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_blockchain::Backend as BlockchainBackend;
use sp_runtime::{
	traits::{Block as BlockT, Header as HeaderT, Zero},
	SaturatedConversion,
};
use std::{
	collections::BTreeMap,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long block boundaries, and the proof material of streams without a watermark, are kept:
/// the design's serving horizon (§ Liftability), mirroring relay-chain availability retention.
pub const SERVING_HORIZON: Duration = Duration::from_secs(25 * 60 * 60);

fn now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Keep `archive` in step with `client` until the client's notification streams end. Calls the
/// runtime and the store synchronously; spawn it as a blocking task.
pub async fn run_archiver<Block, C, S>(
	client: Arc<C>,
	backend: Arc<impl Backend<Block>>,
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
) where
	Block: BlockT,
	C: ProvideRuntimeApi<Block> + BlockchainEvents<Block> + Send + Sync + 'static,
	C::Api: SpecMsgApi<Block>,
	S: ArchiveStore,
{
	// Every import, major sync included. A notification pins its block's state until it is
	// dropped, so state pruning cannot remove a block's sends before they are archived.
	let imports = client.every_import_notification_stream();
	let finality = client.finality_notification_stream();
	Archiver { client, chain: backend.blockchain(), archive, stuck_at: None }
		.run(imports, finality)
		.await;
}

struct Archiver<'a, Block: BlockT, C, S> {
	chain: &'a dyn BlockchainBackend<Block>,
	client: Arc<C>,
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
	/// The block whose unavailable state was last reported.
	stuck_at: Option<Block::Hash>,
}

impl<Block, C, S> Archiver<'_, Block, C, S>
where
	Block: BlockT,
	C: ProvideRuntimeApi<Block>,
	C::Api: SpecMsgApi<Block>,
	S: ArchiveStore,
{
	async fn run(
		mut self,
		imports: ImportNotifications<Block>,
		finality: FinalityNotifications<Block>,
	) {
		let mut imports = imports.fuse();
		let mut finality = finality.fuse();
		// A fresh node's best block is genesis. Wait for its first import instead: a warp-synced
		// node would archive genesis and then find every block after it without state, and never
		// seed.
		let mut retry_leaves = true;
		loop {
			if retry_leaves {
				let info = self.chain.info();
				retry_leaves = false;
				if !info.best_number.is_zero() {
					retry_leaves |= !self.finalize(info.finalized_hash);
					match self.chain.leaves() {
						Ok(leaves) => {
							for hash in leaves {
								if !self.follow(hash) {
									retry_leaves = true;
								}
							}
						},
						Err(e) => {
							log::warn!(target: LOG_TARGET, "Cannot enumerate archive heads: {e}");
							retry_leaves = true;
						},
					}
				}
			}
			select! {
				import = imports.next() => match import {
					Some(import) => {
						if !self.follow(import.hash) { retry_leaves = true; }
					},
					None => return,
				},
				finalized = finality.next() => match finalized {
					Some(notification) => retry_leaves |= !self.finalize(notification.hash),
					None => return,
				},
			}
		}
	}

	fn has_api(&self, hash: Block::Hash) -> Result<bool, sp_api::ApiError> {
		self.client.runtime_api().has_api::<dyn SpecMsgApi<Block>>(hash)
	}

	/// Walk back to archived state, then read the missing blocks parent first.
	fn follow(&mut self, target: Block::Hash) -> bool {
		match self.archive.read().has_boundary(&target) {
			Ok(true) => return true,
			Ok(false) => {},
			Err(_) => return false,
		}
		let origin = if self.archive.read().tip().is_none() {
			match self.has_api(target) {
				Ok(true) => {},
				Ok(false) => return true,
				Err(_) => return false,
			}
			let finalized = self.chain.info().finalized_hash;
			if target != finalized && matches!(self.has_api(finalized), Ok(false)) {
				// Activation can occur independently on siblings above this common ancestor.
				if !self.import(finalized) || !self.retain(finalized) {
					return false;
				}
				None
			} else {
				match self.find_origin(target) {
					Some(Origin::Start(hash)) => Some(hash),
					Some(Origin::Pruned) if self.seed() => None,
					_ => return false,
				}
			}
		} else {
			None
		};
		let Ok(anchor) = self.archive.read().anchor() else { return false };
		let mut path = Vec::new();
		let mut hash = target;
		loop {
			match self.archive.read().has_boundary(&hash) {
				Ok(true) => break,
				Ok(false) => {},
				Err(_) => return false,
			}
			let Ok(Some(header)) = self.chain.header(hash) else { return false };
			if anchor.is_some_and(|at| (*header.number()).saturated_into::<u64>() <= at.number) {
				return true;
			}
			path.push(hash);
			if Some(hash) == origin || header.number().is_zero() {
				break;
			}
			hash = *header.parent_hash();
			if anchor.is_none() &&
				self.archive.read().is_origin_parent(&hash, (*header.number()).saturated_into())
			{
				break;
			}
		}
		path.into_iter().rev().all(|hash| self.import(hash))
	}

	/// Where the run of API-bearing blocks that ends at `target` starts, or `None` if `target` has
	/// no API or a runtime version cannot be read.
	fn find_origin(&mut self, target: Block::Hash) -> Option<Origin<Block::Hash>> {
		let mut origin = target;
		loop {
			let header = self.chain.header(origin).ok().flatten()?;
			if header.number().is_zero() {
				return Some(Origin::Start(origin));
			}
			let parent = *header.parent_hash();
			match self.has_api(parent) {
				Ok(true) => origin = parent,
				Ok(false) => return Some(Origin::Start(origin)),
				Err(e) if state_unavailable(&e) => return Some(Origin::Pruned),
				Err(e) => {
					log::warn!(
						target: LOG_TARGET,
						"Cannot build the spec-msg archive: runtime version at {parent:?} \
						 unavailable: {e}",
					);
					return None;
				},
			}
		}
	}

	/// Seed the empty archive at the newest finalized block, whose state the node keeps, from its
	/// `outbound_frontiers()`. `false` if that failed.
	fn seed(&mut self) -> bool {
		let hash = self.chain.info().finalized_hash;
		let Ok(Some(header)) = self.chain.header(hash) else {
			log::warn!(target: LOG_TARGET, "Missing header {hash:?}");
			return false;
		};
		let frontiers = match self.client.runtime_api().outbound_frontiers(hash) {
			Ok(frontiers) => frontiers,
			Err(e) => {
				log::warn!(target: LOG_TARGET, "outbound_frontiers at {hash:?} failed: {e}");
				return false;
			},
		};
		let streams = frontiers.len();
		let result = self.archive.write().seed(
			hash,
			*header.parent_hash(),
			(*header.number()).saturated_into(),
			now(),
			frontiers,
			read_streams_root(header.digest()),
		);
		if let Err(e) = result {
			log::warn!(target: LOG_TARGET, "Seeding the archive at {hash:?} failed: {e:?}");
			return false;
		}
		log::info!(
			target: LOG_TARGET,
			"The state where spec-msg starts is pruned: seeded the archive at #{} ({hash:?}) with \
			 {streams} streams. Their earlier leaves are not served.",
			header.number(),
		);
		true
	}

	/// Archive one block. `false` stops the current walk; the next best block retries.
	fn import(&mut self, hash: Block::Hash) -> bool {
		let Ok(Some(header)) = self.chain.header(hash) else {
			log::warn!(target: LOG_TARGET, "Missing header {hash:?}");
			return false;
		};
		let sends = match self.has_api(hash) {
			Ok(true) => match self.client.runtime_api().outbound_messages(hash) {
				Ok(sends) => sends,
				Err(e) if state_unavailable(&e) => {
					self.unavailable(hash, &e);
					return false;
				},
				Err(e) => {
					log::warn!(target: LOG_TARGET, "outbound_messages at {hash:?} failed: {e}");
					return false;
				},
			},
			Ok(false) => Vec::new(),
			Err(e) if state_unavailable(&e) => {
				self.unavailable(hash, &e);
				return false;
			},
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Runtime version at {hash:?} unavailable: {e}");
				return false;
			},
		};
		let committed = read_streams_root(header.digest());
		let sent: usize = sends.iter().map(|(_, payloads)| payloads.len()).sum();
		let result = self.archive.write().import_block(
			hash,
			*header.parent_hash(),
			(*header.number()).saturated_into(),
			now(),
			sends,
			committed,
		);
		if let Err(e) = result {
			log::warn!(target: LOG_TARGET, "Archiving {hash:?} failed: {e:?}");
			return false;
		}
		if sent > 0 {
			log::debug!(
				target: LOG_TARGET,
				"Archived block #{} {hash:?}: {sent} sends, committing {committed:?}",
				header.number(),
			);
		}
		true
	}

	/// Report unavailable state once per blocked branch tip.
	fn unavailable(&mut self, hash: Block::Hash, e: &sp_api::ApiError) {
		if self.stuck_at.replace(hash) == Some(hash) {
			log::debug!(target: LOG_TARGET, "State of {hash:?} still unavailable: {e}");
			return;
		}
		let number = self.chain.number(hash).ok().flatten().map(|n| n.to_string());
		log::error!(
			target: LOG_TARGET,
			"Cannot archive this branch: state unavailable at block #{} ({hash:?}): {e}",
			number.as_deref().unwrap_or("?"),
		);
	}

	fn finalize(&mut self, hash: Block::Hash) -> bool {
		if !self.follow(hash) {
			return false;
		}
		let known = self.archive.read().has_boundary(&hash);
		match known {
			Ok(true) => self.retain(hash),
			Ok(false) => true,
			Err(_) => false,
		}
	}

	/// Apply retention at `finalized`, if archived.
	fn retain(&self, finalized: Block::Hash) -> bool {
		let api = self.client.runtime_api();
		let watermarks = match api.outbound_up_to(finalized) {
			Ok(acks) => acks.into_iter().map(|(lane, pos)| (lane.data_stream(), pos.0)).collect(),
			Err(error) => {
				if !matches!(self.has_api(finalized), Ok(false)) {
					log::warn!(target: LOG_TARGET, "outbound_up_to at {finalized:?} failed: {error}");
				}
				BTreeMap::new()
			},
		};
		let cutoff = now().saturating_sub(SERVING_HORIZON.as_secs());
		if let Err(e) = self.archive.write().retain(finalized, &watermarks, cutoff) {
			log::warn!(target: LOG_TARGET, "Archive retention failed: {e:?}");
			return false;
		}
		true
	}
}

/// Where the archive starts on an empty store.
enum Origin<H> {
	/// The first block of the run of API-bearing blocks.
	Start(H),
	/// The run starts below the oldest block whose state is kept.
	Pruned,
}

/// Whether a runtime call failed because the block's state is gone, e.g. pruned.
fn state_unavailable(error: &sp_api::ApiError) -> bool {
	matches!(error, sp_api::ApiError::UnknownBlock(_))
}

/// Fetch missing history from our own peers for a seeded archive.
pub async fn run_backfill<Block, C, S, T>(
	client: Arc<C>,
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
	us: ParaId,
	transport: Arc<T>,
) where
	Block: BlockT,
	C: BlockchainEvents<Block>,
	S: ArchiveStore,
	T: Transport + ?Sized,
{
	let transport = Rotating::new(transport);
	let mut finality = client.finality_notification_stream().fuse();
	while let Some(mut finalized) = finality.next().await {
		while let Some(Some(newer)) = finality.next().now_or_never() {
			finalized = newer;
		}
		backfill(&archive, &finalized.hash, us, &transport).await;
	}
}

/// One round: fetch from the own chain's peers what each seeded stream owes below its floor. A
/// channel owes its leaves from its retained floor; any other stream its head. Fetched under
/// `finalized`'s root, which the own peers still hold. A stream that fails waits for the next
/// round.
pub(crate) async fn backfill<H, S, T>(
	archive: &RwLock<Archive<H, S>>,
	finalized: &H,
	us: ParaId,
	transport: &Rotating<T>,
) where
	H: Encode + Decode + Copy + Eq + std::fmt::Debug,
	S: ArchiveStore,
	T: Transport + ?Sized,
{
	let pending = archive.read().backfill_pending();
	for stream in pending {
		let (under, count, range) = {
			let archive = archive.read();
			let Some((under, count)) = archive.served_at(finalized, &stream) else { continue };
			let Some(range) = archive.backfill_range(&stream, count) else { continue };
			(under, count, range)
		};
		let (target, floor) = (range.start, range.end);
		let response = if target >= floor {
			None
		} else {
			let Some((frontier, payloads)) =
				fetch_range(transport, us, stream, under, target, floor).await
			else {
				log::debug!(target: LOG_TARGET, "No own peer served {stream:?} below {floor}");
				continue;
			};
			Some((frontier, payloads))
		};
		let mut archive = archive.write();
		if !archive.backfill_pending().contains(&stream) ||
			archive.backfill_range(&stream, count) != Some(range)
		{
			continue;
		}
		let result = if let Some((frontier, payloads)) = response {
			let result = archive.backfill(stream, target, &frontier, &payloads);
			if result.is_ok() {
				log::info!(
					target: LOG_TARGET,
					"Backfilled {stream:?} from {} up to {floor}",
					frontier.leaf_count(),
				);
			}
			result
		} else {
			archive.backfill_done(&stream)
		};
		if let Err(e) = result {
			log::warn!(target: LOG_TARGET, "Backfilling {stream:?} failed: {e:?}");
		}
	}
	transport.next_round(us);
}

/// `stream`'s leaves from `start` up to `end` under `under`, with the frontier before the first.
/// If every peer has pruned below `start`, it starts from the lowest leaf one still holds.
async fn fetch_range<T: Transport + ?Sized>(
	transport: &T,
	us: ParaId,
	stream: StreamId,
	under: StreamsRoot,
	start: u64,
	end: u64,
) -> Option<(MmrFrontier, Vec<Vec<u8>>)> {
	let (mut at, mut frontier, mut payloads) = (start, None, Vec::new());
	while at < end {
		let request = MessagesRequest {
			stream,
			under,
			start: Some(MessagePosition(at)),
			max_bytes: MAX_PAYLOAD_BYTES,
		};
		match fetch(transport, us, &request, None).await {
			Ok(response) => {
				let taken = response.payloads.len().min((end - at) as usize);
				if taken == 0 {
					return None;
				}
				frontier.get_or_insert(response.frontier);
				payloads.extend(response.payloads.into_iter().take(taken));
				at += taken as u64;
			},
			Err(RequestError::Unserved { refusals }) if frontier.is_none() => {
				let lowest = refusals
					.iter()
					.filter_map(|(_, error)| match error {
						FetchError::OutsideHorizon { lowest } => Some(lowest.0),
						_ => None,
					})
					.min()?;
				if lowest <= at || lowest >= end {
					return None;
				}
				at = lowest;
			},
			Err(_) => return None,
		}
	}
	Some((frontier?, payloads))
}

#[cfg(test)]
mod tests {
	use super::*;

	use crate::tests::{channel, messages, payloads, Model};
	use cumulus_primitives_spec_messaging::{
		verify_messages, ConsumedStreams, ConsumptionRecord, OutChannelId, SPMS_ENGINE_ID,
	};
	use parking_lot::Mutex;
	use polkadot_core_primitives::Hash;
	use sc_client_api::{in_mem::Blockchain, BlockImportNotification, NewBlockState};
	use sc_utils::mpsc::tracing_unbounded;
	use sp_runtime::{generic::DigestItem, testing::Header};

	type Block = sp_runtime::generic::Block<Header, sp_runtime::OpaqueExtrinsic>;
	type Sends = Vec<(StreamId, Vec<Vec<u8>>)>;
	type Acks = BTreeMap<OutChannelId, MessagePosition>;

	#[derive(Clone, Default)]
	struct Runtime {
		sends: Arc<Mutex<BTreeMap<Hash, Sends>>>,
		acks: Arc<Mutex<BTreeMap<Hash, Acks>>>,
	}

	sp_api::mock_impl_runtime_apis! {
		impl SpecMsgApi<Block> for Runtime {
			#[advanced]
			fn outbound_messages(&self, at: Hash) -> Result<Sends, sp_api::ApiError> {
				Ok(self.sends.lock().get(&at).cloned().unwrap_or_default())
			}
			fn outbound_frontiers() -> Vec<(StreamId, MmrFrontier)> { unreachable!() }
			#[advanced]
			fn outbound_up_to(&self, at: Hash) -> Result<Acks, sp_api::ApiError> {
				self.acks.lock().get(&at).cloned().ok_or_else(|| {
					sp_blockchain::Error::UnknownBlock(format!("{at:?}")).into()
				})
			}
			fn consumed_streams() -> ConsumedStreams { unreachable!() }
			fn max_touched_streams() -> u32 { unreachable!() }
			fn consumption_record() -> ConsumptionRecord { unreachable!() }
		}
	}

	impl ProvideRuntimeApi<Block> for Runtime {
		type Api = Self;
		fn runtime_api(&self) -> sp_api::ApiRef<'_, Self> {
			self.clone().into()
		}
	}

	#[test]
	fn worker_serves_never_best_siblings_from_startup_and_notifications() {
		let chain = Blockchain::<Block>::new();
		let runtime = Arc::new(Runtime::default());
		let archive = Arc::new(RwLock::new(Archive::open(crate::MemoryStore::default())));
		let genesis = Header::new_from_number(0);
		chain
			.insert(genesis.hash(), genesis.clone(), None, None, NewBlockState::Final, true)
			.unwrap();
		let stream = channel(2000);
		let insert = |tag, state| {
			let sends = vec![(stream, payloads(tag, 1))];
			let root = Model::default().send(&sends);
			let mut header = Header::new_from_number(1);
			header.parent_hash = genesis.hash();
			header.digest.push(DigestItem::Consensus(SPMS_ENGINE_ID, root.encode()));
			runtime.sends.lock().insert(header.hash(), sends);
			chain.insert(header.hash(), header.clone(), None, None, state, true).unwrap();
			(header, root)
		};
		let (_, best) = insert(1, NewBlockState::Best);
		let (_, startup) = insert(2, NewBlockState::Normal);
		let (tx, imports) = tracing_unbounded("archive-imports", 8);
		let (_final_tx, finality) = tracing_unbounded("archive-finality", 8);
		let (unpin, _rx) = tracing_unbounded("archive-unpin", 8);
		let worker = Archiver {
			chain: &chain,
			client: runtime.clone(),
			archive: archive.clone(),
			stuck_at: None,
		};
		futures::executor::block_on(async {
			let run = worker.run(imports, finality);
			futures::pin_mut!(run);
			assert!(futures::poll!(&mut run).is_pending());
			let (header, live) = insert(3, NewBlockState::Normal);
			tx.unbounded_send(BlockImportNotification::new(
				header.hash(),
				sp_consensus::BlockOrigin::NetworkBroadcast,
				header,
				false,
				None,
				unpin,
			))
			.unwrap();
			drop(tx);
			run.await;
			for (tag, root) in [(1, best), (2, startup), (3, live)] {
				let request = messages(stream, 0, root, u32::MAX);
				let response = archive.read().serve_messages(&request, u32::MAX).unwrap();
				verify_messages(&request, &response).unwrap();
				assert_eq!(response.payloads.into_inner(), payloads(tag, 1));
			}
		});
	}

	#[test]
	fn retention_uses_only_the_ack_at_the_finalized_sender_block() {
		let chain = Blockchain::<Block>::new();
		let client = Arc::new(Runtime::default());
		let archive = Arc::new(RwLock::new(Archive::open(crate::MemoryStore::default())));
		let worker = Archiver {
			chain: &chain,
			client: client.clone(),
			archive: archive.clone(),
			stuck_at: None,
		};
		let lane = OutChannelId { recipient: 2000.into(), domain: 0, num: 1 };
		let stream = lane.data_stream();
		let finalized = Hash::repeat_byte(1);
		let best = Hash::repeat_byte(2);
		let sends = vec![(stream, payloads(1, 5))];
		let root = Model::default().send(&sends);
		archive
			.write()
			.import_block(finalized, Hash::zero(), 1, 0, sends, Some(root))
			.unwrap();
		archive.write().import_block(best, finalized, 2, 0, vec![], None).unwrap();
		client.acks.lock().insert(best, [(lane, MessagePosition(5))].into());

		assert!(worker.retain(finalized));
		let request = messages(stream, 0, root, u32::MAX);
		assert_eq!(archive.read().serve_messages(&request, u32::MAX).unwrap().payloads.len(), 5);

		client.acks.lock().insert(finalized, [(lane, MessagePosition(2))].into());
		assert!(worker.retain(finalized));
		assert_eq!(
			archive.read().serve_messages(&request, u32::MAX),
			Err(FetchError::OutsideHorizon { lowest: MessagePosition(2) })
		);
		let request = messages(stream, 2, root, u32::MAX);
		let reply = archive.read().serve_messages(&request, u32::MAX).unwrap();
		verify_messages(&request, &reply).unwrap();
		assert_eq!(reply.payloads.into_inner(), payloads(1, 5)[2..]);
	}

	#[test]
	fn pruned_state_is_told_apart_from_other_runtime_call_failures() {
		// What a runtime call at a block whose state the database discarded fails with.
		let pruned: sp_api::ApiError =
			sp_blockchain::Error::UnknownBlock("State already discarded for 0x01".into()).into();
		assert!(state_unavailable(&pruned));
		let other: sp_api::ApiError = sp_blockchain::Error::Backend("io".into()).into();
		assert!(!state_unavailable(&other));
	}
}
