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
//! - **New best block:** rewind past retracted blocks, then archive each enacted block's sends,
//!   during major sync too.
//! - **First start:** replay from the first block whose runtime has `SpecMsgApi`. If that block's
//!   state is pruned (warp sync, or spec-msg enabled late), seed the archive at the newest
//!   finalized block instead: each stream is then served from its leaf count there, until
//!   [`run_watermark_reader`] backfills what it owes below from the own chain's peers.
//! - **Finality:** apply retention with the watermarks [`run_watermark_reader`] has confirmed.

use crate::{
	archive::Archive,
	fetch::{fetch, RequestError, Transport},
	protocol::MAX_PAYLOAD_BYTES,
	relay::FinalizedRoots,
	store::ArchiveStore,
	LOG_TARGET,
};
use codec::{Decode, DecodeAll, Encode};
use cumulus_primitives_core::{ParaId, SpecMsgApi};
use cumulus_primitives_spec_messaging::{
	streams_root::read_streams_root, FetchError, MessagePosition, MessagesRequest, MmrFrontier,
	Register, StreamId, StreamsRoot, MAX_SPECULATIVE_MESSAGE_LEN,
};
use futures::{select, FutureExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use sc_client_api::BlockchainEvents;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_blockchain::{HeaderBackend, HeaderMetadata};
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
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
	watermarks: Arc<Mutex<BTreeMap<StreamId, u64>>>,
) where
	Block: BlockT,
	C: ProvideRuntimeApi<Block>
		+ BlockchainEvents<Block>
		+ HeaderBackend<Block>
		+ HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ Send
		+ Sync
		+ 'static,
	C::Api: SpecMsgApi<Block>,
	S: ArchiveStore,
{
	// Every import, major sync included. A notification pins its block's state until it is
	// dropped, so state pruning cannot remove a block's sends before they are archived.
	let mut imports = client.every_import_notification_stream().fuse();
	let mut finality = client.finality_notification_stream().fuse();
	let mut archiver = Archiver { client, archive, watermarks, stuck_at: None };
	archiver.follow(archiver.client.info().best_hash);
	loop {
		select! {
			import = imports.next() => match import {
				Some(import) if import.is_new_best => archiver.follow(import.hash),
				Some(_) => {},
				None => return,
			},
			finalized = finality.next() => match finalized {
				Some(finalized) => archiver.retain(finalized.hash),
				None => return,
			},
		}
	}
}

struct Archiver<Block: BlockT, C, S> {
	client: Arc<C>,
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
	watermarks: Arc<Mutex<BTreeMap<StreamId, u64>>>,
	/// The block whose unavailable state was last reported.
	stuck_at: Option<Block::Hash>,
}

impl<Block, C, S> Archiver<Block, C, S>
where
	Block: BlockT,
	C: ProvideRuntimeApi<Block>
		+ HeaderBackend<Block>
		+ HeaderMetadata<Block, Error = sp_blockchain::Error>,
	C::Api: SpecMsgApi<Block>,
	S: ArchiveStore,
{
	fn has_api(&self, hash: Block::Hash) -> Result<bool, sp_api::ApiError> {
		self.client.runtime_api().has_api::<dyn SpecMsgApi<Block>>(hash)
	}

	/// Bring the archive's tip to `target`.
	fn follow(&mut self, target: Block::Hash) {
		let tip = self.archive.read().tip();
		match tip {
			Some(tip) if tip.hash == target => {},
			Some(tip) => {
				let route = match sp_blockchain::tree_route(&*self.client, tip.hash, target) {
					Ok(route) => route,
					Err(e) => {
						log::warn!(target: LOG_TARGET, "No route from the archived tip: {e}");
						return;
					},
				};
				if !route.retracted().is_empty() {
					let common = route.common_block();
					if let Err(e) =
						self.archive.write().rewind_to(common.hash, common.number.saturated_into())
					{
						log::warn!(target: LOG_TARGET, "Archive rewind failed: {e:?}");
						return;
					}
				}
				for block in route.enacted() {
					if !self.import(block.hash) {
						return;
					}
				}
			},
			None => {
				let origin = match self.find_origin(target) {
					Some(Origin::Start(origin)) => origin,
					Some(Origin::Pruned) => {
						if self.seed() {
							self.follow(target);
						}
						return;
					},
					None => return,
				};
				let mut chain = vec![target];
				while chain.last() != Some(&origin) {
					let Some(parent) = chain
						.last()
						.and_then(|hash| self.client.header(*hash).ok().flatten())
						.map(|header| *header.parent_hash())
					else {
						return;
					};
					chain.push(parent);
				}
				log::info!(
					target: LOG_TARGET,
					"Building the spec-msg archive from {origin:?}: {} blocks",
					chain.len(),
				);
				for hash in chain.into_iter().rev() {
					if !self.import(hash) {
						return;
					}
				}
			},
		}
	}

	/// Where the run of API-bearing blocks that ends at `target` starts, or `None` if `target` has
	/// no API or a runtime version cannot be read.
	fn find_origin(&mut self, target: Block::Hash) -> Option<Origin<Block::Hash>> {
		if !self.has_api(target).unwrap_or(false) {
			return None;
		}
		let mut origin = target;
		loop {
			let header = self.client.header(origin).ok().flatten()?;
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
		let hash = self.client.info().finalized_hash;
		let Ok(Some(header)) = self.client.header(hash) else {
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
		let Ok(Some(header)) = self.client.header(hash) else {
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

	/// The state of `hash`, which the archive needs next, is gone: the archive cannot move past
	/// it, so the node serves no newer sends. Every later best block retries and fails the same
	/// way, hence an error, once per block.
	fn unavailable(&mut self, hash: Block::Hash, e: &sp_api::ApiError) {
		if self.stuck_at.replace(hash) == Some(hash) {
			log::debug!(target: LOG_TARGET, "State of {hash:?} still unavailable: {e}");
			return;
		}
		let number = self.client.number(hash).ok().flatten().map(|n| n.to_string());
		log::error!(
			target: LOG_TARGET,
			"The spec-msg archive cannot advance: the state of block #{} ({hash:?}) is \
			 unavailable ({e}). This node serves no newer sends. The archive needs each \
			 canonical block's state when the block is archived.",
			number.as_deref().unwrap_or("?"),
		);
	}

	/// Apply retention at `finalized`, if archived.
	fn retain(&self, finalized: Block::Hash) {
		if !self.archive.read().contains(&finalized) {
			return;
		}
		let watermarks = self.watermarks.lock().clone();
		let cutoff = now().saturating_sub(SERVING_HORIZON.as_secs());
		if let Err(e) = self.archive.write().retain(finalized, &watermarks, cutoff) {
			log::warn!(target: LOG_TARGET, "Archive retention failed: {e:?}");
		}
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

/// Read the recipients' registers after each parachain finality notification, so the archiver can
/// prune channels, then backfill a seeded archive. Only the newest notification is processed after
/// a round.
pub async fn run_watermark_reader<Block, C, S, T, R>(
	client: Arc<C>,
	archive: Arc<RwLock<Archive<Block::Hash, S>>>,
	watermarks: Arc<Mutex<BTreeMap<StreamId, u64>>>,
	us: ParaId,
	transport: Arc<T>,
	roots: Arc<R>,
) where
	Block: BlockT,
	C: BlockchainEvents<Block>,
	S: ArchiveStore,
	T: Transport + ?Sized,
	R: FinalizedRoots + ?Sized,
{
	let mut finality = client.finality_notification_stream().fuse();
	while let Some(mut finalized) = finality.next().await {
		while let Some(Some(newer)) = finality.next().now_or_never() {
			finalized = newer;
		}
		read_watermarks(&archive, &finalized.hash, &watermarks, us, &*transport, &*roots).await;
		backfill(&archive, &finalized.hash, &watermarks, us, &*transport).await;
	}
}

/// One round: fetch from the own chain's peers what each seeded stream owes below its floor. A
/// channel owes its leaves from its watermark, once read; any other stream its head. Fetched under
/// `finalized`'s root, which the own peers still hold. A stream that fails waits for the next
/// round.
pub(crate) async fn backfill<H, S, T>(
	archive: &RwLock<Archive<H, S>>,
	finalized: &H,
	watermarks: &Mutex<BTreeMap<StreamId, u64>>,
	us: ParaId,
	transport: &T,
) where
	H: Encode + Decode + Copy + Eq + std::fmt::Debug,
	S: ArchiveStore,
	T: Transport + ?Sized,
{
	let pending = archive.read().backfill_pending();
	for stream in pending {
		let (served, floor) = {
			let archive = archive.read();
			(archive.served_at(finalized, &stream), archive.payload_floor(&stream))
		};
		let Some((under, count)) = served else { continue };
		let target = match stream {
			StreamId::Channel { .. } => match watermarks.lock().get(&stream) {
				Some(mark) => *mark,
				None => continue,
			},
			_ => count.saturating_sub(1),
		};
		let result = if target >= floor {
			archive.write().backfill_done(&stream)
		} else {
			let Some((frontier, payloads)) =
				fetch_range(transport, us, stream, under, target, floor).await
			else {
				log::debug!(target: LOG_TARGET, "No own peer served {stream:?} below {floor}");
				continue;
			};
			let result = archive.write().backfill(stream, &frontier, &payloads);
			if result.is_ok() {
				log::info!(
					target: LOG_TARGET,
					"Backfilled {stream:?} from {} up to {floor}",
					frontier.leaf_count(),
				);
			}
			result
		};
		if let Err(e) = result {
			log::warn!(target: LOG_TARGET, "Backfilling {stream:?} failed: {e:?}");
		}
	}
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

/// One round: for each channel of `us` that still holds payloads at `finalized`, read the
/// recipient's register under its finalized included root and raise the channel's watermark to the
/// register's `up_to`. A channel that fails is skipped until the next round.
pub(crate) async fn read_watermarks<H, S, T, R>(
	archive: &RwLock<Archive<H, S>>,
	finalized: &H,
	watermarks: &Mutex<BTreeMap<StreamId, u64>>,
	us: ParaId,
	transport: &T,
	roots: &R,
) where
	H: Encode + Decode + Copy + Eq + std::fmt::Debug,
	S: ArchiveStore,
	T: Transport + ?Sized,
	R: FinalizedRoots + ?Sized,
{
	let channels = archive.read().unconfirmed_channels(finalized);
	for stream in channels {
		let StreamId::Channel { recipient, domain, num } = stream else { continue };
		let Some(under) = roots.finalized_included_root(recipient).await else {
			log::debug!(target: LOG_TARGET, "No finalized root for {recipient:?}");
			continue;
		};
		let request = MessagesRequest {
			stream: StreamId::Ack { recipient: us, domain, num },
			under,
			start: None,
			max_bytes: MAX_SPECULATIVE_MESSAGE_LEN,
		};
		let Ok(response) = fetch(transport, recipient, &request, None).await else {
			log::debug!(target: LOG_TARGET, "No register of {recipient:?} for {stream:?}");
			continue;
		};
		let register = match response.payloads.as_slice() {
			[payload] => Register::decode_all(&mut &payload[..]).ok(),
			_ => None,
		};
		let Some(register) = register else {
			log::debug!(target: LOG_TARGET, "Unusable register of {recipient:?} for {stream:?}");
			continue;
		};
		let mut watermarks = watermarks.lock();
		let mark = watermarks.entry(stream).or_default();
		*mark = (*mark).max(register.up_to.0);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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
