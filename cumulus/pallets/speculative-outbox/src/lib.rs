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
//! Implements `XcmpMessageSource` by wrapping the inner source (typically
//! `XcmpQueue`), recording outbound messages in the speculative MMR while
//! still forwarding them for standard HRMP delivery.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
use alloc::{vec, vec::Vec};

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_runtime::traits::{Hash as _, Keccak256};

use frame_support::pallet_prelude::*;
use frame_system::pallet_prelude::BlockNumberFor;

use cumulus_primitives_core::{ParaId, XcmpMessageSource};
use polkadot_primitives::v10::{MMRExtensionProof, ProvidesCommitment};

use mmr_lib::{Merge, Result as MmrResult};

/// Keccak256 merge for MMR node construction.
/// Identical to the receiver-side `Keccak256Merge` in `pallet-speculative-inbox`.
struct Keccak256Merge;
impl Merge for Keccak256Merge {
	type Item = H256;
	fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
		let mut concat = [0u8; 64];
		concat[..32].copy_from_slice(lhs.as_ref());
		concat[32..].copy_from_slice(rhs.as_ref());
		Ok(Keccak256::hash(&concat))
	}
}

/// MMR state for a single destination's subtree (peaks-only representation).
#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
	/// Number of leaves inserted so far (used as `size` in append_leaf_to_peaks).
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

	/// Historical provides roots for late block proof generation. Keyed by block number.
	#[pallet::storage]
	pub type HistoricalProvidesRoots<T: Config> =
		StorageMap<_, Twox64Concat, BlockNumberFor<T>, H256>;

	/// Reverse index: provides root hash → block number that produced it.
	/// Allows O(1) lookup in `block_number_for_provides_root` / `generate_late_block_proof`.
	/// Bounded to the same 256-block retention window as `HistoricalProvidesRoots`.
	#[pallet::storage]
	pub type ProvidesRootIndex<T: Config> =
		StorageMap<_, Identity, H256, BlockNumberFor<T>>;

	/// Historical subtree roots, peaks, and leaf counts per destination.
	/// Stores (root, peaks, leaf_count) so extension proofs can be built without a full node store.
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
			// Prune BEFORE inserting so that when the same provides root persists
			// across the retention window, the prune does not delete the entry we
			// are about to write (insert-then-prune would wipe a stable root).
			let retention_window: BlockNumberFor<T> = 256u32.into();
			if n > retention_window {
				let prune_at = n - retention_window;
				if let Some(old_root) = HistoricalProvidesRoots::<T>::take(prune_at) {
					ProvidesRootIndex::<T>::remove(old_root);
				}
				let _ = HistoricalSubtreeState::<T>::clear_prefix(prune_at, 100, None);
			}

			if let Some(provides) = Self::compute_provides_root() {
				log::debug!(
					target: "speculative::outbox",
					"block {:?}: provides_root={:?}",
					n, provides.root,
				);
				HistoricalProvidesRoots::<T>::insert(n, provides.root);
				ProvidesRootIndex::<T>::insert(provides.root, n);

				// Record current subtree state for all active destinations.
				for (dest, state) in OutgoingMMRState::<T>::iter() {
					let root = bag_peaks::<Keccak256Merge>(&state.peaks).unwrap_or_default();
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
				OutgoingMessages::<T>::insert(dest, state.leaf_count, &payload);
				let leaf_hash = Keccak256::hash(&payload);
				state.peaks = append_leaf_to_peaks::<Keccak256Merge>(
					state.peaks,
					state.leaf_count,
					leaf_hash,
				);
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
	/// Compute the cumulative provides root over all per-destination MMRs.
	pub fn compute_provides_root() -> Option<ProvidesCommitment> {
		let mut roots: Vec<(ParaId, H256)> = OutgoingMMRState::<T>::iter()
			.filter(|(_, state)| state.leaf_count > 0)
			.map(|(dest, state)| {
				let root = bag_peaks::<Keccak256Merge>(&state.peaks).unwrap_or_default();
				(dest, root)
			})
			.collect();

		if roots.is_empty() {
			return None;
		}

		roots.sort_by_key(|(id, _)| *id);
		let leaves: Vec<Vec<u8>> = roots.iter().map(|(dest, root)| (dest, root).encode()).collect();

		Some(ProvidesCommitment { root: binary_merkle_tree::merkle_root::<Keccak256, _>(leaves) })
	}

	/// Get the MMR subtree root and leaf count for a destination.
	pub fn destination_state(dest: ParaId) -> Option<(H256, u64)> {
		let state = OutgoingMMRState::<T>::get(&dest);
		if state.leaf_count == 0 {
			return None;
		}
		let root = bag_peaks::<Keccak256Merge>(&state.peaks).unwrap_or_default();
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

	/// Generate a Merkle inclusion proof that `(dest, subtree_root)` is in
	/// the top-level provides root.
	pub fn subtree_inclusion_proof(
		dest: ParaId,
		_subtree_root: H256,
	) -> Option<(Vec<H256>, u32, u32)> {
		let mut roots: Vec<(ParaId, H256)> = OutgoingMMRState::<T>::iter()
			.filter(|(_, state)| state.leaf_count > 0)
			.map(|(d, state)| {
				let root = bag_peaks::<Keccak256Merge>(&state.peaks).unwrap_or_default();
				(d, root)
			})
			.collect();

		if roots.is_empty() {
			return None;
		}

		roots.sort_by_key(|(id, _)| *id);
		let leaf_index = roots.iter().position(|(d, _)| *d == dest)?;
		let leaves: Vec<Vec<u8>> = roots.iter().map(|(d, r)| (d, r).encode()).collect();
		let number_of_leaves = leaves.len() as u32;
		let proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(leaves, leaf_index as u32);

		Some((proof.proof, number_of_leaves, leaf_index as u32))
	}

	/// Generate a late block proof for a receiver that built against an older root.
	pub fn generate_late_block_proof(
		dest: ParaId,
		old_provides_root: H256,
	) -> Option<polkadot_primitives::v10::LateBlockProof> {
		// 1. Find the block number that produced old_provides_root.
		let (old_block_number, _) =
			HistoricalProvidesRoots::<T>::iter().find(|(_, root)| root == &old_provides_root)?;

		// 2. Get historical subtree state (root + peaks + leaf_count stored at that block).
		let (old_subtree_root, old_peaks, old_leaf_count) =
			HistoricalSubtreeState::<T>::get(old_block_number, dest)?;

		let current_provides = Self::compute_provides_root()?;
		let (current_subtree_root, _) = Self::destination_state(dest)?;
		let (current_subtree_proof, num_dest, leaf_idx) =
			Self::subtree_inclusion_proof(dest, current_subtree_root)?;

		// 3. Build old subtree Merkle proof from historical provides root.
		let mut old_roots: Vec<(ParaId, H256)> =
			HistoricalSubtreeState::<T>::iter_prefix(old_block_number)
				.map(|(id, (root, _, _))| (id, root))
				.collect();
		old_roots.sort_by_key(|(id, _)| *id);
		let old_leaf_idx = old_roots.iter().position(|(id, _)| *id == dest)?;
		let old_leaves: Vec<Vec<u8>> = old_roots.iter().map(|(d, r)| (d, r).encode()).collect();
		let old_proof =
			binary_merkle_tree::merkle_proof::<Keccak256, _, _>(old_leaves, old_leaf_idx as u32);

		// 4. Build MMR extension proof if the subtree has grown.
		let current_state = OutgoingMMRState::<T>::get(&dest);
		let subtree_extension = if current_state.leaf_count > old_leaf_count {
			// Collect leaf hashes for all messages appended since old_leaf_count.
			let connecting_nodes: Vec<H256> = (old_leaf_count..current_state.leaf_count)
				.filter_map(|pos| OutgoingMessages::<T>::get(dest, pos))
				.map(|payload| Keccak256::hash(&payload))
				.collect();
			Some(MMRExtensionProof {
				old_peaks: old_peaks.clone(),
				old_leaf_count,
				new_peaks: current_state.peaks.clone(),
				connecting_nodes,
			})
		} else {
			None
		};

		Some(polkadot_primitives::v10::LateBlockProof {
			source: dest,
			old_number_of_destinations: old_roots.len() as u32,
			old_leaf_index: old_leaf_idx as u32,
			number_of_destinations: num_dest,
			leaf_index: leaf_idx,
			old_provides_root,
			old_subtree_root,
			old_subtree_proof: old_proof.proof,
			new_provides_root: current_provides.root,
			new_subtree_root: current_subtree_root,
			new_subtree_proof: current_subtree_proof,
			subtree_extension,
		})
	}

	/// Find the block number that produced the given provides root.
	pub fn block_number_for_provides_root(root: H256) -> Option<BlockNumberFor<T>> {
		ProvidesRootIndex::<T>::get(root)
	}

	/// Generate an MMR extension proof from stored peaks.
	pub fn mmr_extension_proof(
		dest: ParaId,
		_old_subtree_root: H256,
		_old_subtree_size: u64,
	) -> Option<MMRExtensionProof> {
		// With peaks-only storage, the caller already has old_peaks from
		// HistoricalSubtreeState. This entry point is retained for API
		// compatibility; generate_late_block_proof builds the proof directly.
		let current_state = OutgoingMMRState::<T>::get(&dest);
		if current_state.leaf_count == 0 {
			return None;
		}
		Some(MMRExtensionProof {
			old_peaks: Vec::new(), // Caller must supply old peaks from history.
			old_leaf_count: 0,     // Caller must supply old leaf count from history.
			new_peaks: current_state.peaks,
			connecting_nodes: Vec::new(),
		})
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

/// Bag MMR peaks into a single root hash using the canonical merge order:
/// merge(right_accumulator, left_peak) from right to left.
fn bag_peaks<M: Merge<Item = H256>>(peaks: &[H256]) -> mmr_lib::Result<H256> {
	match peaks.len() {
		0 => Err(mmr_lib::Error::InconsistentStore),
		1 => Ok(peaks[0]),
		_ => {
			let mut root = *peaks.last().unwrap();
			for peak in peaks[..peaks.len() - 1].iter().rev() {
				root = M::merge(&root, peak)?;
			}
			Ok(root)
		},
	}
}

/// Append a leaf hash to the peaks list, merging peaks as required.
/// `size` is the number of leaves already in the MMR (0-based leaf count).
fn append_leaf_to_peaks<M: Merge<Item = H256>>(
	mut peaks: Vec<H256>,
	size: u64,
	leaf: H256,
) -> Vec<H256> {
	let mut current = leaf;
	let mut current_size = size;
	while current_size % 2 == 1 {
		if let Some(last_peak) = peaks.pop() {
			current = M::merge(&last_peak, &current).unwrap_or(current);
		}
		current_size /= 2;
	}
	peaks.push(current);
	peaks
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_mmr_root_matches_inbox_pattern() {
		let leaf1 = Keccak256::hash(b"msg1");
		let leaf2 = Keccak256::hash(b"msg2");
		let leaf3 = Keccak256::hash(b"msg3");

		// Build using peaks-only approach (same as inbox).
		let mut peaks = Vec::new();
		for (i, leaf) in [leaf1, leaf2, leaf3].iter().enumerate() {
			peaks = append_leaf_to_peaks::<Keccak256Merge>(peaks, i as u64, *leaf);
		}
		let peaks_root = bag_peaks::<Keccak256Merge>(&peaks).unwrap();

		// Reference: mmr_lib in-memory MMR.
		let store = mmr_lib::util::MemStore::<H256>::default();
		let mut mmr = mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
		mmr.push(leaf1).unwrap();
		mmr.push(leaf2).unwrap();
		mmr.push(leaf3).unwrap();
		let mmr_root = mmr.get_root().unwrap();

		assert_eq!(peaks_root, mmr_root);
	}

	#[test]
	fn test_top_level_proof_generation_verification_roundtrip() {
		let dest_a: ParaId = 1000u32.into();
		let dest_b: ParaId = 2000u32.into();
		let subtree_a = Keccak256::hash(b"msgs_to_a");
		let subtree_b = Keccak256::hash(b"msgs_to_b");

		let mut pairs = vec![(dest_a, subtree_a), (dest_b, subtree_b)];
		pairs.sort_by_key(|(id, _)| *id);
		let leaves: Vec<Vec<u8>> = pairs.iter().map(|(d, r)| (d, r).encode()).collect();
		let number_of_leaves = leaves.len() as u32;
		let provides_root = binary_merkle_tree::merkle_root::<Keccak256, _>(&leaves);

		let leaf_index = pairs.iter().position(|(d, _)| *d == dest_a).unwrap();
		let proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(leaves, leaf_index as u32);

		let leaf_data = (dest_a, subtree_a).encode();
		assert!(binary_merkle_tree::verify_proof::<Keccak256, _, _>(
			&provides_root,
			proof.proof,
			number_of_leaves,
			leaf_index as u32,
			&leaf_data,
		));
	}
}
