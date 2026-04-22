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

//! Types for XCMP MMD inbox pallet.

use codec::{Decode, DecodeWithMemTracking, Encode};
use cumulus_primitives_xcmp_mmd::OutboxLeaf;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use scale_info::TypeInfo;
use sp_core::H256;
use sp_std::vec::Vec;

/// A cross-chain message with all proofs needed for verification.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct MessageWithProof {
	/// Source parachain ID
	pub source: ParaId,
	/// Destination parachain ID
	pub dest: ParaId,
	/// Index of the message in the source outbox MMR
	pub mmr_leaf_index: u64,
	/// Index of the relay chain block in the relay MMR
	pub relay_mmr_leaf_index: u64,
	/// The actual message payload
	pub payload: Vec<u8>,
	/// Proof for the relay MMR leaf (contains ParaHeadsRoot)
	pub relay_mmr_proof: Vec<H256>,
	/// The relay MMR leaf data (BEEFY MMR leaf, needed for verification)
	pub relay_mmr_leaf: Vec<u8>,
	/// The relay MMR size at proof generation time
	pub relay_mmr_size: u64,
	/// Proof for the source parachain head in the ParaHeadsRoot
	pub para_heads_proof: Vec<H256>,
	/// The outbox leaf data (needed for MMR verification)
	pub outbox_leaf: OutboxLeaf,
	/// Proof for the message in the source outbox MMR
	pub outbox_mmr_proof: Vec<H256>,
	/// MMR size at the time of proof generation (needed for verification)
	pub outbox_mmr_size: u64,
}

/// Unbounded version for easier construction (e.g., in tests or relayer)
#[derive(Clone, Encode, Decode, PartialEq, Eq, Debug, TypeInfo)]
pub struct MessageWithProofUnbounded {
	pub source: ParaId,
	pub dest: ParaId,
	pub mmr_leaf_index: u64,
	pub relay_mmr_leaf_index: u64,
	pub payload: Vec<u8>,
	pub relay_mmr_proof: Vec<H256>,
	pub relay_mmr_leaf: Vec<u8>,
	pub relay_mmr_size: u64,
	pub para_heads_proof: Vec<H256>,
	pub outbox_leaf: OutboxLeaf,
	pub outbox_mmr_proof: Vec<H256>,
	pub outbox_mmr_size: u64,
}
