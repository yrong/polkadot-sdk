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
use cumulus_primitives_spec_messaging::mmr::{root_from_peaks, SpecMerge};
use polkadot_primitives::v9::{
	LateBlockProof, MaxSpeculativeMessageLen, OutgoingMessage, ProvidesCommitment, SubtreeExtension,
};

use mmr_lib::{
	leaf_index_to_pos,
	util::{MemMMR, MemStore},
	Merge,
};

/// MMR state for a single destination's subtree (peaks-only representation).
#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
	/// Number of leaves inserted so far (used as `size` in `append_leaf_to_peaks`).
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

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
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
				if let Some(root) = root_from_peaks(&state.peaks) {
					HistoricalSubtreeState::<T>::insert(
						n,
						dest,
						(root, state.peaks, state.leaf_count),
					);
				}
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Record outbound messages in the speculative MMR.
		pub fn record_outbound_messages(dest: ParaId, payloads: Vec<Vec<u8>>) {
			let count = payloads.len() as u32;
			let mut state = OutgoingMMRState::<T>::get(&dest);
			let position_before = state.leaf_count;

			for payload in payloads {
				let leaf_hash = message_leaf::<T>(dest, state.leaf_count, &payload);
				OutgoingMessages::<T>::insert(dest, state.leaf_count, &payload);
				state.peaks = append_leaf_to_peaks(state.peaks, state.leaf_count, leaf_hash);
				state.leaf_count += 1;
			}

			OutgoingMMRState::<T>::insert(dest, state);
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
	/// Compute the flat provides commitment: the sorted `(destination, subtree_root)`
	/// set over all per-destination MMRs. `None` when nothing has been sent.
	pub fn compute_provides() -> Option<ProvidesCommitment> {
		let entries = OutgoingMMRState::<T>::iter()
			.filter(|(_, state)| state.leaf_count > 0)
			.filter_map(|(dest, state)| root_from_peaks(&state.peaks).map(|root| (dest, root)));

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
		let root = root_from_peaks(&state.peaks)?;
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
		let mut mmr = MemMMR::<H256, SpecMerge>::new(0, &store);
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
		let new_subtree_root = root_from_peaks(&current.peaks)?;

		let subtree_extension = if current.leaf_count > old_leaf_count {
			// Rebuild the full subtree MMR and prove the appended leaves against it.
			let store = MemStore::<H256>::default();
			let mut mmr = MemMMR::<H256, SpecMerge>::new(0, &store);
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
	OutgoingMessage::new(T::SelfParaId::get(), dest, position, bounded).hash_leaf()
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

/// Append a leaf hash to the peaks list, merging equal-height peaks via `SpecMerge`
/// (matching `mmr_lib`'s internal node construction). `size` is the number of leaves
/// already in the MMR (0-based leaf count).
fn append_leaf_to_peaks(mut peaks: Vec<H256>, size: u64, leaf: H256) -> Vec<H256> {
	let mut current = leaf;
	let mut current_size = size;
	while current_size % 2 == 1 {
		if let Some(last_peak) = peaks.pop() {
			current = SpecMerge::merge(&last_peak, &current).unwrap_or(current);
		}
		current_size /= 2;
	}
	peaks.push(current);
	peaks
}

#[cfg(test)]
mod tests {
	use super::*;

	fn leaf(source: u32, dest: u32, position: u64, payload: &[u8]) -> H256 {
		let bounded = BoundedVec::try_from(payload.to_vec()).unwrap();
		OutgoingMessage::new(source.into(), dest.into(), position, bounded).hash_leaf()
	}

	#[test]
	fn peaks_root_matches_mmr_lib() {
		let l1 = leaf(1, 2, 0, b"msg1");
		let l2 = leaf(1, 2, 1, b"msg2");
		let l3 = leaf(1, 2, 2, b"msg3");

		// Build using the peaks-only approach (same as the pallet).
		let mut peaks = Vec::new();
		for (i, l) in [l1, l2, l3].iter().enumerate() {
			peaks = append_leaf_to_peaks(peaks, i as u64, *l);
		}
		let peaks_root = root_from_peaks(&peaks).unwrap();

		// Reference: mmr_lib in-memory MMR with the same SpecMerge.
		let store = MemStore::<H256>::default();
		let mut mmr = MemMMR::<H256, SpecMerge>::new(0, &store);
		mmr.push(l1).unwrap();
		mmr.push(l2).unwrap();
		mmr.push(l3).unwrap();
		let mmr_root = mmr.get_root().unwrap();

		assert_eq!(peaks_root, mmr_root);
	}
}
