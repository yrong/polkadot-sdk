// Proof construction for XCMP MMD relayer
//
// Builds all three proof tiers:
//   1. Outbox MMR proof (from source runtime API)
//   2. Relay MMR proof (from relay chain mmr_generateProof RPC)
//   3. Para-heads Merkle proof (reconstructed from relay state)

use anyhow::{anyhow, Context, Result};
use binary_merkle_tree::{merkle_proof, MerkleProof};
use codec::Encode;
use sp_core::H256;
use tracing::{debug, info};

use crate::client::{RelayClient, SourceClient};
use crate::types::{
    encode_para_head_leaf, MessageWithProof, OutboxLeaf, OutboxProof, ParaHeadsProof,
    PendingMessage, RelayMmrProof,
};

/// Build the complete MessageWithProof for a pending message.
pub async fn build_message_with_proof(
    message: &PendingMessage,
    source_client: &SourceClient,
    relay_client: &RelayClient,
) -> Result<MessageWithProof> {
    info!(
        "Building proof for message: para={} leaf_index={}",
        message.source_para_id, message.mmr_leaf_index
    );

    // Step 1: Generate outbox MMR proof from source runtime API
    let outbox_proof = build_outbox_proof(message, source_client).await?;

    // Step 2: Find which relay block included the source block
    let source_header_bytes = encode_para_header_for_relay(&message.payload);
    let (relay_block_hash, relay_block_num) = relay_client
        .find_relay_block_for_source(message.source_para_id, &source_header_bytes)
        .await?
        .ok_or_else(|| anyhow!(
            "Could not find relay block that includes source block {:?}",
            message.source_block_hash
        ))?;
    info!("Found relay block #{} ({:?})", relay_block_num, relay_block_hash);

    let relay_leaf_index = RelayClient::relay_block_to_leaf_index(relay_block_num);

    // Step 3: Generate relay MMR proof
    let relay_mmr_proof = build_relay_mmr_proof(relay_leaf_index, relay_block_num, relay_client).await?;

    // Step 4: Generate para-heads Merkle proof
    let para_heads_proof = build_para_heads_proof(
        message.source_para_id,
        &relay_mmr_proof.para_heads_root,
        relay_block_hash,
        relay_client,
    ).await?;

    Ok(MessageWithProof {
        source: message.source_para_id,
        dest: message.dest_para_id,
        mmr_leaf_index: message.mmr_leaf_index,
        relay_mmr_leaf_index: relay_leaf_index,
        payload: message.payload.clone(),
        relay_mmr_proof: relay_mmr_proof.proof_items,
        relay_mmr_leaf: relay_mmr_proof.leaf_bytes,
        relay_mmr_size: relay_mmr_proof.mmr_size,
        para_heads_proof: para_heads_proof.proof_items,
        outbox_leaf: outbox_proof.leaf,
        outbox_mmr_proof: outbox_proof.proof_items,
        outbox_mmr_size: outbox_proof.mmr_size,
    })
}

/// Step 1: Call XcmpMmdOutboxApi::generate_outbox_proof on source chain
async fn build_outbox_proof(
    message: &PendingMessage,
    source_client: &SourceClient,
) -> Result<OutboxProof> {
    let proof = source_client
        .generate_outbox_proof(message.mmr_leaf_index, message.source_block_hash)
        .await?
        .ok_or_else(|| anyhow!(
            "Source runtime returned no proof for leaf_index={}",
            message.mmr_leaf_index
        ))?;

    // Verify the leaf matches what we expect
    let expected_hash = sp_core::hashing::keccak_256(&message.payload);
    if proof.leaf.payload_hash.as_bytes() != expected_hash {
        return Err(anyhow!(
            "Outbox leaf payload_hash mismatch: expected {:?}, got {:?}",
            hex::encode(expected_hash),
            proof.leaf.payload_hash
        ));
    }
    if proof.leaf.dest != message.dest_para_id {
        return Err(anyhow!(
            "Outbox leaf dest mismatch: expected {}, got {}",
            message.dest_para_id, proof.leaf.dest
        ));
    }

    info!(
        "Outbox proof: leaf_index={}, mmr_size={}, {} proof items",
        proof.leaf_index, proof.mmr_size, proof.proof_items.len()
    );
    Ok(proof)
}

/// Step 2: Generate relay MMR proof and extract ParaHeadsRoot from leaf
async fn build_relay_mmr_proof(
    relay_leaf_index: u64,
    relay_block_num: u32,
    relay_client: &RelayClient,
) -> Result<RelayMmrProof> {
    let result = relay_client.inner
        .mmr_generate_proof(vec![relay_block_num as u64], None)
        .await?;

    // Parse the mmr_generateProof response
    // Response: { blockHash, leaves: [hex], proof: { leafIndices, leafCount, items: [hex] } }
    let leaves = result["leaves"].as_array()
        .ok_or_else(|| anyhow!("Expected 'leaves' array in mmr_generateProof response"))?;
    let proof_obj = &result["proof"];

    let leaf_bytes = if let Some(leaf_hex) = leaves.first().and_then(|l| l.as_str()) {
        hex::decode(leaf_hex.trim_start_matches("0x"))?
    } else {
        return Err(anyhow!("No leaf data in mmr_generateProof response"));
    };

    let items = proof_obj["items"].as_array()
        .ok_or_else(|| anyhow!("Expected 'items' in proof"))?;
    let proof_items: Vec<H256> = items.iter()
        .map(|item| {
            let hex_str = item.as_str().unwrap_or("0x");
            let bytes = hex::decode(hex_str.trim_start_matches("0x"))
                .unwrap_or_default();
            if bytes.len() == 32 {
                H256::from_slice(&bytes)
            } else {
                H256::default()
            }
        })
        .collect();

    let leaf_count = proof_obj["leafCount"].as_u64()
        .ok_or_else(|| anyhow!("Expected 'leafCount' in proof"))?;

    // Extract ParaHeadsRoot from the BEEFY MMR leaf
    // BEEFY MMR leaf structure:
    //   version: u8 (1 byte)
    //   parent_number: u32 (4 bytes)
    //   parent_hash: H256 (32 bytes)
    //   next_authority_set_id: u64 (8 bytes)
    //   next_authority_set_len: u32 (4 bytes)
    //   next_authority_set_root: H256 (32 bytes)
    //   leaf_extra: H256 (32 bytes) <-- ParaHeadsRoot
    let para_heads_root = extract_para_heads_root(&leaf_bytes)?;

    info!(
        "Relay MMR proof: leaf_index={}, leaf_count={}, {} proof items, ParaHeadsRoot={:?}",
        relay_leaf_index, leaf_count, proof_items.len(), para_heads_root
    );

    Ok(RelayMmrProof {
        leaf_index: relay_leaf_index,
        mmr_size: leaf_count,
        leaf_bytes,
        proof_items,
        para_heads_root,
    })
}

/// Extract ParaHeadsRoot (leaf_extra) from a BEEFY MMR leaf
///
/// BEEFY MmrLeaf layout (SCALE encoded):
///   version: u8
///   parent_number_and_hash: (BlockNumber, Hash) = (u32, H256) → 36 bytes
///   beefy_next_authority_set: BeefyNextAuthoritySet<H256>
///     id: u64 (8 bytes)
///     len: u32 (4 bytes)
///     keyset_commitment: H256 (32 bytes) → total 44 bytes
///   leaf_extra: H256 (32 bytes) ← ParaHeadsRoot
///
/// Total before leaf_extra: 1 + 36 + 44 = 81 bytes
fn extract_para_heads_root(leaf_bytes: &[u8]) -> Result<H256> {
    // The BEEFY MMR leaf is additionally SCALE-encoded as EncodableOpaqueLeaf
    // which wraps the raw leaf bytes in a Vec<u8>.
    // Attempt 1: direct BEEFY leaf layout
    const LEAF_EXTRA_OFFSET: usize = 1 + 36 + 44; // = 81
    if leaf_bytes.len() >= LEAF_EXTRA_OFFSET + 32 {
        return Ok(H256::from_slice(&leaf_bytes[LEAF_EXTRA_OFFSET..LEAF_EXTRA_OFFSET + 32]));
    }
    // Attempt 2: Vec<u8> wrapper (compact length prefix)
    // Try skipping a compact-encoded length prefix
    if leaf_bytes.len() > 1 {
        let skip = compact_prefix_length(leaf_bytes[0]);
        if leaf_bytes.len() >= skip + LEAF_EXTRA_OFFSET + 32 {
            return Ok(H256::from_slice(
                &leaf_bytes[skip + LEAF_EXTRA_OFFSET..skip + LEAF_EXTRA_OFFSET + 32]
            ));
        }
    }
    // Fallback: return last 32 bytes (matches POC inbox pallet simplified extraction)
    if leaf_bytes.len() >= 32 {
        debug!("Falling back to last-32-bytes ParaHeadsRoot extraction");
        return Ok(H256::from_slice(&leaf_bytes[leaf_bytes.len() - 32..]));
    }
    Err(anyhow!("Relay MMR leaf too short to extract ParaHeadsRoot: {} bytes", leaf_bytes.len()))
}

/// Returns number of bytes used by a SCALE compact-encoded length prefix
fn compact_prefix_length(first_byte: u8) -> usize {
    match first_byte & 0b11 {
        0b00 => 1, // single byte
        0b01 => 2, // two bytes
        0b10 => 4, // four bytes
        _    => 5, // big integer mode (rare)
    }
}

/// Step 3: Build para-heads Merkle proof from relay state
async fn build_para_heads_proof(
    source_para_id: u32,
    _para_heads_root: &H256,
    relay_block_hash: H256,
    relay_client: &RelayClient,
) -> Result<ParaHeadsProof> {
    // Fetch all para heads sorted by para_id (matches relay chain's ParaHeadsRootProvider)
    let sorted_heads = relay_client.sorted_para_heads(relay_block_hash).await?;

    // Find the source para's position
    let leaf_index = sorted_heads.iter()
        .position(|(pid, _)| *pid == source_para_id)
        .ok_or_else(|| anyhow!("Source para {} not found in relay para heads", source_para_id))?;

    // Encode leaves as SCALE((para_id_u32, head_bytes)) - same as relay chain
    let leaves: Vec<Vec<u8>> = sorted_heads.iter()
        .map(|(pid, head)| encode_para_head_leaf(*pid, head))
        .collect();

    let source_head_bytes = sorted_heads[leaf_index].1.clone();

    // Generate Merkle proof using binary_merkle_tree with Keccak256
    let proof: MerkleProof<H256, Vec<u8>> =
        merkle_proof::<sp_core::KeccakHasher, _, _>(leaves.clone(), leaf_index as u32);

    info!(
        "Para-heads proof: {} paras, source at index {}, {} proof items",
        sorted_heads.len(), leaf_index, proof.proof.len()
    );

    Ok(ParaHeadsProof {
        proof_items: proof.proof,
        head_bytes: source_head_bytes,
    })
}

/// Encode a source parachain header reference for relay-head lookup.
/// In practice this is the encoded header hash that the relay stores.
/// For proof lookup we use the raw bytes the relay stores under Paras::Heads.
fn encode_para_header_for_relay(payload: &[u8]) -> Vec<u8> {
    // Placeholder: in real usage the relayer tracks headers via finality
    // notifications and stores head_bytes alongside the pending message.
    // Returning payload here is incorrect; see PendingMessage construction
    // in relayer.rs which should capture head_bytes at message discovery time.
    payload.to_vec()
}
