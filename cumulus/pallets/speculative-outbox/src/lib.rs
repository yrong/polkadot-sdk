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
//! Maintains per-destination MMRs accumulating all outbound messages, stores
//! payload bytes on-chain, and exposes runtime APIs for providers to query
//! `MessageBatch`es.
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

use cumulus_primitives_core::{ParaId, XcmpMessageSource};
use polkadot_primitives::v10::{ProvidesCommitment, MMRExtensionProof};

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

/// MMR state for a single destination's subtree.
#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
	pub size: u64,
}

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The inner XCMP message source (typically `XcmpQueue`).
		type InnerXcmpMessageSource: XcmpMessageSource;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		MessagesRecorded { destination: ParaId, count: u32 },
	}

	/// Per-destination MMR state (current size).
	#[pallet::storage]
	pub type OutgoingMMRState<T: Config> =
		StorageMap<_, Twox64Concat, ParaId, MMRState, ValueQuery>;

	/// MMR nodes for per-destination subtrees.
	#[pallet::storage]
	pub type MMRNodes<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		ParaId,
		Twox64Concat,
		u64,
		H256,
	>;

	/// Historical provides roots for late block proof generation.
	#[pallet::storage]
	pub type HistoricalProvidesRoots<T: Config> =
		StorageMap<_, Twox64Concat, BlockNumberFor<T>, H256>;

	/// Historical subtree roots and sizes per destination.
	#[pallet::storage]
	pub type HistoricalSubtreeState<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		BlockNumberFor<T>,
		Twox64Concat,
		ParaId,
		(H256, u64),
	>;

	/// Payload bytes for outgoing messages.
	#[pallet::storage]
	pub type OutgoingMessages<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		ParaId,
		Twox64Concat,
		u64,
		Vec<u8>,
	>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_finalize(n: BlockNumberFor<T>) {
			if let Some(provides) = Self::compute_provides_root() {
				HistoricalProvidesRoots::<T>::insert(n, provides.root);
				
				// Record current subtree state for all active destinations.
				for (dest, state) in OutgoingMMRState::<T>::iter() {
					let root = compute_mmr_root_from_storage::<T>(dest, state.size);
					HistoricalSubtreeState::<T>::insert(n, dest, (root, state.size));
				}
			}

			// Simple pruning: keep only the last 256 blocks of history.
			let retention_window = 256u32.into();
			if n > retention_window {
				let prune_at = n - retention_window;
				HistoricalProvidesRoots::<T>::remove(prune_at);
				let _ = HistoricalSubtreeState::<T>::clear_prefix(prune_at, 100, None);
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Record outbound messages in the speculative MMR.
		pub fn record_outbound_messages(
			dest: ParaId,
			payloads: Vec<Vec<u8>>,
		) {
			let count = payloads.len() as u32;
			let mut state = OutgoingMMRState::<T>::get(&dest);
			
			let mut mmr = mmr_lib::MMR::<H256, Keccak256Merge, _>::new(
				state.size,
				DestMMRStore::<T>::new(dest),
			);

			for payload in payloads {
				let pos = state.size; // This is actually the leaf index, but for payloads we use it as pos.
				// Wait, mmr_lib uses node positions, not leaf indices.
				// For OutgoingMessages, we'll use the leaf index.
				let leaf_idx = mmr_lib::helper::pos_to_leaf_index(mmr.mmr_size());
				OutgoingMessages::<T>::insert(dest, leaf_idx, &payload);
				mmr.push(Keccak256::hash(&payload)).expect("MMR push failed");
			}

			state.size = mmr.mmr_size();
			OutgoingMMRState::<T>::insert(dest, state);

			Self::deposit_event(Event::MessagesRecorded {
				destination: dest,
				count,
			});
		}
	}
}

/// Helper to wrap runtime storage as an mmr_lib store.
struct DestMMRStore<T>(ParaId, core::marker::PhantomData<T>);
impl<T> DestMMRStore<T> {
	fn new(dest: ParaId) -> Self {
		Self(dest, core::marker::PhantomData)
	}
}

impl<T: Config> mmr_lib::MMRStoreReadOps<H256> for DestMMRStore<T> {
	fn get_elem(&self, pos: u64) -> mmr_lib::Result<Option<H256>> {
		Ok(MMRNodes::<T>::get(self.0, pos))
	}
}

impl<T: Config> mmr_lib::MMRStoreWriteOps<H256> for DestMMRStore<T> {
	fn append(&mut self, _pos: u64, elems: Vec<H256>) -> mmr_lib::Result<()> {
		// Note: _pos is the starting position of elems in the MMR.
		// mmr_lib handles the position mapping.
		let mut current_pos = _pos;
		for elem in elems {
			MMRNodes::<T>::insert(self.0, current_pos, elem);
			current_pos += 1;
		}
		Ok(())
	}
}

impl<T: Config> Pallet<T> {
	/// Compute the cumulative provides root over all per-destination MMRs.
	pub fn compute_provides_root() -> Option<ProvidesCommitment> {
		let mut roots: Vec<(ParaId, H256)> = OutgoingMMRState::<T>::iter()
			.map(|(dest, state)| (dest, compute_mmr_root_from_storage::<T>(dest, state.size)))
			.collect();

		if roots.is_empty() {
			return None;
		}

		roots.sort_by_key(|(id, _)| *id);
		let leaves: Vec<Vec<u8>> = roots
			.iter()
			.map(|(dest, root)| (dest, root).encode())
			.collect();

		Some(ProvidesCommitment {
			root: binary_merkle_tree::merkle_root::<Keccak256, _>(leaves),
		})
	}

	/// Get the MMR subtree root and leaf count for a destination.
	pub fn destination_state(dest: ParaId) -> Option<(H256, u64)> {
		let state = OutgoingMMRState::<T>::get(&dest);
		if state.size == 0 {
			return None;
		}
		Some((compute_mmr_root_from_storage::<T>(dest, state.size), state.size))
	}

	/// Read payload bytes for a destination starting at `from_position`.
	pub fn outbound_messages(
		dest: ParaId,
		from_position: u64,
		max_messages: u32,
	) -> Vec<(u64, Vec<u8>)> {
		let state = OutgoingMMRState::<T>::get(&dest);
		let leaf_count = mmr_lib::helper::pos_to_leaf_index(state.size);
		let end = leaf_count.min(from_position + max_messages as u64);

		(from_position..end)
			.filter_map(|pos| {
				OutgoingMessages::<T>::get(dest, pos)
					.map(|payload| (pos, payload))
			})
			.collect()
	}

	/// Generate a Merkle inclusion proof that `(dest, subtree_root)` is in
	/// the top-level provides root.
	pub fn subtree_inclusion_proof(
		dest: ParaId,
		_subtree_root: H256,
	) -> Option<(Vec<H256>, u32, u32)> {
		let mut roots: Vec<(ParaId, H256)> = OutgoingMMRState::<T>::iter()
			.map(|(d, state)| (d, compute_mmr_root_from_storage::<T>(d, state.size)))
			.collect();

		if roots.is_empty() {
			return None;
		}

		roots.sort_by_key(|(id, _)| *id);
		let leaf_index = roots.iter().position(|(d, _)| *d == dest)?;

		let leaves: Vec<Vec<u8>> = roots
			.iter()
			.map(|(d, r)| (d, r).encode())
			.collect();

		let number_of_leaves = leaves.len() as u32;
		let proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(
			leaves,
			leaf_index as u32,
		);

		Some((proof.proof, number_of_leaves, leaf_index as u32))
	}

	/// Generate a late block proof for a receiver that built against an older root.
	pub fn generate_late_block_proof(
		dest: ParaId,
		old_provides_root: H256,
	) -> Option<polkadot_primitives::v10::LateBlockProof> {
		// 1. Find block number for old_provides_root
		let (old_block_number, _) = HistoricalProvidesRoots::<T>::iter()
			.find(|(_, root)| root == &old_provides_root)?;
			
		// 2. Get historical subtree state
		let (old_subtree_root, old_subtree_size) = HistoricalSubtreeState::<T>::get(old_block_number, dest)?;

		let current_provides = Self::compute_provides_root()?;
		let (current_subtree_root, current_subtree_size) = Self::destination_state(dest)?;
		let (current_subtree_proof, num_dest, leaf_idx) =
			Self::subtree_inclusion_proof(dest, current_subtree_root)?;

		// 3. Generate old subtree proof (inclusion in old_provides_root)
		let mut old_roots: Vec<(ParaId, H256)> = HistoricalSubtreeState::<T>::iter_prefix(old_block_number)
			.collect();
		old_roots.sort_by_key(|(id, _)| *id);
		let old_leaf_idx = old_roots.iter().position(|(id, _)| *id == dest)?;
		let old_leaves: Vec<Vec<u8>> = old_roots.iter().map(|(d, r)| (d, r).encode()).collect();
		let old_proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(old_leaves, old_leaf_idx as u32);

		// 4. Generate MMR extension proof if size increased
		let subtree_extension = if current_subtree_size > old_subtree_size {
			Self::mmr_extension_proof(dest, old_subtree_root, old_subtree_size)
		} else {
			None
		};

		Some(polkadot_primitives::v10::LateBlockProof {
			source: 0u32.into(), // Caller will fill
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

	/// Generate an MMR extension proof.
	pub fn mmr_extension_proof(
		dest: ParaId,
		_old_subtree_root: H256,
		old_subtree_size: u64,
	) -> Option<MMRExtensionProof> {
		let current_size = OutgoingMMRState::<T>::get(&dest).size;
		if current_size <= old_subtree_size {
			return None;
		}

		// Use mmr-lib's helper to get peaks.
		let old_peaks = mmr_lib::helper::get_peaks(old_subtree_size);
		let mut old_peak_hashes = Vec::new();
		for pos in old_peaks {
			if let Some(hash) = MMRNodes::<T>::get(dest, pos) {
				old_peak_hashes.push(hash);
			}
		}

		let current_peaks = mmr_lib::helper::get_peaks(current_size);
		let mut current_peak_hashes = Vec::new();
		for pos in current_peaks {
			if let Some(hash) = MMRNodes::<T>::get(dest, pos) {
				current_peak_hashes.push(hash);
			}
		}

		Some(MMRExtensionProof {
			old_peaks: old_peak_hashes,
			new_peaks: current_peak_hashes,
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

		messages
	}
}

// ── Helpers ──

/// Compute the MMR root from an ordered list of leaf hashes.
///
/// Identical to the receiver's `compute_mmr_root` in `pallet-speculative-inbox`.
fn compute_mmr_root(leaves: &[H256]) -> H256 {
	if leaves.is_empty() {
		return H256::zero();
	}
	let store = mmr_lib::util::MemStore::<H256>::default();
	let mut mmr =
		mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
	for leaf in leaves {
		let _ = mmr.push(*leaf);
	}
	mmr.get_root().unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use super::*;
	use mmr_lib::helper::leaf_index_to_pos;
	use sp_mmr_primitives::utils::NodesUtils;

	#[test]
	fn test_mmr_root_matches_inbox_pattern() {
		let leaf1 = Keccak256::hash(b"msg1");
		let leaf2 = Keccak256::hash(b"msg2");
		let leaf3 = Keccak256::hash(b"msg3");

		let root = compute_mmr_root(&[leaf1, leaf2, leaf3]);
		assert_ne!(root, H256::zero());

		// Verify against MerkleProof::calculate_root (same as inbox).
		let leaf_count: u64 = 3;
		let mmr_node_size = NodesUtils::new(leaf_count).size();
		let leaf_pos = leaf_index_to_pos(2);

		let store = mmr_lib::util::MemStore::<H256>::default();
		let mut mmr =
			mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
		mmr.push(leaf1).unwrap();
		mmr.push(leaf2).unwrap();
		let pos2 = mmr.push(leaf3).unwrap();
		let proof = mmr.gen_proof(vec![pos2]).unwrap();

		let mp = mmr_lib::MerkleProof::<H256, Keccak256Merge>::new(
			mmr_node_size,
			proof.proof_items().to_vec(),
		);
		let calculated =
			mp.calculate_root(vec![(leaf_pos, leaf3)]).unwrap();

		assert_eq!(calculated, root);
	}

	#[test]
	fn test_top_level_proof_generation_verification_roundtrip() {
		// Provider-generated proof → receiver verification
		let dest_a: ParaId = 1000u32.into();
		let dest_b: ParaId = 2000u32.into();
		let subtree_a = Keccak256::hash(b"msgs_to_a");
		let subtree_b = Keccak256::hash(b"msgs_to_b");

		let mut pairs = vec![(dest_a, subtree_a), (dest_b, subtree_b)];
		pairs.sort_by_key(|(id, _)| *id);
		let leaves: Vec<Vec<u8>> =
			pairs.iter().map(|(d, r)| (d, r).encode()).collect();
		let number_of_leaves = leaves.len() as u32;

		let provides_root =
			binary_merkle_tree::merkle_root::<Keccak256, _>(&leaves);

		let leaf_index = pairs.iter().position(|(d, _)| *d == dest_a).unwrap();
		let proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(
			leaves,
			leaf_index as u32,
		);

		// Receiver-side verification (same as pallet-speculative-inbox)
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
