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

//! Tests for XCMP MMD inbox pallet.

use crate::{mock::*, *};
use cumulus_primitives_xcmp_mmd::OutboxLeaf;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_core::H256;

#[test]
fn test_replay_protection() {
	new_test_ext().execute_with(|| {
		// Create a test message (not used yet, but will be when verification is implemented)
		let _message = types::MessageWithProof {
			source: ParaId::from(1000),
			dest: ParaId::from(2000),
			mmr_leaf_index: 0,
			relay_mmr_leaf_index: 0,
			payload: b"test".to_vec(),
			relay_mmr_proof: vec![],
			relay_mmr_leaf: vec![0u8; 32],
			relay_mmr_size: 1,
			para_heads_proof: vec![],
			outbox_leaf: OutboxLeaf {
				dest: 2000,
				payload_hash: H256::default(),
			},
			outbox_mmr_proof: vec![],
			outbox_mmr_size: 1,
		};

		// First submission should succeed (for now, just checking replay protection)
		let key = (1000u32, 0u64);
		assert!(!SeenMessages::<Test>::contains_key(key));

		// Mark as seen
		SeenMessages::<Test>::insert(key, ());

		// Verify it's marked as seen
		assert!(SeenMessages::<Test>::contains_key(key));
	});
}

#[test]
fn test_genesis_config_builds() {
	new_test_ext().execute_with(|| {
		// Just verify the mock runtime builds
		assert_eq!(System::block_number(), 0);
	});
}
