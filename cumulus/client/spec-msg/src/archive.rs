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

//! The sender-side archive: the own chain's sends, keyed for the fetch protocol.
//!
//! - **Per stream:** every MMR node by position and every payload by leaf index, so `mmr_lib` can
//!   build a proof at any historic size.
//! - **Per block:** a `Boundary` with each stream's leaf count and root. The recomputed
//!   `StreamsRoot` is indexed only if it equals the header digest, so a divergence shows as an
//!   unknown root, never a wrong proof.
//! - **Per root:** the retained blocks that committed it, to resolve a request's `under`.
//!
//! [`Archive::retain`] prunes channels below their watermark and the rest past the serving horizon.

use crate::{
	store::{ArchiveStore, Batch},
	LOG_TARGET,
};
use codec::{Decode, Encode};
use core::{cell::RefCell, fmt::Debug};
use cumulus_primitives_spec_messaging::{
	leaf_hash,
	mmr::SpecMerge,
	streams_root::{gen_stream_proof, streams_root},
	FetchError, FetchResponse, MessagePosition, MessagesRequest, MessagesResponse,
	MmrExtensionProof, MmrFrontier, MmrRoot, StreamId, StreamsRoot, MAX_MESSAGES_PER_RESPONSE,
};
use mmr_lib::{
	helper::{get_peaks, leaf_index_to_mmr_size},
	MMRStoreReadOps, MMRStoreWriteOps, MMR,
};
use polkadot_core_primitives::Hash;
use std::collections::{BTreeMap, BTreeSet};

const PREFIX: &[u8] = b"spec-msg/archive/v2/";

/// Maximum payloads served per response, whatever the byte budget: each costs a store read, so
/// tiny leaves under a large budget would make a 1 KiB request cost many reads. The protocol's
/// decode bound, so a requester never refuses a full response.
pub const MAX_LEAVES_PER_RESPONSE: u64 = MAX_MESSAGES_PER_RESPONSE as u64;

/// Errors of the archive's write paths. Serving refuses with a [`FetchError`] instead.
#[derive(Debug, PartialEq, Eq)]
pub enum ArchiveError {
	/// The parent is outside the live archive.
	NotChild,
	/// Only an empty archive can be seeded.
	NotEmpty,
	/// Backfilled leaves do not lead to the stored peaks at the stream's floor.
	Diverged,
	/// A block the operation needs is not archived.
	UnknownBlock,
	/// An MMR node the operation needs is missing.
	MissingNode,
	/// The store refused a write.
	Store(String),
}

/// An archived block and its height.
#[derive(Encode, Decode, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tip<H> {
	/// Block hash.
	pub hash: H,
	/// Block number.
	pub number: u64,
}

/// One stream's state at a block: leaf count and MMR root.
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
struct StreamState<H> {
	owners: Vec<(u64, H)>,
	leaf_count: u64,
	root: Hash,
}

/// The stream states after a block with sends, and the root over them.
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
struct BlockState<H> {
	/// The recomputed `StreamsRoot`, if it matched the header digest. `None` means the block is
	/// archived but not served.
	root: Option<StreamsRoot>,
	/// Every stream with at least one leaf, in `StreamId` order.
	streams: Vec<(StreamId, StreamState<H>)>,
}

impl<H> BlockState<H> {
	fn stream(&self, stream: &StreamId) -> Option<&StreamState<H>> {
		self.streams.iter().find(|(s, _)| s == stream).map(|(_, state)| state)
	}

	fn entries(&self) -> BTreeMap<StreamId, Hash> {
		self.streams.iter().map(|(s, state)| (*s, state.root)).collect()
	}
}

/// What the archive keeps per block. The [`BlockState`] itself is stored once, under the block
/// that had the sends; a block without sends points at it, so the state at any block is two reads
/// away.
#[derive(Encode, Decode, Clone, Debug)]
struct Boundary<H> {
	number: u64,
	parent: H,
	/// Unix seconds at import; the serving horizon counts from it.
	imported_at: u64,
	/// The block whose [`BlockState`] holds this block's stream states: itself if it had sends,
	/// else the nearest ancestor that had them. `None` before any send.
	state_at: Option<H>,
}

/// Per stream: everything below `payload` has no payload, everything below `node` keeps only the
/// peaks at `node`.
#[derive(Encode, Decode, Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Floors {
	/// Confirmed pruning can advance within a sparse seed before backfill fills it.
	retained_from: u64,
	payload: u64,
	node: u64,
}

/// The `mmr_lib` node count for `leaf_count` leaves.
fn mmr_size(leaf_count: u64) -> u64 {
	if leaf_count == 0 {
		0
	} else {
		leaf_index_to_mmr_size(leaf_count - 1)
	}
}

fn key(kind: u8, parts: &[&[u8]]) -> Vec<u8> {
	let mut key = PREFIX.to_vec();
	key.push(kind);
	for part in parts {
		key.extend_from_slice(part);
	}
	key
}

fn tip_key() -> Vec<u8> {
	key(b't', &[])
}

fn boundary_key<H: Encode>(hash: &H) -> Vec<u8> {
	key(b'b', &[&hash.encode()])
}

fn root_key(root: &StreamsRoot) -> Vec<u8> {
	key(b'r', &[root.0.as_bytes()])
}

fn node_key(stream: &StreamId, pos: u64) -> Vec<u8> {
	key(b'n', &[&stream.encode(), &pos.to_be_bytes()])
}

fn payload_key(stream: &StreamId, index: u64) -> Vec<u8> {
	key(b'p', &[&stream.encode(), &index.to_be_bytes()])
}

fn floors_key(stream: &StreamId) -> Vec<u8> {
	key(b'f', &[&stream.encode()])
}

fn state_key<H: Encode>(hash: &H) -> Vec<u8> {
	key(b's', &[&hash.encode()])
}

fn canon_key(number: u64) -> Vec<u8> {
	key(b'c', &[&number.to_be_bytes()])
}

fn oldest_key() -> Vec<u8> {
	key(b'o', &[])
}

fn pruned_state_key() -> Vec<u8> {
	key(b'x', &[])
}

/// The seeded streams whose leaves below the floor may still be owed (see [`Archive::seed`]).
fn backfill_key() -> Vec<u8> {
	key(b'k', &[])
}

fn children_key<H: Encode>(parent: &H) -> Vec<u8> {
	key(b'd', &[&parent.encode()])
}

fn owned_key<H: Encode>(owner: &H, base_key: &[u8]) -> Vec<u8> {
	key(b'w', &[&owner.encode(), &base_key[PREFIX.len()..]])
}

fn count_key(stream: &StreamId) -> Vec<u8> {
	key(b'F', &[&stream.encode()])
}

/// One stream's MMR nodes, read from the store with an overlay for nodes written but not yet
/// committed.
struct Nodes<'a, S, H> {
	owners: &'a [(u64, H)],
	base: u64,
	store: &'a S,
	stream: StreamId,
	pending: RefCell<BTreeMap<u64, Hash>>,
}

impl<'a, S: ArchiveStore, H: Encode> Nodes<'a, S, H> {
	fn new(store: &'a S, stream: StreamId, owners: &'a [(u64, H)], base: u64) -> Self {
		Self { store, stream, owners, base, pending: Default::default() }
	}

	fn row(&self, pos: u64, node: bool) -> Option<Vec<u8>> {
		let (base, key) = if node {
			(mmr_size(self.base), node_key(&self.stream, pos))
		} else {
			(self.base, payload_key(&self.stream, pos))
		};
		if pos < base {
			return self.store.get(&key);
		}
		let end = self
			.owners
			.partition_point(|(first, _)| (if node { mmr_size(*first) } else { *first }) <= pos);
		let (_, writer) = self.owners.get(end.checked_sub(1)?)?;
		self.store.get(&owned_key(writer, &key))
	}

	fn get(&self, pos: u64) -> Option<Hash> {
		if let Some(hash) = self.pending.borrow().get(&pos) {
			return Some(*hash);
		}
		self.row(pos, true).and_then(|v| Hash::decode(&mut &v[..]).ok())
	}

	/// The peaks of the MMR with `leaf_count` leaves, highest first.
	fn peaks(&self, leaf_count: u64) -> Option<Vec<Hash>> {
		get_peaks(mmr_size(leaf_count)).into_iter().map(|pos| self.get(pos)).collect()
	}

	fn mmr(&self, leaf_count: u64) -> MMR<Hash, SpecMerge, &Self> {
		MMR::new(mmr_size(leaf_count), self)
	}
}

impl<S: ArchiveStore, H: Encode> MMRStoreReadOps<Hash> for &Nodes<'_, S, H> {
	fn get_elem(&self, pos: u64) -> mmr_lib::Result<Option<Hash>> {
		Ok(self.get(pos))
	}
}

impl<S: ArchiveStore, H: Encode> MMRStoreWriteOps<Hash> for &Nodes<'_, S, H> {
	fn append(&mut self, pos: u64, elems: Vec<Hash>) -> mmr_lib::Result<()> {
		let mut pending = self.pending.borrow_mut();
		for (i, elem) in elems.into_iter().enumerate() {
			pending.insert(pos + i as u64, elem);
		}
		Ok(())
	}
}

/// The sender-side archive. See the module docs.
pub struct Archive<H, S> {
	store: S,
	hash: core::marker::PhantomData<H>,
}

impl<H, S> Archive<H, S>
where
	H: Encode + Decode + Copy + Eq + Debug,
	S: ArchiveStore,
{
	/// Open the archive in `store`, resuming at its persisted tip.
	pub fn open(store: S) -> Self {
		Self { store, hash: Default::default() }
	}

	/// The last imported block, or the anchor if finality discarded that block.
	pub fn tip(&self) -> Option<Tip<H>> {
		self.load(&tip_key()).ok().flatten()
	}

	pub(crate) fn anchor(&self) -> Result<Option<Tip<H>>, ArchiveError> {
		self.load(&key(b'a', &[]))
	}

	pub(crate) fn is_origin_parent(&self, parent: &H, number: u64) -> bool {
		self.store.get(&key(b'i', &[])) == Some((parent, number).encode())
	}

	/// Whether `hash` is archived.
	pub fn contains(&self, hash: &H) -> bool {
		self.has_boundary(hash).unwrap_or(false)
	}

	pub(crate) fn has_boundary(&self, hash: &H) -> Result<bool, ArchiveError> {
		Ok(self.load::<Boundary<H>>(&boundary_key(hash))?.is_some())
	}

	fn boundary(&self, hash: &H) -> Option<Boundary<H>> {
		self.store
			.get(&boundary_key(hash))
			.and_then(|v| Boundary::decode(&mut &v[..]).ok())
	}

	fn block_state(&self, hash: &H) -> Option<BlockState<H>> {
		self.store
			.get(&state_key(hash))
			.and_then(|v| BlockState::decode(&mut &v[..]).ok())
	}

	fn canon(&self, number: u64) -> Option<H> {
		self.store.get(&canon_key(number)).and_then(|v| H::decode(&mut &v[..]).ok())
	}

	fn floors(&self, stream: &StreamId) -> Floors {
		self.store
			.get(&floors_key(stream))
			.and_then(|v| Floors::decode(&mut &v[..]).ok())
			.unwrap_or_default()
	}

	/// The stream states after `hash`. `None` if `hash` is not archived; empty for a block before
	/// any send.
	fn state_at(&self, hash: &H) -> Option<BTreeMap<StreamId, StreamState<H>>> {
		let boundary = self.boundary(hash)?;
		match boundary.state_at {
			Some(at) => Some(self.block_state(&at)?.streams.into_iter().collect()),
			None => Some(BTreeMap::new()),
		}
	}

	fn base_count(&self, stream: &StreamId) -> Result<u64, ArchiveError> {
		Ok(self.load(&count_key(stream))?.unwrap_or(0))
	}

	fn load<T: Decode>(&self, key: &[u8]) -> Result<Option<T>, ArchiveError> {
		self.store
			.try_get(key)
			.map_err(ArchiveError::Store)?
			.map(|v| {
				T::decode(&mut &v[..])
					.map_err(|_| ArchiveError::Store("invalid archive metadata".into()))
			})
			.transpose()
	}

	fn hashes(&self, key: &[u8]) -> Result<Vec<H>, ArchiveError> {
		Ok(self.load(key)?.unwrap_or_default())
	}

	/// Archive sends against the state of the block's own parent.
	pub fn import_block(
		&mut self,
		hash: H,
		parent: H,
		number: u64,
		imported_at: u64,
		sends: Vec<(StreamId, Vec<Vec<u8>>)>,
		committed: Option<StreamsRoot>,
	) -> Result<(), ArchiveError> {
		if self.load::<Boundary<H>>(&boundary_key(&hash))?.is_some() {
			return Ok(());
		}
		let anchor = self.anchor()?;
		if anchor.is_some_and(|at| number <= at.number) {
			return Err(ArchiveError::NotChild);
		}
		let mut batch = Batch::default();
		let parent_boundary = self.load::<Boundary<H>>(&boundary_key(&parent))?;
		if let Some(b) = &parent_boundary {
			if b.number.checked_add(1) != Some(number) {
				return Err(ArchiveError::NotChild);
			}
		} else {
			let origin_key = key(b'i', &[]);
			match self.load::<(H, u64)>(&origin_key)? {
				Some(origin) if origin == (parent, number) && anchor.is_none() => {},
				None if anchor.is_none() => {
					batch.insert.push((origin_key, (parent, number).encode()))
				},
				_ => return Err(ArchiveError::NotChild),
			}
		}
		let mut updates = BTreeMap::<_, Vec<_>>::new();
		for (stream, payloads) in sends {
			if !payloads.is_empty() {
				updates.entry(stream).or_default().extend(payloads);
			}
		}
		let touched = !updates.is_empty();
		let inherited = parent_boundary.and_then(|b| b.state_at);
		let mut streams = BTreeMap::new();
		if let Some(at) = inherited.filter(|_| touched) {
			let state = self.block_state(&at).ok_or(ArchiveError::UnknownBlock)?;
			for (stream, mut s) in state.streams {
				let base = self.base_count(&stream)?;
				s.owners.retain(|(first, _)| *first >= base);
				streams.insert(stream, s);
			}
		}
		for (stream, payloads) in updates {
			let first = streams.get(&stream).map_or(0, |s| s.leaf_count);
			let mut owners = streams.remove(&stream).map(|s| s.owners).unwrap_or_default();
			let nodes = Nodes::new(&self.store, stream, &owners, self.base_count(&stream)?);
			let mut mmr = nodes.mmr(first);
			for (i, payload) in payloads.iter().enumerate() {
				mmr.push(leaf_hash(payload)).map_err(|_| ArchiveError::MissingNode)?;
				batch.insert.push((
					owned_key(&hash, &payload_key(&stream, first + i as u64)),
					payload.clone(),
				));
			}
			let leaf_count = first + payloads.len() as u64;
			let bag = mmr.get_root().map_err(|_| ArchiveError::MissingNode)?;
			let root = MmrRoot::from_bag(leaf_count, bag).0;
			mmr.commit().map_err(|_| ArchiveError::MissingNode)?;
			for (pos, node) in nodes.pending.take() {
				batch.insert.push((owned_key(&hash, &node_key(&stream, pos)), node.encode()));
			}
			owners.push((first, hash));
			streams.insert(stream, StreamState { leaf_count, root, owners });
		}

		let (state, state_at) = if touched {
			let entries = streams.iter().map(|(s, state)| (*s, state.root)).collect();
			let recomputed = streams_root(&entries);
			let root = if recomputed.is_some() && recomputed == committed {
				recomputed
			} else {
				log::warn!(
					target: LOG_TARGET,
					"Block {number} ({hash:?}): recomputed StreamsRoot {recomputed:?} differs from \
					 the header's {committed:?}; archived but not served",
				);
				None
			};
			(Some(BlockState { root, streams: streams.into_iter().collect() }), Some(hash))
		} else {
			if committed.is_some() {
				log::warn!(
					target: LOG_TARGET,
					"Block {number} ({hash:?}) commits a StreamsRoot but sent nothing",
				);
			}
			(None, inherited)
		};

		if let Some(state) = state {
			if let Some(root) = state.root {
				let mut occurrences = self.hashes(&root_key(&root))?;
				occurrences.push(hash);
				batch.insert.push((root_key(&root), occurrences.encode()));
			}
			batch.insert.push((state_key(&hash), state.encode()));
		}
		let mut children = self.hashes(&children_key(&parent))?;
		children.push(hash);
		batch.insert.push((children_key(&parent), children.encode()));
		let boundary = Boundary { number, parent, imported_at, state_at };
		batch.insert.push((boundary_key(&hash), boundary.encode()));
		let tip = Tip { hash, number };
		batch.insert.push((tip_key(), tip.encode()));
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	/// Start an empty archive at `hash` from every stream's frontier there, for a node without the
	/// state of earlier blocks. Only the peaks are stored, so each stream is served from its leaf
	/// count at `hash` on, until [`Self::backfill`] fills what it owes below. The root over the
	/// frontiers is indexed unless `hash`'s header commits a different one; without a digest it is
	/// the root the last sending block committed.
	pub fn seed(
		&mut self,
		hash: H,
		parent: H,
		number: u64,
		imported_at: u64,
		frontiers: Vec<(StreamId, MmrFrontier)>,
		committed: Option<StreamsRoot>,
	) -> Result<(), ArchiveError> {
		if self.load::<Tip<H>>(&tip_key())?.is_some() {
			return Err(ArchiveError::NotEmpty);
		}

		let mut batch = Batch::default();
		let mut streams = BTreeMap::<StreamId, StreamState<H>>::new();
		let mut backfill = BTreeSet::new();
		for (stream, frontier) in frontiers {
			let leaf_count = frontier.leaf_count();
			if leaf_count == 0 {
				continue;
			}
			let positions = get_peaks(mmr_size(leaf_count));
			for (pos, peak) in positions.into_iter().zip(frontier.peaks()) {
				batch.insert.push((node_key(&stream, pos), peak.encode()));
			}
			let floors = Floors { payload: leaf_count, node: leaf_count, retained_from: 0 };
			batch.insert.push((floors_key(&stream), floors.encode()));
			streams.insert(
				stream,
				StreamState { leaf_count, root: frontier.root().0, owners: Vec::new() },
			);
			batch.insert.push((count_key(&stream), leaf_count.encode()));
			backfill.insert(stream);
		}
		batch.insert.push((backfill_key(), backfill.encode()));

		let entries = streams.iter().map(|(s, state)| (*s, state.root)).collect();
		let recomputed = streams_root(&entries);
		let root = if committed.is_none() || committed == recomputed {
			recomputed
		} else {
			log::warn!(
				target: LOG_TARGET,
				"Seed block {number} ({hash:?}): StreamsRoot {recomputed:?} over its frontiers \
				 differs from the header's {committed:?}; seeded but not served",
			);
			None
		};
		if let Some(root) = root {
			batch.insert.push((root_key(&root), vec![hash].encode()));
		}
		let state = BlockState { root, streams: streams.into_iter().collect() };
		batch.insert.push((state_key(&hash), state.encode()));
		let boundary = Boundary { number, parent, imported_at, state_at: Some(hash) };
		batch.insert.push((boundary_key(&hash), boundary.encode()));
		let tip = Tip { hash, number };
		batch.insert.push((tip_key(), tip.encode()));
		batch.insert.push((key(b'a', &[]), tip.encode()));
		batch.insert.push((canon_key(number), hash.encode()));
		batch.insert.push((oldest_key(), number.encode()));
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	/// The seeded streams not yet backfilled or found complete.
	pub(crate) fn backfill_pending(&self) -> BTreeSet<StreamId> {
		self.store
			.get(&backfill_key())
			.and_then(|v| BTreeSet::decode(&mut &v[..]).ok())
			.unwrap_or_default()
	}

	/// `stream` owes nothing below its floor.
	pub(crate) fn backfill_done(&mut self, stream: &StreamId) -> Result<(), ArchiveError> {
		let mut batch = Batch::default();
		self.clear_backfill(&mut batch, stream)?;
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	fn clear_backfill(&self, batch: &mut Batch, stream: &StreamId) -> Result<(), ArchiveError> {
		let mut pending: BTreeSet<StreamId> = self.load(&backfill_key())?.unwrap_or_default();
		pending.remove(stream);
		if pending.is_empty() {
			batch.delete.push(backfill_key());
		} else {
			batch.insert.push((backfill_key(), pending.encode()));
		}
		Ok(())
	}

	/// A pending seed always has a persisted floor, even before its first backfill.
	pub(crate) fn seeded_payload_floor(&self, stream: &StreamId) -> Option<u64> {
		self.load::<Floors>(&floors_key(stream))
			.ok()
			.flatten()
			.map(|floors| floors.payload)
	}

	/// The root `hash` is served under, and `stream`'s leaf count there.
	pub(crate) fn served_at(&self, hash: &H, stream: &StreamId) -> Option<(StreamsRoot, u64)> {
		let at = self.boundary(hash)?.state_at?;
		let state = self.block_state(&at)?;
		Some((state.root?, state.stream(stream)?.leaf_count))
	}

	/// Store a seeded stream's `payloads` from `start` up to its floor, with `frontier` the
	/// stream's frontier at `start`, and lower both floors to `start`. The leaves must lead from
	/// `frontier` to the peaks stored at the floor, else nothing is written.
	pub(crate) fn backfill(
		&mut self,
		stream: StreamId,
		frontier: &MmrFrontier,
		payloads: &[Vec<u8>],
	) -> Result<(), ArchiveError> {
		let start = frontier.leaf_count();
		let floors: Floors = self.load(&floors_key(&stream))?.unwrap_or_default();
		if start < floors.retained_from ||
			floors.payload != floors.node ||
			start.checked_add(payloads.len() as u64) != Some(floors.node)
		{
			return Err(ArchiveError::Diverged);
		}

		let mut batch = Batch::default();
		let nodes = Nodes::<_, H>::new(&self.store, stream, &[], floors.node);
		{
			let mut pending = nodes.pending.borrow_mut();
			for (pos, peak) in get_peaks(mmr_size(start)).into_iter().zip(frontier.peaks()) {
				pending.insert(pos, *peak);
			}
		}
		let mut mmr = nodes.mmr(start);
		for (i, payload) in payloads.iter().enumerate() {
			mmr.push(leaf_hash(payload)).map_err(|_| ArchiveError::MissingNode)?;
			batch.insert.push((payload_key(&stream, start + i as u64), payload.clone()));
		}
		mmr.commit().map_err(|_| ArchiveError::MissingNode)?;
		let built = nodes.pending.take();
		let stored = Nodes::<_, H>::new(&self.store, stream, &[], floors.node);
		let linked = get_peaks(mmr_size(floors.node))
			.into_iter()
			.all(|pos| built.get(&pos).is_some() && built.get(&pos) == stored.get(pos).as_ref());
		if !linked {
			return Err(ArchiveError::Diverged);
		}
		for (pos, node) in built {
			batch.insert.push((node_key(&stream, pos), node.encode()));
		}
		batch.insert.push((
			floors_key(&stream),
			Floors { payload: start, node: start, retained_from: floors.retained_from }.encode(),
		));
		self.clear_backfill(&mut batch, &stream)?;
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	/// Promote one child and retire the sibling subtrees in the same atomic write.
	fn promote(&mut self, hash: H, boundary: Boundary<H>) -> Result<(), ArchiveError> {
		let mut batch = Batch::default();
		let mut removed = BTreeMap::<Hash, Vec<H>>::new();
		let mut siblings = self.hashes(&children_key(&boundary.parent))?;
		if !siblings.contains(&hash) {
			return Err(ArchiveError::UnknownBlock);
		}
		siblings.retain(|h| *h != hash);
		batch.delete.push(children_key(&boundary.parent));
		let mut tip = self.load::<Tip<H>>(&tip_key())?;
		while let Some(stale) = siblings.pop() {
			let b = self.boundary(&stale).ok_or(ArchiveError::UnknownBlock)?;
			siblings.extend(self.hashes(&children_key(&stale))?);
			batch.delete.extend([children_key(&stale), boundary_key(&stale)]);
			if b.state_at == Some(stale) {
				let state = self.block_state(&stale).ok_or(ArchiveError::UnknownBlock)?;
				self.writer_rows(&mut batch, stale, &state, false)?;
				if let Some(root) = state.root {
					removed.entry(root.0).or_default().push(stale);
				}
				batch.delete.push(state_key(&stale));
			}
			if tip.is_some_and(|t| t.hash == stale) {
				tip = None;
			}
		}
		self.remove_occurrences(&mut batch, removed)?;
		if boundary.state_at == Some(hash) {
			let state = self.block_state(&hash).ok_or(ArchiveError::UnknownBlock)?;
			self.writer_rows(&mut batch, hash, &state, true)?;
		}
		let anchor = Tip { hash, number: boundary.number };
		batch.insert.push((key(b'a', &[]), anchor.encode()));
		batch.insert.push((canon_key(boundary.number), hash.encode()));
		if self.load::<u64>(&oldest_key())?.is_none() {
			batch.insert.push((oldest_key(), boundary.number.encode()));
			batch.delete.push(key(b'i', &[]));
		}
		let tip = tip.unwrap_or(anchor);
		batch.insert.push((tip_key(), tip.encode()));
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	fn writer_rows(
		&self,
		batch: &mut Batch,
		hash: H,
		state: &BlockState<H>,
		promote: bool,
	) -> Result<(), ArchiveError> {
		for (stream, s) in &state.streams {
			let Some(&(first, owner)) = s.owners.last() else { continue };
			if owner != hash {
				continue;
			}
			let payloads = (first..s.leaf_count).map(|i| payload_key(stream, i));
			let nodes = (mmr_size(first)..mmr_size(s.leaf_count)).map(|i| node_key(stream, i));
			for key in payloads.chain(nodes) {
				let owned = owned_key(&hash, &key);
				if promote {
					let value = self.store.get(&owned).ok_or(ArchiveError::MissingNode)?;
					batch.insert.push((key, value));
				}
				batch.delete.push(owned);
			}
			if promote {
				batch.insert.push((count_key(stream), s.leaf_count.encode()));
			}
		}
		Ok(())
	}

	fn remove_occurrences(
		&self,
		batch: &mut Batch,
		removed: BTreeMap<Hash, Vec<H>>,
	) -> Result<(), ArchiveError> {
		for (root, hashes) in removed {
			let key = root_key(&StreamsRoot(root));
			let mut owners = self.hashes(&key)?;
			owners.retain(|owner| !hashes.contains(owner));
			if owners.is_empty() {
				batch.delete.push(key);
			} else {
				batch.insert.push((key, owners.encode()));
			}
		}
		Ok(())
	}

	/// Serve a [`MessagesRequest`] with at most `payload_cap` payload bytes.
	///
	/// `start: None` serves the last leaf under `under`: its payload if it fits the budget, else
	/// only its position, with the extension covering the leaf.
	pub fn serve_messages(&self, req: &MessagesRequest, payload_cap: u32) -> FetchResponse {
		let owners = self.hashes(&root_key(&req.under)).map_err(|_| FetchError::UnknownRoot)?;
		for hash in owners {
			if let Some(state) = self.block_state(&hash) {
				if state.root == Some(req.under) {
					let result = self.serve_state(req, payload_cap, &state);
					if !matches!(result, Err(FetchError::UnknownRoot)) {
						return result;
					}
				}
			}
		}
		Err(FetchError::UnknownRoot)
	}

	fn serve_state(
		&self,
		req: &MessagesRequest,
		payload_cap: u32,
		state: &BlockState<H>,
	) -> FetchResponse {
		let stream = state.stream(&req.stream).ok_or(FetchError::UnknownStream)?;
		let leaf_count = stream.leaf_count;
		let nodes = Nodes::new(
			&self.store,
			req.stream,
			&stream.owners,
			self.base_count(&req.stream).map_err(|_| FetchError::UnknownRoot)?,
		);
		let start = match req.start {
			Some(start) if start.0 > leaf_count => return Err(FetchError::BeyondHead),
			Some(start) => start.0,
			None => leaf_count.checked_sub(1).ok_or(FetchError::UnknownStream)?,
		};

		let floors = self.floors(&req.stream);
		let budget = u64::from(req.max_bytes.min(payload_cap));
		let wants_payloads = budget > 0 && start < leaf_count;
		if wants_payloads && start < floors.payload {
			return Err(FetchError::OutsideHorizon { lowest: MessagePosition(floors.payload) });
		}
		// The frontier and the extension need the nodes kept above the node floor.
		if start < floors.node {
			return Err(FetchError::OutsideHorizon { lowest: MessagePosition(floors.node) });
		}

		let mut payloads = Vec::new();
		if wants_payloads {
			let mut used = 0u64;
			for index in start..leaf_count.min(start.saturating_add(MAX_LEAVES_PER_RESPONSE)) {
				let payload = nodes.row(index, false).ok_or_else(|| self.fault(req, "payload"))?;
				used += payload.len() as u64;
				if used > budget {
					break;
				}
				payloads.push(payload);
			}
		}
		let end = start + payloads.len() as u64;
		let payloads = payloads.try_into().map_err(|_| self.fault(req, "payload count"))?;

		let frontier = nodes
			.peaks(start)
			.and_then(|peaks| MmrFrontier::from_parts(peaks, start))
			.ok_or_else(|| self.fault(req, "peaks"))?;
		let extension = Self::extension(&nodes, end, leaf_count)
			.ok_or_else(|| self.fault(req, "extension nodes"))?;
		let (_, tree_proof) = gen_stream_proof(&state.entries(), req.stream)
			.ok_or_else(|| self.fault(req, "stream proof"))?;
		Ok(MessagesResponse { frontier, payloads, extension, tree_proof })
	}

	/// A root the archive resolved but cannot fully serve: report it as unknown.
	fn fault(&self, req: &MessagesRequest, what: &str) -> FetchError {
		log::warn!(
			target: LOG_TARGET,
			"Archive fault: missing {what} for {:?} from {:?} under {:?}",
			req.stream,
			req.start,
			req.under,
		);
		FetchError::UnknownRoot
	}

	/// The extension proof from `from` to `to` leaves.
	fn extension(nodes: &Nodes<'_, S, H>, from: u64, to: u64) -> Option<MmrExtensionProof> {
		if from == to {
			return Some(MmrExtensionProof {
				leaf_count: to,
				connecting_nodes: Default::default(),
			});
		}
		if from == 0 {
			// Nothing to extend from: the connecting nodes are the new MMR's peaks.
			return Some(MmrExtensionProof {
				leaf_count: to,
				connecting_nodes: nodes.peaks(to)?.try_into().ok()?,
			});
		}
		let proof = nodes.mmr(to).gen_ancestry_proof(mmr_size(from)).ok()?;
		Some(MmrExtensionProof {
			leaf_count: to,
			connecting_nodes: proof
				.prev_peaks_proof
				.proof_items()
				.iter()
				.map(|(_, h)| *h)
				.collect::<Vec<_>>()
				.try_into()
				.ok()?,
		})
	}

	/// The channels at `finalized` that still hold payloads above their floor, or may owe a
	/// backfill below it: their watermark decides both.
	pub fn unconfirmed_channels(&self, finalized: &H) -> Vec<StreamId> {
		let backfill = self.backfill_pending();
		self.state_at(finalized)
			.unwrap_or_default()
			.into_iter()
			.filter(|(stream, state)| {
				matches!(stream, StreamId::Channel { .. }) &&
					(state.leaf_count > self.floors(stream).payload || backfill.contains(stream))
			})
			.map(|(stream, _)| stream)
			.collect()
	}

	/// Apply retention at `finalized`: drop boundaries imported before `horizon_cutoff` (never
	/// `finalized` itself) with their roots, except the root the oldest kept boundary still points
	/// at; prune each channel below its watermark (none: keep all); keep every other stream from
	/// the first leaf the oldest kept boundary sent, or else from its head. `watermarks` must be
	/// read at `finalized`, so a reverted read never prunes.
	pub fn retain(
		&mut self,
		finalized: H,
		watermarks: &BTreeMap<StreamId, u64>,
		horizon_cutoff: u64,
	) -> Result<(), ArchiveError> {
		let anchor = self.anchor()?;
		let top = self.boundary(&finalized).ok_or(ArchiveError::UnknownBlock)?.number;
		if anchor.is_some_and(|a| a.number > top) {
			return Ok(());
		}
		let mut path = Vec::new();
		let mut cursor = finalized;
		while !anchor.is_some_and(|a| a.hash == cursor) {
			let b = self.boundary(&cursor).ok_or(ArchiveError::UnknownBlock)?;
			if anchor.is_some_and(|a| b.number <= a.number) {
				return Err(ArchiveError::NotChild);
			}
			let parent = b.parent;
			let origin = anchor.is_none() &&
				self.store.get(&key(b'i', &[])) == Some((parent, b.number).encode());
			path.push((cursor, b));
			if origin {
				break;
			}
			cursor = parent;
		}
		for (h, b) in path.into_iter().rev() {
			self.promote(h, b)?;
		}
		let mut oldest = self.load::<u64>(&oldest_key())?.ok_or(ArchiveError::UnknownBlock)?;
		let mut batch = Batch::default();

		// Drop boundaries past the horizon, oldest first.
		let mut dropped_states = Vec::new();
		while oldest < top {
			let hash = self.canon(oldest).ok_or(ArchiveError::UnknownBlock)?;
			let boundary = self.boundary(&hash).ok_or(ArchiveError::UnknownBlock)?;
			if boundary.imported_at >= horizon_cutoff {
				break;
			}
			batch.delete.push(boundary_key(&hash));
			batch.delete.push(canon_key(oldest));
			if boundary.state_at == Some(hash) {
				dropped_states.push(hash);
			}
			oldest += 1;
		}
		// A dropped block's state is still current while the kept blocks have sent nothing since:
		// they all point at it, and for an idle sender its root is the newest included one. Keep
		// that state and its root; delete every other dropped state with its root. Only the oldest
		// kept block can point at a dropped state, and every later kept one then points at it too.
		let oldest_hash = self.canon(oldest).ok_or(ArchiveError::UnknownBlock)?;
		let oldest_boundary = self.boundary(&oldest_hash).ok_or(ArchiveError::UnknownBlock)?;
		let current = oldest_boundary.state_at;
		let previous: Option<H> = self.load(&pruned_state_key())?;
		let mut pruned_state = None;
		let mut removed = BTreeMap::<Hash, Vec<H>>::new();
		for hash in previous.into_iter().chain(dropped_states) {
			if Some(hash) == current {
				pruned_state = Some(hash);
				continue;
			}
			if let Some(root) = self.block_state(&hash).ok_or(ArchiveError::UnknownBlock)?.root {
				removed.entry(root.0).or_default().push(hash);
			}
			batch.delete.push(state_key(&hash));
		}
		batch.insert.push((oldest_key(), oldest.encode()));
		self.remove_occurrences(&mut batch, removed)?;
		match pruned_state {
			Some(hash) => batch.insert.push((pruned_state_key(), hash.encode())),
			None => batch.delete.push(pruned_state_key()),
		}

		// Raise the floors, in the same batch: the parent of the oldest kept block is read here,
		// before its boundary goes.
		let kept = self.state_at(&oldest_hash).ok_or(ArchiveError::UnknownBlock)?;
		let before = if oldest_boundary.state_at == Some(oldest_hash) {
			match self.load::<Boundary<H>>(&boundary_key(&oldest_boundary.parent))? {
				Some(_) => {
					self.state_at(&oldest_boundary.parent).ok_or(ArchiveError::UnknownBlock)?
				},
				None => BTreeMap::new(),
			}
		} else {
			BTreeMap::new()
		};
		let at_finalized = self.state_at(&finalized).ok_or(ArchiveError::UnknownBlock)?;
		for (stream, state) in &at_finalized {
			let count = |streams: &BTreeMap<StreamId, StreamState<H>>| {
				streams.get(stream).map_or(0, |s| s.leaf_count)
			};
			let target = match stream {
				StreamId::Channel { .. } => {
					watermarks.get(stream).copied().unwrap_or(0).min(state.leaf_count)
				},
				// The head under the oldest kept root stays servable, with the frontier before it,
				// and so does every leaf a kept block sent.
				_ => {
					let head = count(&kept).saturating_sub(1);
					if oldest_boundary.state_at == Some(oldest_hash) {
						count(&before).min(head)
					} else {
						head
					}
				},
			};
			self.raise_floor(&mut batch, stream, target)?;
		}
		self.store.commit(batch).map_err(ArchiveError::Store)
	}

	/// Drop `stream`'s payloads below `target`, and its nodes below `target` except the peaks
	/// there. Floors only rise.
	fn raise_floor(
		&self,
		batch: &mut Batch,
		stream: &StreamId,
		target: u64,
	) -> Result<(), ArchiveError> {
		let floors: Floors = self.load(&floors_key(stream))?.unwrap_or_default();
		if target <= floors.retained_from {
			return Ok(());
		}
		for index in floors.payload..target {
			batch.delete.push(payload_key(stream, index));
		}
		if target > floors.node {
			// Nodes below the old floor are gone already, except its peaks.
			let keep: Vec<u64> = get_peaks(mmr_size(target));
			let candidates = get_peaks(mmr_size(floors.node))
				.into_iter()
				.chain(mmr_size(floors.node)..mmr_size(target));
			for pos in candidates {
				if !keep.contains(&pos) {
					batch.delete.push(node_key(stream, pos));
				}
			}
		}
		let floors = Floors {
			payload: floors.payload.max(target),
			node: floors.node.max(target),
			retained_from: target,
		};
		batch.insert.push((floors_key(stream), floors.encode()));
		Ok(())
	}
}
