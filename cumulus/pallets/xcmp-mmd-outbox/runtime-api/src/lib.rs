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

//! Runtime API for XCMP MMD outbox proof generation.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, Encode};
use cumulus_primitives_xcmp_mmd::OutboxLeaf;
use sp_core::H256;

/// Proof for a single outbox leaf.
#[derive(Clone, Encode, Decode, Debug)]
pub struct OutboxProof {
	/// The outbox leaf being proven.
	pub leaf: OutboxLeaf,
	/// MMR proof (list of sibling hashes).
	pub proof: Vec<H256>,
	/// MMR size at the time of proof generation.
	pub mmr_size: u64,
}

sp_api::decl_runtime_apis! {
	/// Runtime API for generating outbox MMR proofs.
	pub trait XcmpMmdOutboxApi {
		/// Generate a proof for the outbox leaf at the given index.
		///
		/// Returns `None` if the leaf index doesn't exist.
		fn generate_outbox_proof(leaf_index: u64) -> Option<OutboxProof>;

		/// Get the current MMR root hash.
		fn mmr_root() -> H256;

		/// Get the current MMR leaf count.
		fn mmr_leaf_count() -> u64;
	}
}
