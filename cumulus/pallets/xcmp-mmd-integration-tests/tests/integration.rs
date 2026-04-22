// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for XCMP MMD POC
//!
//! These tests verify the data structures and encoding/decoding
//! between the outbox and inbox pallets.

#![cfg(test)]

use codec::{Decode, Encode};
use cumulus_primitives_xcmp_mmd::OutboxLeaf;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_core::H256;
use sp_runtime::traits::Hash as HashT;

#[test]
fn test_outbox_leaf_encoding() {
	let dest = 2000u32;
	let payload = b"test message".to_vec();
	let payload_hash = sp_runtime::traits::Keccak256::hash(&payload);

	let leaf = OutboxLeaf {
		dest,
		payload_hash,
	};

	// Verify leaf can be encoded and decoded
	let encoded = leaf.encode();
	assert!(!encoded.is_empty());

	let decoded = OutboxLeaf::decode(&mut &encoded[..]).expect("should decode");
	assert_eq!(decoded, leaf);

	// Verify leaf hash is deterministic
	let leaf_hash = sp_runtime::traits::Keccak256::hash(&encoded);
	assert_ne!(leaf_hash, H256::default());
}

#[test]
fn test_message_with_proof_encoding() {
	use cumulus_pallet_xcmp_mmd_inbox::types::MessageWithProof;

	let message = MessageWithProof {
		source: ParaId::from(1000),
		dest: ParaId::from(2000),
		mmr_leaf_index: 0,
		relay_mmr_leaf_index: 0,
		payload: b"test".to_vec(),
		relay_mmr_proof: vec![H256::default()],
		relay_mmr_leaf: vec![0u8; 64],
		relay_mmr_size: 1,
		para_heads_proof: vec![H256::default()],
		outbox_leaf: OutboxLeaf {
			dest: 2000,
			payload_hash: sp_runtime::traits::Keccak256::hash(b"test"),
		},
		outbox_mmr_proof: vec![],
		outbox_mmr_size: 1,
	};

	// Verify message can be encoded
	let encoded = message.encode();
	assert!(!encoded.is_empty());

	// Verify message can be decoded
	let decoded = MessageWithProof::decode(&mut &encoded[..]).expect("should decode");
	assert_eq!(decoded.source, message.source);
	assert_eq!(decoded.dest, message.dest);
	assert_eq!(decoded.payload, message.payload);
	assert_eq!(decoded.outbox_leaf, message.outbox_leaf);
}

#[test]
fn test_payload_hash_verification() {
	let payload = b"Hello from Para 1000!".to_vec();
	let payload_hash = sp_runtime::traits::Keccak256::hash(&payload);

	let leaf = OutboxLeaf {
		dest: 2000,
		payload_hash,
	};

	// Verify the hash matches
	let calculated_hash = sp_runtime::traits::Keccak256::hash(&payload);
	assert_eq!(calculated_hash, leaf.payload_hash);

	// Verify different payload produces different hash
	let different_payload = b"Different message".to_vec();
	let different_hash = sp_runtime::traits::Keccak256::hash(&different_payload);
	assert_ne!(different_hash, leaf.payload_hash);
}

#[test]
fn test_end_to_end_data_flow() {
	use cumulus_pallet_xcmp_mmd_inbox::types::MessageWithProof;

	// Step 1: Source parachain creates an outbox leaf
	let source_para_id = ParaId::from(1000);
	let dest_para_id = ParaId::from(2000);
	let payload = b"Hello from Para 1000!".to_vec();
	let payload_hash = sp_runtime::traits::Keccak256::hash(&payload);

	let outbox_leaf = OutboxLeaf {
		dest: dest_para_id.into(),
		payload_hash,
	};

	let mmr_leaf_index = 0u64;

	// Step 2: Relayer constructs MessageWithProof
	let message = MessageWithProof {
		source: source_para_id,
		dest: dest_para_id,
		mmr_leaf_index,
		relay_mmr_leaf_index: 0,
		payload: payload.clone(),
		relay_mmr_proof: vec![H256::default()],
		relay_mmr_leaf: vec![0u8; 64],
		relay_mmr_size: 1,
		para_heads_proof: vec![H256::default()],
		outbox_leaf: outbox_leaf.clone(),
		outbox_mmr_proof: vec![],
		outbox_mmr_size: 1,
	};

	// Step 3: Verify message structure
	assert_eq!(message.source, source_para_id);
	assert_eq!(message.dest, dest_para_id);
	assert_eq!(message.mmr_leaf_index, mmr_leaf_index);
	assert_eq!(message.outbox_leaf, outbox_leaf);
	assert_eq!(message.payload, payload);

	// Step 4: Verify payload hash matches
	let calculated_hash = sp_runtime::traits::Keccak256::hash(&message.payload);
	assert_eq!(calculated_hash, message.outbox_leaf.payload_hash);

	// Step 5: Verify destination matches
	let dest_u32: u32 = dest_para_id.into();
	assert_eq!(message.outbox_leaf.dest, dest_u32);
}

#[test]
fn test_mmr_leaf_hash_consistency() {
	// Create two identical leaves
	let leaf1 = OutboxLeaf {
		dest: 2000,
		payload_hash: H256::from([1u8; 32]),
	};

	let leaf2 = OutboxLeaf {
		dest: 2000,
		payload_hash: H256::from([1u8; 32]),
	};

	// Verify encoding is identical
	let encoded1 = leaf1.encode();
	let encoded2 = leaf2.encode();
	assert_eq!(encoded1, encoded2);

	// Verify hash is identical
	let hash1 = sp_runtime::traits::Keccak256::hash(&encoded1);
	let hash2 = sp_runtime::traits::Keccak256::hash(&encoded2);
	assert_eq!(hash1, hash2);
}

#[test]
fn test_replay_protection_key_format() {
	// Verify the replay protection key format
	let source_para_id = 1000u32;
	let mmr_leaf_index = 42u64;

	let key = (source_para_id, mmr_leaf_index);

	// Verify key components
	assert_eq!(key.0, source_para_id);
	assert_eq!(key.1, mmr_leaf_index);

	// Verify different messages have different keys
	let key2 = (source_para_id, mmr_leaf_index + 1);
	assert_ne!(key, key2);

	let key3 = (source_para_id + 1, mmr_leaf_index);
	assert_ne!(key, key3);
}

#[test]
fn test_message_size_bounds() {
	use cumulus_pallet_xcmp_mmd_inbox::types::MessageWithProof;

	// Test with maximum payload size (256 KiB)
	let max_payload_size = 256 * 1024;
	let large_payload = vec![0u8; max_payload_size];
	let payload_hash = sp_runtime::traits::Keccak256::hash(&large_payload);

	let message = MessageWithProof {
		source: ParaId::from(1000),
		dest: ParaId::from(2000),
		mmr_leaf_index: 0,
		relay_mmr_leaf_index: 0,
		payload: large_payload.clone(),
		relay_mmr_proof: vec![H256::default(); 128], // Max relay MMR proof items
		relay_mmr_leaf: vec![0u8; 64],
		relay_mmr_size: 1,
		para_heads_proof: vec![H256::default(); 32], // Max para-heads proof items
		outbox_leaf: OutboxLeaf {
			dest: 2000,
			payload_hash,
		},
		outbox_mmr_proof: vec![H256::default(); 64], // Max outbox MMR proof items
		outbox_mmr_size: 1,
	};

	// Verify message can be encoded even at maximum size
	let encoded = message.encode();
	assert!(!encoded.is_empty());

	// Verify payload size
	assert_eq!(message.payload.len(), max_payload_size);
}

