// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Runtime API implementation for XCMP MMD outbox.

use crate::pallet::{Config, Keccak256Merge, MmrLeafCount, MmrRootHash, OutboxLeaves, Pallet};
use alloc::vec::Vec;
use codec::Encode;
use cumulus_primitives_xcmp_mmd::OutboxLeaf;
use mmr_lib::util::MemStore;
use sp_core::H256;
use sp_runtime::traits::Hash;

impl<T: Config> Pallet<T> {
	/// Generate a proof for the outbox leaf at the given index.
	pub fn generate_proof(leaf_index: u64) -> Option<(OutboxLeaf, Vec<H256>, u64)> {
		// Get the leaf
		let leaf = OutboxLeaves::<T>::get(leaf_index)?;

		// Get current MMR size
		let mmr_size = MmrLeafCount::<T>::get();

		// If the requested leaf is beyond current size, return None
		if leaf_index >= mmr_size {
			return None;
		}

		// Rebuild MMR from all stored leaves
		let store = MemStore::default();
		let mut mmr = mmr_lib::MMR::<_, Keccak256Merge, _>::new(0, &store);

		for i in 0..mmr_size {
			if let Some(stored_leaf) = OutboxLeaves::<T>::get(i) {
				let stored_hash = sp_runtime::traits::Keccak256::hash(&stored_leaf.encode());
				let _ = mmr.push(stored_hash);
			}
		}

		// Generate proof for the requested leaf
		let proof_result = mmr.gen_proof(alloc::vec![leaf_index]);

		match proof_result {
			Ok(merkle_proof) => {
				// Extract proof items (sibling hashes)
				// The proof_items() returns a slice of H256
				let proof_items: Vec<H256> = merkle_proof.proof_items().to_vec();
				Some((leaf, proof_items, mmr_size))
			},
			Err(_) => None,
		}
	}

	/// Get the current MMR root hash.
	pub fn get_mmr_root() -> H256 {
		MmrRootHash::<T>::get()
	}

	/// Get the current MMR leaf count.
	pub fn get_mmr_leaf_count() -> u64 {
		MmrLeafCount::<T>::get()
	}
}
