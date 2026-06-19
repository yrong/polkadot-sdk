// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! # Speculative Outbox Pallet
//!
//! Sender-side pallet for the inclusion-based speculative messaging PoC.
//!
//! Maintains per-destination MMRs accumulating all outbound messages using a
//! peaks-only representation (O(log n) storage), stores payload bytes on-chain,
//! and exposes runtime APIs for providers to query `MessageBatch`es.
//!
//! The MMR uses `mmr_lib` with the domain-tagged `blake2_256` `SpecMerge` from
//! `cumulus-primitives-spec-messaging`; leaves are `OutgoingMessage::hash_leaf`
//! values and a destination's `subtree_root` is its peaks bagged with
//! `root_from_peaks`. The `provides` commitment is the flat `CommitmentSet` of
//! `(destination, subtree_root)` entries — there is no top-level Merkle tree.
//!
//! Implements `XcmpMessageSource` by wrapping the inner source (typically
//! `XcmpQueue`), recording outbound messages in the speculative MMR while
//! still forwarding them for standard HRMP delivery.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
use alloc::{vec, vec::Vec};

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_runtime::BoundedVec;

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::BlockNumberFor;

use cumulus_primitives_core::{ParaId, XcmpMessageSource};
use cumulus_primitives_spec_messaging::{
	mmr::{root_from_peaks, Mmr, MmrAccumulator, SpecMerge},
	LateBlockProof, MaxSpeculativeMessageLen, OutgoingMessage, SpecHasher, SubtreeExtension,
};
use polkadot_primitives::v9::{ProvidesCommitment, MAX_DESTINATIONS_PER_BLOCK};

use mmr_lib::{
	leaf_index_to_pos,
	util::{MemMMR, MemStore},
};

/// MMR state for a single destination's subtree (peaks-only representation).
#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
	/// Number of leaves inserted so far (the MMR `size`).
	pub leaf_count: u64,
	/// MMR peaks — O(log n) hashes sufficient to reconstruct the subtree root.
	pub peaks: Vec<H256>,
}

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config<RuntimeEvent: From<Event<Self>>> {
		/// The inner XCMP message source (typically `XcmpQueue`).
		type InnerXcmpMessageSource: XcmpMessageSource;
		/// This parachain's own id, bound into each message's `hash_leaf` preimage.
		type SelfParaId: Get<ParaId>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		MessagesRecorded { destination: ParaId, count: u32 },
	}

	/// Per-destination MMR state (leaf count + peaks).
	#[pallet::storage]
	pub type OutgoingMMRState<T: Config> =
		StorageMap<_, Twox64Concat, ParaId, MMRState, ValueQuery>;

	/// Historical subtree roots, peaks, and leaf counts per (block, destination).
	/// Stores `(subtree_root, peaks, leaf_count)` so late-block extension proofs can
	/// be built without a full node store. Pruned to a 256-block retention window.
	#[pallet::storage]
	pub type HistoricalSubtreeState<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		BlockNumberFor<T>,
		Twox64Concat,
		ParaId,
		(H256, Vec<H256>, u64),
	>;

	/// Payload bytes for outgoing messages.
	#[pallet::storage]
	pub type OutgoingMessages<T: Config> =
		StorageDoubleMap<_, Twox64Concat, ParaId, Twox64Concat, u64, Vec<u8>>;

	/// Destinations whose subtree root changed since the last rotation. Filled by
	/// [`Pallet::record_outbound_messages`] as messages are taken, then promoted into
	/// [`ProvidesThisBlock`] at `on_initialize` of the following block. This is the
	/// accumulator side of the delta-`provides` rotation; it is never read directly by
	/// `compute_provides`.
	#[pallet::storage]
	pub type PendingProvides<T: Config> = StorageValue<
		_,
		frame_support::BoundedBTreeSet<ParaId, sp_core::ConstU32<MAX_DESTINATIONS_PER_BLOCK>>,
		ValueQuery,
	>;

	/// Frozen snapshot of the destinations to commit in *this* block's `provides`. Rotated
	/// in from [`PendingProvides`] at `on_initialize` so it is stable for the whole block
	/// (both the on-chain `speculative_extension` read and the off-chain runtime API see the
	/// same set), while `record_outbound_messages` fills a fresh accumulator for next block.
	#[pallet::storage]
	pub type ProvidesThisBlock<T: Config> = StorageValue<
		_,
		frame_support::BoundedBTreeSet<ParaId, sp_core::ConstU32<MAX_DESTINATIONS_PER_BLOCK>>,
		ValueQuery,
	>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			// Promote the destinations recorded during the previous block's
			// `take_outbound_messages` into this block's provides snapshot, and reset the
			// accumulator. `parachain-system` reads `compute_provides` (in its `on_finalize`)
			// *before* it calls `take_outbound_messages`, so the delta we publish in block N is
			// exactly what was recorded in block N-1 — the same one-block lag the cumulative
			// implementation already had.
			ProvidesThisBlock::<T>::put(PendingProvides::<T>::take());
			T::DbWeight::get().reads_writes(1, 2)
		}

		fn on_finalize(n: BlockNumberFor<T>) {
			// Prune the retention window first.
			let retention_window: BlockNumberFor<T> = 256u32.into();
			if n > retention_window {
				let prune_at = n - retention_window;
				let _ = HistoricalSubtreeState::<T>::clear_prefix(prune_at, 100, None);
			}

			// Record the current subtree state for every active destination so a
			// late-block proof can later reconstruct an extension from any of them.
			for (dest, state) in OutgoingMMRState::<T>::iter() {
				if state.leaf_count == 0 {
					continue;
				}
				let root = root_from_peaks::<SpecHasher>(&state.peaks);
				HistoricalSubtreeState::<T>::insert(n, dest, (root, state.peaks, state.leaf_count));
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Record outbound messages in the speculative MMR.
		pub fn record_outbound_messages(dest: ParaId, payloads: Vec<Vec<u8>>) {
			let count = payloads.len() as u32;
			let mut state = OutgoingMMRState::<T>::get(&dest);
			let position_before = state.leaf_count;

			let mut mmr = Mmr::<SpecHasher>::from_parts(state.peaks, state.leaf_count);
			for payload in payloads {
				let position = mmr.size();
				let leaf = message_leaf::<T>(dest, position, &payload);
				OutgoingMessages::<T>::insert(dest, position, &payload);
				mmr.append(leaf);
			}
			let (peaks, size) = mmr.into_parts();
			state.peaks = peaks;
			state.leaf_count = size;

			OutgoingMMRState::<T>::insert(dest, state);

			// Mark this destination's root as changed so it is included in the next block's
			// delta `provides`. Bounded by `MAX_DESTINATIONS_PER_BLOCK` (the same bound as
			// `ProvidesCommitment`); a full set means more distinct destinations were touched
			// in one block than the commitment can carry, so surface it rather than drop quietly.
			PendingProvides::<T>::mutate(|set| {
				if set.try_insert(dest).is_err() {
					log::warn!(
						target: "speculative::outbox",
						"PendingProvides full ({} dests); dropping dest={:?} from provides",
						MAX_DESTINATIONS_PER_BLOCK, dest,
					);
				}
			});
			log::debug!(
				target: "speculative::outbox",
				"recorded {} message(s) to dest={:?} positions {}..{}",
				count, dest, position_before, position_before + count as u64,
			);
			Self::deposit_event(Event::MessagesRecorded { destination: dest, count });
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Compute the flat provides commitment for *this* block: the sorted
	/// `(destination, subtree_root)` set over only the destinations whose root changed in the
	/// previous block (the delta), read from the [`ProvidesThisBlock`] snapshot. Unchanged
	/// destinations are not re-committed every block — the relay retains their last root in its
	/// provides window (count-based eviction), and stale receivers bridge via a late-block proof.
	/// `None` when no destination changed.
	pub fn compute_provides() -> Option<ProvidesCommitment> {
		let entries = ProvidesThisBlock::<T>::get().into_iter().filter_map(|dest| {
			let state = OutgoingMMRState::<T>::get(&dest);
			(state.leaf_count > 0).then(|| (dest, root_from_peaks::<SpecHasher>(&state.peaks)))
		});

		let set = ProvidesCommitment::try_from_iter(entries).ok()?;
		if set.is_empty() {
			None
		} else {
			Some(set)
		}
	}

	/// Get the MMR subtree root and leaf count for a destination.
	pub fn destination_state(dest: ParaId) -> Option<(H256, u64)> {
		let state = OutgoingMMRState::<T>::get(&dest);
		if state.leaf_count == 0 {
			return None;
		}
		let root = root_from_peaks::<SpecHasher>(&state.peaks);
		Some((root, state.leaf_count))
	}

	/// Read payload bytes for a destination starting at `from_position`.
	pub fn outbound_messages(
		dest: ParaId,
		from_position: u64,
		max_messages: u32,
	) -> Vec<(u64, Vec<u8>)> {
		let leaf_count = OutgoingMMRState::<T>::get(&dest).leaf_count;
		let end = leaf_count.min(from_position + max_messages as u64);
		(from_position..end)
			.filter_map(|pos| OutgoingMessages::<T>::get(dest, pos).map(|p| (pos, p)))
			.collect()
	}

	/// Read a slice of outbound messages along with a combined MMR inclusion proof
	/// against the per-destination `subtree_root`. The receiver verifies the returned
	/// proof with `MerkleProof::<_, SpecMerge>::new(mmr_size, proof).verify(root, leaves)`.
	///
	/// Returns `None` if the destination has no messages or the requested slice is empty.
	///
	/// Cost: O(leaf_count) — replays all stored payloads into an in-memory MMR to produce
	/// the proof. Acceptable for the PoC; production would want incremental MMR storage.
	pub fn outbound_messages_with_proof(
		dest: ParaId,
		from_position: u64,
		max_messages: u32,
	) -> Option<(Vec<(u64, Vec<u8>)>, u64, Vec<H256>)> {
		let leaf_count = OutgoingMMRState::<T>::get(&dest).leaf_count;
		if leaf_count == 0 {
			return None;
		}
		let end = leaf_count.min(from_position.saturating_add(max_messages as u64));
		if end <= from_position {
			return None;
		}

		// Replay every stored payload through a MemMMR to derive node positions
		// and the gen_proof witness items for the requested slice.
		let store = MemStore::<H256>::default();
		let mut mmr = MemMMR::<H256, SpecMerge<SpecHasher>>::new(0, &store);
		let mut messages: Vec<(u64, Vec<u8>)> = Vec::new();
		for leaf_idx in 0..leaf_count {
			let payload = OutgoingMessages::<T>::get(dest, leaf_idx)?;
			mmr.push(message_leaf::<T>(dest, leaf_idx, &payload)).ok()?;
			if leaf_idx >= from_position && leaf_idx < end {
				messages.push((leaf_idx, payload));
			}
		}
		if messages.is_empty() {
			return None;
		}

		let positions: Vec<u64> = (from_position..end).map(leaf_index_to_pos).collect();
		let mmr_size = mmr.mmr_size();
		let proof = mmr.gen_proof(positions).ok()?;
		Some((messages, mmr_size, proof.proof_items().to_vec()))
	}

	/// Generate a late block proof for a receiver `dest` that built against an older
	/// per-destination subtree root. The proof shows that the source's subtree for
	/// `dest` was only appended to between `old_subtree_root` and the current root.
	pub fn generate_late_block_proof(
		dest: ParaId,
		old_subtree_root: H256,
	) -> Option<LateBlockProof> {
		// Find the historical leaf count when `dest`'s subtree root was `old_subtree_root`.
		let (_, _old_peaks, old_leaf_count) = historical_state_for::<T>(dest, old_subtree_root)?;

		let current = OutgoingMMRState::<T>::get(&dest);
		let new_subtree_root = root_from_peaks::<SpecHasher>(&current.peaks);

		let subtree_extension = if current.leaf_count > old_leaf_count {
			// Rebuild the full subtree MMR and prove the appended leaves against it.
			let store = MemStore::<H256>::default();
			let mut mmr = MemMMR::<H256, SpecMerge<SpecHasher>>::new(0, &store);
			let mut incremental: Vec<H256> = Vec::new();
			for pos in 0..current.leaf_count {
				let payload = OutgoingMessages::<T>::get(dest, pos)?;
				let leaf = message_leaf::<T>(dest, pos, &payload);
				mmr.push(leaf).ok()?;
				if pos >= old_leaf_count {
					incremental.push(leaf);
				}
			}
			let positions: Vec<u64> =
				(old_leaf_count..current.leaf_count).map(leaf_index_to_pos).collect();
			let proof = mmr.gen_proof(positions).ok()?;
			Some(SubtreeExtension {
				new_mmr_size: mmr.mmr_size(),
				proof: proof.proof_items().to_vec(),
				incremental,
			})
		} else {
			None
		};

		// `source` is *this* (the sender) para id — it is matched against the
		// receiver's `requires[source]` entry in `apply_messaging_proofs`.
		Some(LateBlockProof {
			source: T::SelfParaId::get(),
			old_subtree_root,
			new_subtree_root,
			subtree_extension,
		})
	}

	/// Find the block number at which `dest`'s subtree root was `subtree_root`.
	pub fn block_number_for_subtree_root(
		dest: ParaId,
		subtree_root: H256,
	) -> Option<BlockNumberFor<T>> {
		historical_state_for::<T>(dest, subtree_root).map(|(bn, _, _)| bn)
	}
}

impl<T: Config> XcmpMessageSource for Pallet<T> {
	fn take_outbound_messages(
		maximum_channels: usize,
		excluded_recipients: &[ParaId],
	) -> Vec<(ParaId, Vec<u8>)> {
		let messages = T::InnerXcmpMessageSource::take_outbound_messages(
			maximum_channels,
			excluded_recipients,
		);
		for (dest, data) in &messages {
			Pallet::<T>::record_outbound_messages(*dest, vec![data.clone()]);
		}
		// Messages are delivered exclusively via the speculative pathway.
		// Returning them here would also put them into horizontal_messages,
		// causing double processing on the receiver side.
		Vec::new()
	}
}

// ── Helpers ──

/// Hash a message into its MMR leaf, binding `(source, destination, position)` via
/// `OutgoingMessage::hash_leaf` (`blake2_256`, domain-tagged).
fn message_leaf<T: Config>(dest: ParaId, position: u64, payload: &[u8]) -> H256 {
	let bounded: BoundedVec<u8, MaxSpeculativeMessageLen> =
		BoundedVec::try_from(payload.to_vec()).unwrap_or_default();
	OutgoingMessage::new(T::SelfParaId::get(), dest, position, bounded).hash_leaf::<SpecHasher>()
}

/// Find the `(block_number, peaks, leaf_count)` historical entry whose subtree root
/// for `dest` equals `subtree_root`.
fn historical_state_for<T: Config>(
	dest: ParaId,
	subtree_root: H256,
) -> Option<(BlockNumberFor<T>, Vec<H256>, u64)> {
	HistoricalSubtreeState::<T>::iter().find_map(|(bn, d, (root, peaks, leaf_count))| {
		(d == dest && root == subtree_root).then_some((bn, peaks, leaf_count))
	})
}

#[cfg(test)]
mod mock;

#[cfg(test)]
mod rotation_tests {
	use crate::{
		mock::{new_test_ext, SpeculativeOutbox, Test},
		OutgoingMMRState, PendingProvides, ProvidesThisBlock,
	};
	use cumulus_primitives_core::ParaId;
	use frame_support::traits::OnInitialize;

	const DEST_A: u32 = 3000;
	const DEST_B: u32 = 4000;

	/// Advance to the next block by running the pallet's `on_initialize`, which performs the
	/// `PendingProvides` -> `ProvidesThisBlock` rotation.
	fn rotate_into(block: u64) {
		SpeculativeOutbox::on_initialize(block);
	}

	#[test]
	fn delta_provides_appears_next_block_then_clears() {
		new_test_ext().execute_with(|| {
			// Block 1: record a message to DEST_A. It is accumulated in `PendingProvides`
			// but not yet visible in `provides` (nothing has been rotated in).
			rotate_into(1);
			SpeculativeOutbox::record_outbound_messages(ParaId::from(DEST_A), vec![b"a".to_vec()]);
			assert!(PendingProvides::<Test>::get().contains(&ParaId::from(DEST_A)));
			assert!(SpeculativeOutbox::compute_provides().is_none());

			// Block 2 (N+1): rotation promotes DEST_A into the snapshot, so it shows up in
			// `provides` exactly once, with the destination's current subtree root.
			rotate_into(2);
			assert!(ProvidesThisBlock::<Test>::get().contains(&ParaId::from(DEST_A)));
			assert!(PendingProvides::<Test>::get().is_empty());
			let provides = SpeculativeOutbox::compute_provides().expect("delta has one entry");
			assert_eq!(provides.len(), 1);
			let (expected_root, _) =
				SpeculativeOutbox::destination_state(ParaId::from(DEST_A)).unwrap();
			assert_eq!(provides.get(ParaId::from(DEST_A)), Some(&expected_root));

			// Block 3 (N+2): no new records, so the destination is no longer re-committed —
			// `provides` is empty even though the MMR state still exists.
			rotate_into(3);
			assert!(SpeculativeOutbox::compute_provides().is_none());
			assert!(OutgoingMMRState::<Test>::get(ParaId::from(DEST_A)).leaf_count > 0);
		});
	}

	#[test]
	fn delta_dedups_and_batches_destinations_per_block() {
		new_test_ext().execute_with(|| {
			rotate_into(1);
			// Two messages to DEST_A and one to DEST_B in the same block.
			SpeculativeOutbox::record_outbound_messages(
				ParaId::from(DEST_A),
				vec![b"a1".to_vec(), b"a2".to_vec()],
			);
			SpeculativeOutbox::record_outbound_messages(ParaId::from(DEST_A), vec![b"a3".to_vec()]);
			SpeculativeOutbox::record_outbound_messages(ParaId::from(DEST_B), vec![b"b1".to_vec()]);

			// DEST_A appears once despite two record calls (set dedups).
			assert_eq!(PendingProvides::<Test>::get().len(), 2);

			rotate_into(2);
			let provides = SpeculativeOutbox::compute_provides().expect("two entries");
			assert_eq!(provides.len(), 2);
			assert!(provides.get(ParaId::from(DEST_A)).is_some());
			assert!(provides.get(ParaId::from(DEST_B)).is_some());
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn leaf(source: u32, dest: u32, position: u64, payload: &[u8]) -> H256 {
		let bounded = BoundedVec::try_from(payload.to_vec()).unwrap();
		OutgoingMessage::new(source.into(), dest.into(), position, bounded)
			.hash_leaf::<SpecHasher>()
	}

	#[test]
	fn peaks_root_matches_mmr_lib() {
		let leaves = [leaf(1, 2, 0, b"msg1"), leaf(1, 2, 1, b"msg2"), leaf(1, 2, 2, b"msg3")];

		// Build using the peaks-only accumulator (same as the pallet).
		let mut acc = Mmr::<SpecHasher>::new();
		for l in &leaves {
			acc.append(*l);
		}

		// Reference: mmr_lib in-memory MMR with the same SpecMerge.
		let store = MemStore::<H256>::default();
		let mut mmr = MemMMR::<H256, SpecMerge<SpecHasher>>::new(0, &store);
		for l in &leaves {
			mmr.push(*l).unwrap();
		}

		assert_eq!(acc.root(), mmr.get_root().unwrap());
	}
}
