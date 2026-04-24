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

//! Helper functions for XCMP MMD verification.

use alloc::vec;
use alloc::vec::Vec;
use codec::{Decode, Encode};
use cumulus_primitives_xcmp_mmd::{OutboxLeaf, XcmpMmdDigest};
use frame_support::traits::Get;
use sp_core::H256;
use sp_mmr_primitives::AncestryProof;
use sp_runtime::traits::Hash as HashT;



/// Read the relay MMR root from the relay chain state proof.
pub fn read_mmr_root_from_relay_proof<T: frame_system::Config + cumulus_pallet_parachain_system::Config>(
) -> Result<H256, crate::Error<T>> {
	// Get the validation data which contains relay parent info
	let validation_data = cumulus_pallet_parachain_system::ValidationData::<T>::get()
		.ok_or(crate::Error::<T>::FailedToReadRelayMmrRoot)?;

	// Get the relay state proof
	let relay_state_proof_storage = cumulus_pallet_parachain_system::RelayStateProof::<T>::get()
		.ok_or(crate::Error::<T>::FailedToReadRelayMmrRoot)?;

	// Construct RelayChainStateProof
	let relay_state_proof = cumulus_pallet_parachain_system::RelayChainStateProof::new(
		T::SelfParaId::get(),
		validation_data.relay_parent_storage_root,
		relay_state_proof_storage,
	)
	.map_err(|_| crate::Error::<T>::FailedToReadRelayMmrRoot)?;

	// Read MMR root from the proof
	let mmr_root: H256 = relay_state_proof
		.read_entry(polkadot_primitives::well_known_keys::MMR_ROOT_HASH, None)
		.map_err(|_| crate::Error::<T>::FailedToReadRelayMmrRoot)?;

	Ok(mmr_root)
}

/// Verify a relay MMR proof and extract the ParaHeadsRoot.
///
/// The relay chain MMR uses BEEFY MMR leaves which contain the ParaHeadsRoot.
/// This function:
/// 1. Verifies the MMR proof using mmr-lib
/// 2. Extracts the ParaHeadsRoot from the verified leaf
///
/// Note: For this POC, we use a simplified approach where the leaf data
/// is provided as raw bytes. A production implementation would use the
/// full BEEFY MMR leaf structure with proper decoding.
pub fn verify_relay_mmr_proof<T: frame_system::Config>(
	mmr_root: H256,
	relay_mmr_leaf_index: u64,
	relay_mmr_size: u64,
	relay_mmr_leaf: &[u8],
	relay_mmr_proof: &[H256],
) -> Result<H256, crate::Error<T>> {
	use mmr_lib::{Merge, Result as MmrResult};
	use sp_consensus_beefy::mmr::MmrLeaf;
	use sp_mmr_primitives::EncodableOpaqueLeaf;

	// Define Keccak256Merge for relay MMR (same as used by pallet-mmr)
	struct Keccak256Merge;
	impl Merge for Keccak256Merge {
		type Item = H256;
		fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
			let mut concat = [0u8; 64];
			concat[..32].copy_from_slice(lhs.as_ref());
			concat[32..].copy_from_slice(rhs.as_ref());
			Ok(sp_runtime::traits::Keccak256::hash(&concat))
		}
	}

	// Hash the relay MMR leaf
	let leaf_hash = sp_runtime::traits::Keccak256::hash(relay_mmr_leaf);

	// Create the MerkleProof
	let proof = mmr_lib::MerkleProof::<H256, Keccak256Merge>::new(
		relay_mmr_size,
		relay_mmr_proof.to_vec(),
	);

	// Verify the proof by calculating the root
	let calculated_root = proof
		.calculate_root(vec![(relay_mmr_leaf_index, leaf_hash)])
		.map_err(|_| crate::Error::<T>::InvalidRelayMmrProof)?;

	// Check if the calculated root matches the expected root
	if calculated_root != mmr_root {
		return Err(crate::Error::<T>::InvalidRelayMmrProof);
	}

	// Extract ParaHeadsRoot from the proven relay leaf.
	//
	// On Polkadot-style relays we rely on:
	// - `pallet_beefy_mmr::LeafExtra = H256`
	// - `LeafExtra` is the relay `ParaHeadsRoot`
	//
	// The leaf may come as:
	// - raw SCALE-encoded `MmrLeaf<_, _, _, H256>`
	// - SCALE-encoded `EncodableOpaqueLeaf(Vec<u8>)` wrapping the compact leaf bytes
	let leaf: MmrLeaf<u32, H256, H256, H256> = MmrLeaf::decode(&mut &relay_mmr_leaf[..])
		.or_else(|_| {
			let enc = EncodableOpaqueLeaf::decode(&mut &relay_mmr_leaf[..])?;
			MmrLeaf::decode(&mut &enc.0[..])
		})
		.map_err(|_| crate::Error::<T>::InvalidRelayMmrProof)?;

	let para_heads_root = leaf.leaf_extra;

	Ok(para_heads_root)
}

/// Verify a para-heads proof against the ParaHeadsRoot.
///
/// The ParaHeadsRoot is a binary merkle tree root of all parachain heads,
/// built with KeccakHasher and SCALE((para_id_u32, head_bytes)) leaves,
/// sorted by para_id — matching the relay chain's ParaHeadsRootProvider.
pub fn verify_para_heads_proof<T: frame_system::Config>(
	para_heads_root: H256,
	source_para_id: u32,
	source_head: &[u8],
	para_head_index: u32,
	para_heads_count: u32,
	para_heads_proof: &[H256],
) -> Result<(), crate::Error<T>> {
	// Leaf encoding matches relay chain: SCALE((para_id_u32, head_bytes))
	let leaf: Vec<u8> = (source_para_id, source_head.to_vec()).encode();

	let valid = binary_merkle_tree::verify_proof::<sp_runtime::traits::Keccak256, _, _>(
		&para_heads_root,
		para_heads_proof.iter().copied(),
		para_heads_count,
		para_head_index,
		&leaf,
	);

	if valid {
		Ok(())
	} else {
		Err(crate::Error::<T>::InvalidParaHeadsProof)
	}
}

/// Decode a parachain header from bytes.
pub fn decode_source_header<T: frame_system::Config>(
	header_bytes: &[u8],
) -> Result<sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>, crate::Error<T>> {
	sp_runtime::generic::Header::decode(&mut &header_bytes[..])
		.map_err(|_| crate::Error::<T>::FailedToDecodeSourceHeader)
}

/// Extract the outbox MMR root from a parachain header's digest.
pub fn extract_outbox_mmr_root<T: frame_system::Config>(
	header: &sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
) -> Result<H256, crate::Error<T>> {
	for digest_item in &header.digest.logs {
		if let sp_runtime::DigestItem::PreRuntime(engine_id, data) = digest_item {
			if engine_id == b"xmmd" {
				let xcmp_digest = XcmpMmdDigest::decode(&mut &data[..])
					.map_err(|_| crate::Error::<T>::FailedToExtractOutboxMmrRoot)?;
				return Ok(xcmp_digest.root);
			}
		}
	}
	Err(crate::Error::<T>::FailedToExtractOutboxMmrRoot)
}

/// Verify an outbox MMR proof and return the leaf.
pub fn verify_outbox_mmr_proof<T: frame_system::Config>(
	outbox_mmr_root: H256,
	mmr_leaf_index: u64,
	mmr_size: u64,
	outbox_leaf: &OutboxLeaf,
	outbox_mmr_proof: &[H256],
) -> Result<(), crate::Error<T>> {
	use codec::Encode;
	use mmr_lib::{Merge, Result as MmrResult};

	// Define the same Keccak256Merge used in the outbox pallet
	struct Keccak256Merge;
	impl Merge for Keccak256Merge {
		type Item = H256;
		fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
			let mut concat = [0u8; 64];
			concat[..32].copy_from_slice(lhs.as_ref());
			concat[32..].copy_from_slice(rhs.as_ref());
			Ok(sp_runtime::traits::Keccak256::hash(&concat))
		}
	}

	// Hash the leaf (same as in outbox pallet)
	let leaf_hash = sp_runtime::traits::Keccak256::hash(&outbox_leaf.encode());

	// Create the MerkleProof
	let proof = mmr_lib::MerkleProof::<H256, Keccak256Merge>::new(
		mmr_size,
		outbox_mmr_proof.to_vec(),
	);

	// Verify the proof
	let calculated_root = proof
		.calculate_root(vec![(mmr_leaf_index, leaf_hash)])
		.map_err(|_| crate::Error::<T>::InvalidOutboxMmrProof)?;

	// Check if the calculated root matches the expected root
	if calculated_root == outbox_mmr_root {
		Ok(())
	} else {
		Err(crate::Error::<T>::InvalidOutboxMmrProof)
	}
}

/// Verify payload hash matches the expected hash.
pub fn verify_payload_hash<T: frame_system::Config>(
	payload: &[u8],
	expected_hash: H256,
) -> Result<(), crate::Error<T>> {
	let actual_hash = sp_runtime::traits::Keccak256::hash(payload);
	if actual_hash == expected_hash {
		Ok(())
	} else {
		Err(crate::Error::<T>::PayloadHashMismatch)
	}
}

/// Verify relay ancestry proof and derive historical MMR root.
///
/// Given the current relay MMR root, proves that the anchor block is an ancestor
/// and returns the MMR root at the anchor block.
pub fn verify_relay_ancestry_proof<T: frame_system::Config>(
	current_mmr_root: H256,
	ancestry_proof: AncestryProof<H256>,
	anchor_block_number: u32,
	current_block_number: u32,
) -> Result<H256, crate::Error<T>> {
	// Sanity check: anchor must be in the past
	if anchor_block_number >= current_block_number {
		return Err(crate::Error::<T>::InvalidAncestryProof);
	}

	// Use pallet_mmr's stateless ancestry proof verification
	// This returns the historical MMR root at the anchor block
	let historical_mmr_root = pallet_mmr::verify_ancestry_proof::<
		sp_runtime::traits::Keccak256,
		sp_consensus_beefy::mmr::MmrLeaf<u32, H256, H256, H256>,
	>(current_mmr_root, ancestry_proof)
		.map_err(|_| crate::Error::<T>::InvalidAncestryProof)?;

	Ok(historical_mmr_root)
}

