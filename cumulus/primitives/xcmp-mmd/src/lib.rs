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

//! Primitives for XCMP MMD (Merkle Mountain Range based cross-chain messaging).
//!
//! This crate provides the core types and constants for the XCMP MMD minimal POC,
//! which replaces HRMP's on-chain payload storage with off-chain storage and
//! cryptographic commitments verified via nested Merkle proofs.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_core::H256;

/// An outbox MMR leaf committing to a single outbound XCMP message.
///
/// The leaf contains the destination parachain ID and a hash of the payload bytes.
/// The global `mmr_leaf_index` (not stored in the leaf itself) serves as the unique
/// identifier/"nonce" for replay protection.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct OutboxLeaf {
	/// Destination parachain ID (as u32).
	pub dest: u32,
	/// Keccak256 hash of the payload bytes.
	pub payload_hash: H256,
}

/// Digest item deposited in the source parachain header to commit the outbox MMR root.
///
/// This is encoded and placed in `DigestItem::PreRuntime(*b"xmmd", ...)`.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct XcmpMmdDigest {
	/// Version byte for future extensibility. Start with 0.
	pub version: u8,
	/// The outbox MMR root after all messages in this block have been appended.
	pub root: H256,
}

/// Hard bounds for the minimal POC to prevent resource exhaustion.
pub mod bounds {
	/// Maximum number of messages that can be submitted in a single `submit_xcmp_mmd` call.
	pub const MAX_MESSAGES_PER_CALL: u32 = 4;

	/// Maximum payload size in bytes (256 KiB).
	pub const MAX_PAYLOAD_BYTES: u32 = 256 * 1024;

	/// Maximum number of proof items in a relay MMR proof.
	/// This bounds the depth of the relay chain's MMR.
	pub const MAX_RELAY_MMR_PROOF_ITEMS: u32 = 128;

	/// Maximum number of proof items in a para-heads Merkle proof.
	/// This bounds the number of parachains (log2 scale).
	pub const MAX_PARA_HEADS_PROOF_ITEMS: u32 = 32;

	/// Maximum number of proof items in an outbox MMR proof.
	/// This bounds the depth of the source parachain's outbox MMR.
	pub const MAX_OUTBOX_MMR_PROOF_ITEMS: u32 = 64;

	/// Implied maximum total call size (approximate).
	/// This is derived from the above bounds: ~768 KiB.
	pub const MAX_TOTAL_CALL_BYTES: u32 = 768 * 1024;
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn outbox_leaf_encodes_correctly() {
		let leaf = OutboxLeaf {
			dest: 1000,
			payload_hash: H256::from([1u8; 32]),
		};
		let encoded = leaf.encode();
		let decoded = OutboxLeaf::decode(&mut &encoded[..]).unwrap();
		assert_eq!(leaf, decoded);
	}

	#[test]
	fn xcmp_mmd_digest_encodes_correctly() {
		let digest = XcmpMmdDigest {
			version: 0,
			root: H256::from([2u8; 32]),
		};
		let encoded = digest.encode();
		let decoded = XcmpMmdDigest::decode(&mut &encoded[..]).unwrap();
		assert_eq!(digest, decoded);
	}
}
