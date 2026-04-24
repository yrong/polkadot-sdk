// Core types shared across the relayer

use codec::{Decode, Encode};
use serde::{Deserialize, Serialize};
use sp_core::H256;

/// An outbox MMR leaf (matches cumulus-primitives-xcmp-mmd::OutboxLeaf)
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
pub struct OutboxLeaf {
    pub dest: u32,
    pub payload_hash: H256,
}

/// The digest item deposited in source parachain headers
#[derive(Clone, Debug, Encode, Decode)]
pub struct XcmpMmdDigest {
    pub version: u8,
    pub root: H256,
}

/// A pending message discovered on the source chain
#[derive(Clone, Debug)]
pub struct PendingMessage {
    /// Source parachain ID
    pub source_para_id: u32,
    /// Destination parachain ID
    pub dest_para_id: u32,
    /// Leaf index in the source outbox MMR
    pub mmr_leaf_index: u64,
    /// Source block hash where the message was committed
    pub source_block_hash: H256,
    /// Source block number
    pub source_block_number: u32,
    /// The outbox MMR root from the header digest
    pub outbox_mmr_root: H256,
    /// The raw payload bytes
    pub payload: Vec<u8>,
}

/// MessageWithProof submitted to destination submit_xcmp_mmd extrinsic.
///
/// Matches cumulus-pallet-xcmp-mmd-inbox::types::MessageWithProof
#[derive(Clone, Debug, Encode, Decode, Serialize, Deserialize)]
pub struct MessageWithProof {
    pub source: u32,
    pub dest: u32,
    pub mmr_leaf_index: u64,
    pub relay_mmr_leaf_index: u64,
    pub payload: Vec<u8>,
    pub relay_mmr_proof: Vec<H256>,
    pub relay_mmr_leaf: Vec<u8>,
    pub relay_mmr_size: u64,
    pub relay_anchor_number: u32,
    pub relay_ancestry_proof: Option<AncestryProof>,
    pub para_heads_proof: Vec<H256>,
    pub source_head: Vec<u8>,
    pub para_head_index: u32,
    pub para_heads_count: u32,
    pub outbox_leaf: OutboxLeaf,
    pub outbox_mmr_proof: Vec<H256>,
    pub outbox_mmr_size: u64,
}

/// Relay MMR ancestry proof (matches sp_mmr_primitives::AncestryProof)
#[derive(Clone, Debug, Encode, Decode, Serialize, Deserialize)]
pub struct AncestryProof {
    pub prev_peaks: Vec<H256>,
    pub prev_leaf_count: u64,
    pub leaf_count: u64,
    pub items: Vec<(u64, H256)>,
}

/// Result of outbox proof generation from source runtime API
#[derive(Clone, Debug, Deserialize)]
pub struct OutboxProof {
    pub leaf_index: u64,
    pub mmr_size: u64,
    pub leaf: OutboxLeaf,
    pub proof_items: Vec<H256>,
}

/// Result of relay MMR proof generation
#[derive(Clone, Debug)]
pub struct RelayMmrProof {
    pub leaf_index: u64,
    pub mmr_size: u64,
    pub leaf_bytes: Vec<u8>,
    pub proof_items: Vec<H256>,
    pub para_heads_root: H256,
}

/// Para-heads Merkle proof
#[derive(Clone, Debug)]
pub struct ParaHeadsProof {
    pub proof_items: Vec<H256>,
    pub head_bytes: Vec<u8>,
    pub leaf_index: u32,
    pub number_of_leaves: u32,
}

/// SCALE-encoded para head entry as stored in the Merkle tree
/// Matches the relay chain's ParaHeadsRootProvider: SCALE((para_id_u32, head_bytes))
pub fn encode_para_head_leaf(para_id: u32, head_bytes: &[u8]) -> Vec<u8> {
    (para_id, head_bytes.to_vec()).encode()
}
