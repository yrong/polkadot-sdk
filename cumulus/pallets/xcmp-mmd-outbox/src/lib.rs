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

//! # XCMP MMD Outbox Pallet
//!
//! This pallet wraps the outbound XCMP message source and commits each message to an
//! append-only MMR. After all messages are processed, it deposits a digest item in the
//! block header containing the MMR root.
//!
//! ## Overview
//!
//! The pallet acts as a wrapper around `XcmpMessageSource` (typically `XcmpQueue`).
//! For each outbound message `(dest, payload)`:
//! - Computes `payload_hash = Keccak256(payload)`
//! - Appends leaf `(dest, payload_hash)` to the outbox MMR
//! - Increments the global `MmrLeafCount`
//!
//! In `on_finalize`, it:
//! - Computes the MMR root from stored nodes
//! - Encodes `XcmpMmdDigest { version: 0, root }`
//! - Deposits `DigestItem::PreRuntime(*b"xmmd", ...)`
//!
//! ## MMR Implementation
//!
//! This pallet manages its own MMR using `polkadot-ckb-merkle-mountain-range` directly,
//! rather than depending on `pallet-mmr`. This is because `pallet-mmr` expects one leaf
//! per block, but we need multiple leaves per block (one per XCMP message).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use pallet::*;

pub mod runtime_api_impl;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use alloc::vec::Vec;
	use codec::Encode;
	use cumulus_primitives_xcmp_mmd::{OutboxLeaf, XcmpMmdDigest};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use mmr_lib::{util::MemStore, Merge, Result as MmrResult};
	use polkadot_parachain_primitives::primitives::Id as ParaId;
	use sp_runtime::{traits::Hash, DigestItem};

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The wrapped outbound XCMP message source (typically `XcmpQueue`).
		type OutboundXcmpMessageSource: cumulus_primitives_core::XcmpMessageSource;

		/// Maximum number of pending outbox leaves per block.
		#[pallet::constant]
		type MaxPendingOutboxLeaves: Get<u32>;
	}

	/// Global monotonic counter for MMR leaf indices.
	/// This serves as the unique identifier/"nonce" for each outbound message.
	#[pallet::storage]
	pub type MmrLeafCount<T: Config> = StorageValue<_, u64, ValueQuery>;

	/// Historical outbox leaves stored for proof generation.
	/// Maps leaf_index -> OutboxLeaf.
	///
	/// Note: For POC, we store all leaves. Production would need pruning.
	#[pallet::storage]
	pub type OutboxLeaves<T: Config> = StorageMap<_, Twox64Concat, u64, OutboxLeaf>;

	/// Current MMR root hash.
	#[pallet::storage]
	pub type MmrRootHash<T: Config> = StorageValue<_, sp_core::H256, ValueQuery>;

	/// Merge implementation for Keccak256 hashing.
	pub struct Keccak256Merge;

	impl Merge for Keccak256Merge {
		type Item = sp_core::H256;

		fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
			let mut concat = [0u8; 64];
			concat[..32].copy_from_slice(lhs.as_ref());
			concat[32..].copy_from_slice(rhs.as_ref());
			Ok(sp_runtime::traits::Keccak256::hash(&concat))
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_finalize(_n: BlockNumberFor<T>) {
			// Get the current MMR root
			let mmr_root = MmrRootHash::<T>::get();

			// Encode the digest
			let digest = XcmpMmdDigest { version: 0, root: mmr_root };
			let digest_data = digest.encode();

			// Deposit as PreRuntime digest item with engine ID "xmmd"
			let digest_item = DigestItem::PreRuntime(*b"xmmd", digest_data);
			frame_system::Pallet::<T>::deposit_log(digest_item);
		}
	}

	impl<T: Config> Pallet<T> {
		/// Process a single outbound message: hash the payload and append to MMR.
		pub fn note_outbound(dest: ParaId, payload: &[u8]) {
			// Compute payload hash using Keccak256
			let payload_hash = sp_runtime::traits::Keccak256::hash(payload);

			// Create the outbox leaf
			let leaf = OutboxLeaf { dest: dest.into(), payload_hash };

			// Get current leaf count
			let leaf_index = MmrLeafCount::<T>::get();

			// Store the leaf for historical reference
			OutboxLeaves::<T>::insert(leaf_index, leaf.clone());

			// Hash the leaf for MMR
			let leaf_hash = sp_runtime::traits::Keccak256::hash(&leaf.encode());

			// Append to MMR
			Self::mmr_push(leaf_hash);

			// Increment leaf count
			MmrLeafCount::<T>::put(leaf_index.saturating_add(1));

			log::debug!(
				target: "xcmp-mmd-outbox",
				"Noted outbound message to {:?}, leaf_index: {}, payload_hash: {:?}",
				dest,
				leaf_index,
				payload_hash
			);
		}

		/// Append a leaf hash to the MMR and update the root.
		fn mmr_push(leaf_hash: sp_core::H256) {
			let leaf_count = MmrLeafCount::<T>::get();

			// Create a memory store and MMR
			let store = MemStore::default();
			let mut mmr = mmr_lib::MMR::<_, Keccak256Merge, _>::new(0, &store);

			// Rebuild MMR from all stored leaves
			for i in 0..leaf_count {
				if let Some(stored_leaf) = OutboxLeaves::<T>::get(i) {
					let stored_hash = sp_runtime::traits::Keccak256::hash(&stored_leaf.encode());
					let _ = mmr.push(stored_hash);
				}
			}

			// Push the new leaf
			let _ = mmr.push(leaf_hash);

			// Get and store the new root
			if let Ok(root) = mmr.get_root() {
				MmrRootHash::<T>::put(root);
			}
		}
	}

	/// Wrapper implementation of `XcmpMessageSource` that observes messages.
	impl<T: Config> cumulus_primitives_core::XcmpMessageSource for Pallet<T> {
		fn take_outbound_messages(
			maximum_channels: usize,
			excluded_recipients: &[ParaId],
		) -> Vec<(ParaId, Vec<u8>)> {
			// Drain messages from the wrapped source
			let messages = T::OutboundXcmpMessageSource::take_outbound_messages(
				maximum_channels,
				excluded_recipients,
			);

			// Process each message
			for (dest, payload) in messages.iter() {
				Self::note_outbound(*dest, payload);
			}

			messages
		}
	}
}
