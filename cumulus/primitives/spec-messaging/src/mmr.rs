// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! MMR merge for the Speculative Messaging protocol.
//!
//! The per-destination subtree MMR is built on [`mmr_lib`]
//! (`polkadot-ckb-merkle-mountain-range`) parameterised with [`SpecMerge`]. We
//! reuse `mmr_lib`'s audited inclusion proofs (`gen_proof`/`MerkleProof::verify`)
//! and append-only ancestry proofs (`gen_ancestry_proof`/`verify_ancestor`)
//! rather than hand-rolling any proof code.
//!
//! [`SpecMerge`] gives the MMR full domain separation: leaves are hashed by
//! [`crate::outgoing_message::OutgoingMessage::hash_leaf`] (`LEAF_TAG`), inner
//! nodes by [`SpecMerge::merge`] (`INNER_TAG`), and peak-bagging by
//! [`SpecMerge::merge_peaks`] (`PEAK_TAG`). `mmr_lib` calls `merge` for tree
//! nodes and the overridable `merge_peaks` when bagging peaks, so no two roles
//! can collide on the same hash.

use mmr_lib::{Error as MmrError, Merge};
use polkadot_core_primitives::Hash;
use sp_io::hashing::blake2_256;

use crate::{INNER_TAG, PEAK_TAG};

/// Domain-tagged `blake2_256` merge for the speculative-messaging MMR, used as
/// the `mmr_lib::Merge` implementation for the per-destination subtree.
pub struct SpecMerge;

impl Merge for SpecMerge {
	type Item = Hash;

	/// Inner-node merge: `blake2_256(INNER_TAG ++ left ++ right)`.
	fn merge(left: &Hash, right: &Hash) -> Result<Hash, MmrError> {
		Ok(tagged_node(INNER_TAG, left, right))
	}

	/// Peak-bagging merge: `blake2_256(PEAK_TAG ++ left ++ right)`. Domain-separated
	/// from `merge` so a bagged value can never be reinterpreted as an inner node.
	fn merge_peaks(left: &Hash, right: &Hash) -> Result<Hash, MmrError> {
		Ok(tagged_node(PEAK_TAG, left, right))
	}
}

/// `blake2_256(tag ++ left ++ right)`.
fn tagged_node(tag: u8, left: &Hash, right: &Hash) -> Hash {
	let mut preimage = [0u8; 1 + 32 + 32];
	preimage[0] = tag;
	preimage[1..33].copy_from_slice(left.as_bytes());
	preimage[33..65].copy_from_slice(right.as_bytes());
	blake2_256(&preimage).into()
}

/// Compute the MMR root from its peaks (highest to lowest), matching
/// `mmr_lib`'s bagging (`merge_peaks(right, left)` folded right-to-left).
///
/// This lets the on-chain outbox keep only the O(log n) peaks and still derive
/// the same `subtree_root` that `mmr_lib`'s `MMR::get_root` and
/// `MerkleProof::verify` produce. Returns `None` for an empty peak list (an MMR
/// with no leaves has no commitment).
pub fn root_from_peaks(peaks: &[Hash]) -> Option<Hash> {
	let mut iter = peaks.iter().rev();
	let mut acc = *iter.next()?;
	for left in iter {
		// mmr_lib bags as merge_peaks(right, left); `acc` carries the right side.
		acc = SpecMerge::merge_peaks(&acc, left).ok()?;
	}
	Some(acc)
}

#[cfg(test)]
mod tests {
	use super::*;
	use mmr_lib::{leaf_index_to_pos, util::MemStore, MerkleProof, MMR};

	fn h(byte: u8) -> Hash {
		Hash::repeat_byte(byte)
	}

	#[test]
	fn merge_and_merge_peaks_are_domain_separated() {
		let a = h(1);
		let b = h(2);

		// Inner-node merge and peak-bagging of the same inputs must differ.
		assert_ne!(SpecMerge::merge(&a, &b).unwrap(), SpecMerge::merge_peaks(&a, &b).unwrap());
	}

	#[test]
	fn merge_is_order_sensitive() {
		let a = h(1);
		let b = h(2);

		assert_ne!(SpecMerge::merge(&a, &b).unwrap(), SpecMerge::merge(&b, &a).unwrap());
		assert_ne!(
			SpecMerge::merge_peaks(&a, &b).unwrap(),
			SpecMerge::merge_peaks(&b, &a).unwrap()
		);
	}

	#[test]
	fn single_peak_root_is_the_peak_itself() {
		assert_eq!(root_from_peaks(&[h(7)]), Some(h(7)));
		assert_eq!(root_from_peaks(&[]), None);
	}

	#[test]
	fn root_from_peaks_matches_mmr_lib() {
		// Build a small MMR and confirm root_from_peaks(peaks) == MMR::get_root().
		let store = MemStore::default();
		let mut mmr = MMR::<Hash, SpecMerge, _>::new(0, &store);
		for i in 0..5u8 {
			mmr.push(h(i)).unwrap();
		}
		let root = mmr.get_root().unwrap();

		// Peaks of a 5-leaf MMR: one height-2 peak (leaves 0..4) and one height-0
		// peak (leaf 4). Reconstruct them from the leaves via SpecMerge.
		let n01 = SpecMerge::merge(&h(0), &h(1)).unwrap();
		let n23 = SpecMerge::merge(&h(2), &h(3)).unwrap();
		let peak_2 = SpecMerge::merge(&n01, &n23).unwrap();
		let peaks = [peak_2, h(4)];

		assert_eq!(root_from_peaks(&peaks), Some(root));
	}

	#[test]
	fn inclusion_proof_round_trips() {
		let store = MemStore::default();
		let mut mmr = MMR::<Hash, SpecMerge, _>::new(0, &store);
		let positions: Vec<u64> = (0..6u8).map(|i| mmr.push(h(i)).unwrap()).collect();
		let root = mmr.get_root().unwrap();

		// Prove leaves at indices 1 and 4.
		let proof_positions = vec![leaf_index_to_pos(1), leaf_index_to_pos(4)];
		let proof: MerkleProof<Hash, SpecMerge> = mmr.gen_proof(proof_positions).unwrap();

		let leaves = vec![(positions[1], h(1)), (positions[4], h(4))];
		assert!(proof.verify(root, leaves).unwrap());

		// A tampered leaf must fail verification.
		let bad = vec![(positions[1], h(99)), (positions[4], h(4))];
		assert!(!proof.verify(root, bad).unwrap());
	}
}
