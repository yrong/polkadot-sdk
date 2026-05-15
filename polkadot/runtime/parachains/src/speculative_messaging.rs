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

//! Speculative Messaging — relay-chain enactment module (Phase 1 PoC).
//!
//! Stores the latest `ProvidesRoots` per parachain and provides helpers for
//! enactment-time dependency satisfaction checks.
//!
//! ## What the relay chain does (and does NOT do)
//!
//! - **Stores** one `Hash` per parachain (the latest provides root), overwritten
//!   each time a candidate with a provides commitment is enacted.
//! - **Checks** at enactment time that every `RequiresCommitment.expected_root`
//!   matches the persisted `ProvidesRoots[source]`. This is a hash-equality
//!   check, not a cryptographic verification.
//! - **Does NOT** verify MMR proofs, store message payloads, or keep root
//!   history. All cryptographic work lives in the PVF.

use frame_support::pallet_prelude::*;

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {}

	/// Latest provides root per parachain.
	///
	/// Updated each time a candidate with a provides commitment is enacted.
	#[pallet::storage]
	pub type ProvidesRoots<T: Config> =
		StorageMap<_, Twox64Concat, polkadot_primitives::Id, polkadot_primitives::Hash>;

	/// Speculative messaging data for candidates in `PendingAvailability`.
	///
	/// Populated during `process_candidates` from the V4 candidate's commitments,
	/// read during enactment for requires matching and provides root updates.
	/// Keyed by `CandidateHash`.
	#[pallet::storage]
	pub type PendingSpeculativeData<T: Config> = StorageMap<
		_,
		Twox64Concat,
		polkadot_primitives::CandidateHash,
		(polkadot_primitives::Hash, Vec<(polkadot_primitives::Id, polkadot_primitives::Hash)>),
	>;

	impl<T: Config> Pallet<T> {
		/// Read the latest provides root for a parachain.
		pub fn provides_root(
			para_id: &polkadot_primitives::Id,
		) -> Option<polkadot_primitives::Hash> {
			ProvidesRoots::<T>::get(para_id)
		}

		/// Update the provides root after a candidate is enacted.
		pub fn update_provides_root(
			para_id: polkadot_primitives::Id,
			root: polkadot_primitives::Hash,
		) {
			ProvidesRoots::<T>::insert(para_id, root);
		}

		/// Store speculative data for a candidate entering `PendingAvailability`.
		///
		/// Called from `process_candidates` when a V4 candidate is backed.
		/// The data is read during enactment for dependency satisfaction.
		pub fn store_pending_speculative(
			candidate_hash: polkadot_primitives::CandidateHash,
			provides_root: polkadot_primitives::Hash,
			requires: Vec<(polkadot_primitives::Id, polkadot_primitives::Hash)>,
		) {
			PendingSpeculativeData::<T>::insert(
				candidate_hash,
				(provides_root, requires),
			);
		}

		/// Take speculative data for a candidate being enacted, removing it from storage.
		pub fn take_pending_speculative(
			candidate_hash: &polkadot_primitives::CandidateHash,
		) -> Option<(
			polkadot_primitives::Hash,
			Vec<(polkadot_primitives::Id, polkadot_primitives::Hash)>,
		)> {
			PendingSpeculativeData::<T>::take(candidate_hash)
		}

		/// Peek at speculative data for a candidate without removing it.
		pub fn peek_pending_speculative(
			candidate_hash: &polkadot_primitives::CandidateHash,
		) -> Option<(
			polkadot_primitives::Hash,
			Vec<(polkadot_primitives::Id, polkadot_primitives::Hash)>,
		)> {
			PendingSpeculativeData::<T>::get(candidate_hash)
		}
	}
}
