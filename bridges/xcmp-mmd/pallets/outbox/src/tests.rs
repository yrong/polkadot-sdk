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

//! Tests for XCMP MMD outbox pallet.

use crate::{mock::*, *};
use codec::Decode;
use frame_support::traits::Hooks;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::traits::Hash;

#[test]
fn test_note_outbound_increments_leaf_count() {
	new_test_ext().execute_with(|| {
		// Initially, leaf count should be 0
		assert_eq!(MmrLeafCount::<Test>::get(), 0);

		// Note an outbound message
		let dest = ParaId::from(1000);
		let payload = b"test message".to_vec();
		Pallet::<Test>::note_outbound(dest, &payload);

		// Leaf count should be incremented
		assert_eq!(MmrLeafCount::<Test>::get(), 1);

		// Leaf should be stored
		let leaf = OutboxLeaves::<Test>::get(0).expect("Leaf should exist");
		assert_eq!(leaf.dest, 1000);
		assert_eq!(leaf.payload_hash, sp_runtime::traits::Keccak256::hash(&payload));
	});
}

#[test]
fn test_multiple_messages_increment_leaf_count() {
	new_test_ext().execute_with(|| {
		// Note multiple messages
		for i in 0..5 {
			let dest = ParaId::from(1000 + i);
			let payload = format!("message {}", i).into_bytes();
			Pallet::<Test>::note_outbound(dest, &payload);
		}

		// Leaf count should be 5
		assert_eq!(MmrLeafCount::<Test>::get(), 5);

		// All leaves should be stored
		for i in 0..5 {
			let leaf = OutboxLeaves::<Test>::get(i as u64).expect("Leaf should exist");
			assert_eq!(leaf.dest, 1000 + i);
		}
	});
}

#[test]
fn test_mmr_root_updates() {
	new_test_ext().execute_with(|| {
		// Initial root should be default (zero)
		let initial_root = MmrRootHash::<Test>::get();
		assert_eq!(initial_root, sp_core::H256::default());

		// Note a message
		let dest = ParaId::from(1000);
		let payload = b"test message".to_vec();
		Pallet::<Test>::note_outbound(dest, &payload);

		// Root should be updated
		let new_root = MmrRootHash::<Test>::get();
		assert_ne!(new_root, initial_root);
		assert_ne!(new_root, sp_core::H256::default());
	});
}

#[test]
fn test_digest_deposited_on_finalize() {
	new_test_ext().execute_with(|| {
		// Note a message
		let dest = ParaId::from(1000);
		let payload = b"test message".to_vec();
		Pallet::<Test>::note_outbound(dest, &payload);

		// Get the MMR root before finalize
		let mmr_root = MmrRootHash::<Test>::get();

		// Call on_finalize
		XcmpMmdOutbox::on_finalize(1);

		// Check that digest was deposited
		let digest = frame_system::Pallet::<Test>::digest();
		assert_eq!(digest.logs.len(), 1);

		// Verify it's a PreRuntime digest with correct engine ID
		if let sp_runtime::DigestItem::PreRuntime(engine_id, data) = &digest.logs[0] {
			assert_eq!(engine_id, b"xmmd");

			// Decode the digest
			let xcmp_digest =
				cumulus_primitives_xcmp_mmd::XcmpMmdDigest::decode(&mut &data[..]).unwrap();
			assert_eq!(xcmp_digest.version, 0);
			assert_eq!(xcmp_digest.root, mmr_root);
		} else {
			panic!("Expected PreRuntime digest");
		}
	});
}

#[test]
fn test_generate_proof() {
	new_test_ext().execute_with(|| {
		// Note multiple messages
		for i in 0..3 {
			let dest = ParaId::from(1000 + i);
			let payload = format!("message {}", i).into_bytes();
			Pallet::<Test>::note_outbound(dest, &payload);
		}

		// Generate proof for leaf 1
		let result = Pallet::<Test>::generate_proof(1);
		assert!(result.is_some());

		let (leaf, proof, mmr_size) = result.unwrap();
		assert_eq!(leaf.dest, 1001);
		assert_eq!(mmr_size, 3);
		assert!(!proof.is_empty()); // Should have proof items
	});
}

#[test]
fn test_generate_proof_for_nonexistent_leaf() {
	new_test_ext().execute_with(|| {
		// Try to generate proof for leaf that doesn't exist
		let result = Pallet::<Test>::generate_proof(999);
		assert!(result.is_none());
	});
}

#[test]
fn test_mmr_root_changes_with_each_message() {
	new_test_ext().execute_with(|| {
		let mut roots = alloc::vec::Vec::new();

		// Note messages and collect roots
		for i in 0..5 {
			let dest = ParaId::from(1000 + i);
			let payload = format!("message {}", i).into_bytes();
			Pallet::<Test>::note_outbound(dest, &payload);

			let root = MmrRootHash::<Test>::get();
			roots.push(root);
		}

		// All roots should be different
		for i in 0..roots.len() {
			for j in (i + 1)..roots.len() {
				assert_ne!(roots[i], roots[j], "Roots at {} and {} should differ", i, j);
			}
		}
	});
}
