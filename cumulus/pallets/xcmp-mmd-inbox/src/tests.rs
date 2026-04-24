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
use sp_runtime::DispatchError;

#[test]
fn test_replay_protection() {
	new_test_ext().execute_with(|| {
		// Replay protection is a pure storage check.
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

fn dummy_message(dest: ParaId) -> types::MessageWithProof {
	types::MessageWithProof {
		source: ParaId::from(1000),
		dest,
		mmr_leaf_index: 0,
		relay_mmr_leaf_index: 0,
		payload: b"test".to_vec(),
		relay_mmr_proof: vec![],
		relay_mmr_leaf: vec![],
		relay_mmr_size: 1,
		relay_anchor_number: 0,
		relay_ancestry_proof: None,
		para_heads_proof: vec![],
		source_head: vec![],
		para_head_index: 0,
		para_heads_count: 1,
		outbox_leaf: OutboxLeaf {
			dest: dest.into(),
			payload_hash: H256::default(),
		},
		outbox_mmr_proof: vec![],
		outbox_mmr_size: 1,
	}
}

#[test]
fn submit_rejects_wrong_destination_early() {
	new_test_ext().execute_with(|| {
		let msg = dummy_message(ParaId::from(9999));
		let err = XcmpMmdInbox::submit_xcmp_mmd(RuntimeOrigin::signed(1), vec![msg])
			.unwrap_err();
		assert!(matches!(err, DispatchError::Module(_)));
	});
}

#[test]
fn submit_rejects_too_many_messages_early() {
	new_test_ext().execute_with(|| {
		let msg = dummy_message(SelfParaId::get());
		let msgs = vec![msg.clone(), msg.clone(), msg.clone(), msg.clone(), msg];
		let err = XcmpMmdInbox::submit_xcmp_mmd(RuntimeOrigin::signed(1), msgs).unwrap_err();
		assert!(matches!(err, DispatchError::Module(_)));
	});
}
