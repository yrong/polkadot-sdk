// Proof construction for XCMP MMD relayer
//
// Builds all three proof tiers:
//   1. Outbox MMR proof (from source runtime API)
//   2. Relay MMR proof (from relay chain mmr_generateProof RPC)
//   3. Para-heads Merkle proof (reconstructed from relay state)

use anyhow::{anyhow, Context, Result};
use binary_merkle_tree::{merkle_proof, MerkleProof};
use sp_core::H256;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

use crate::client::{DestClient, RelayClient, SourceClient};
use crate::types::{
    encode_para_head_leaf, MessageWithProof, OutboxProof, ParaHeadsProof,
    PendingMessage, RelayMmrProof,
};

/// Rounds: stabilize VFP, then re-check after the full proof bundle; outer loop if VFP drifts.
const PROOF_BUNDLE_REBUILD_ATTEMPTS: usize = 20;
/// Back-to-back reads of dest `ValidationData` until stable.
const DEST_VFP_STABILIZE_ROUNDS: usize = 20;

/// Build the complete MessageWithProof for a pending message.
pub async fn build_message_with_proof(
    message: &PendingMessage,
    source_client: &SourceClient,
    relay_client: &RelayClient,
    dest_client: &DestClient,
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

    for bundle_attempt in 0..PROOF_BUNDLE_REBUILD_ATTEMPTS {
        // Anchor to dest ValidationData: two consecutive reads agree (suggestion 1).
        let (_dest_at, stable_vd) = dest_client
            .stabilized_persisted_validation_data(DEST_VFP_STABILIZE_ROUNDS)
            .await
            .with_context(|| "stabilize dest ParachainSystem::ValidationData")?;
        let anchor_num = stable_vd.relay_parent_number;
        if anchor_num < relay_block_num {
            return Err(anyhow!(
                "Destination relay parent #{} is behind relay inclusion block #{}: wait and retry",
                anchor_num,
                relay_block_num
            ));
        }
        let anchor_hash = relay_client
            .inner
            .block_hash_by_number(anchor_num)
            .await
            .with_context(|| "chain_getBlockHash(anchor) for MMR proof")?;

        // Step 3: Generate relay MMR proof
        let relay_mmr_proof = build_relay_mmr_proof(
            relay_leaf_index,
            relay_block_num,
            relay_client,
            anchor_num,
            anchor_hash,
        )
        .await?;

        // Step 4: Generate para-heads Merkle proof
        let para_heads_proof = build_para_heads_proof(
            message.source_para_id,
            &relay_mmr_proof.para_heads_root,
            relay_block_hash,
            relay_client,
        )
        .await?;

        // Step 5: Generate ancestry proof if anchor != relay_block_num
        let relay_ancestry_proof = if anchor_num == relay_block_num {
            // Proof anchored at same block - no ancestry proof needed
            None
        } else {
            // Proof anchored at newer block - generate ancestry proof
            Some(build_relay_ancestry_proof(
                relay_block_num,
                anchor_num,
                anchor_hash,
                relay_client,
            ).await?)
        };

        let mwp = MessageWithProof {
            source: message.source_para_id,
            dest: message.dest_para_id,
            mmr_leaf_index: message.mmr_leaf_index,
            relay_mmr_leaf_index: relay_leaf_index,
            payload: message.payload.clone(),
            relay_mmr_proof: relay_mmr_proof.proof_items,
            relay_mmr_leaf: relay_mmr_proof.leaf_bytes,
            relay_mmr_size: relay_mmr_proof.mmr_size,
            relay_anchor_number: relay_block_num,
            relay_ancestry_proof,
            para_heads_proof: para_heads_proof.proof_items,
            source_head: para_heads_proof.head_bytes,
            para_head_index: para_heads_proof.leaf_index,
            para_heads_count: para_heads_proof.number_of_leaves,
            outbox_leaf: outbox_proof.leaf.clone(),
            outbox_mmr_proof: outbox_proof.proof_items.clone(),
            outbox_mmr_size: outbox_proof.mmr_size,
        };

        // Re-read before returning: if dest VFP moved while we built, rebuild (suggestion 1).
        let head_after = dest_client.inner.finalized_head().await?;
        let vd_after = dest_client
            .persisted_validation_data(Some(head_after))
            .await
            .with_context(|| "re-read dest ValidationData after proof bundle")?;
        if vd_after.relay_parent_number == stable_vd.relay_parent_number
            && vd_after.relay_parent_storage_root == stable_vd.relay_parent_storage_root
        {
            return Ok(mwp);
        }
        warn!(
            bundle_attempt,
            "dest ValidationData changed after building proof (relay parent {:?} -> {:?}); rebuilding",
            stable_vd.relay_parent_number,
            vd_after.relay_parent_number
        );
        sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!(
        "Could not produce a MessageWithProof consistent with dest ValidationData after {} bundle attempts",
        PROOF_BUNDLE_REBUILD_ATTEMPTS
    ))
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
    anchor_num: u32,
    anchor_hash: H256,
) -> Result<RelayMmrProof> {
    // `anchor_num` / `anchor_hash` come from the stabilized dest `ParachainSystem::ValidationData`
    // (see `stabilized_persisted_validation_data` + `read_mmr_root` in the inbox).

    let result = relay_client
        .inner
        .mmr_generate_proof(
            vec![relay_block_num as u64],
            Some(anchor_num as u64),
            Some(anchor_hash),
        )
        .await
        .with_context(|| "mmr_generateProof")?;

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
        leaf_index: leaf_index as u32,
        number_of_leaves: sorted_heads.len() as u32,
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

/// Step 5: Generate relay MMR ancestry proof
async fn build_relay_ancestry_proof(
    prev_block_num: u32,
    current_block_num: u32,
    current_block_hash: H256,
    relay_client: &RelayClient,
) -> Result<crate::types::AncestryProof> {
    let result = relay_client
        .inner
        .mmr_generate_ancestry_proof(
            prev_block_num as u64,
            Some(current_block_num as u64),
            Some(current_block_hash),
        )
        .await
        .with_context(|| "mmr_generateAncestryProof")?;

    // Parse the mmr_generateAncestryProof response
    // Response: { prevPeaks: [hex], prevLeafCount: hex, leafCount: hex, items: [[index, hash], ...] }
    let prev_peaks = result["prevPeaks"].as_array()
        .ok_or_else(|| anyhow!("Expected 'prevPeaks' array in mmr_generateAncestryProof response"))?;
    let prev_peaks: Vec<H256> = prev_peaks.iter()
        .map(|peak| {
            let hex_str = peak.as_str().unwrap_or("0x");
            let bytes = hex::decode(hex_str.trim_start_matches("0x")).unwrap_or_default();
            if bytes.len() == 32 {
                H256::from_slice(&bytes)
            } else {
                H256::default()
            }
        })
        .collect();

    let prev_leaf_count = result["prevLeafCount"].as_str()
        .ok_or_else(|| anyhow!("Expected 'prevLeafCount' in ancestry proof"))?;
    let prev_leaf_count = u64::from_str_radix(prev_leaf_count.trim_start_matches("0x"), 16)?;

    let leaf_count = result["leafCount"].as_str()
        .ok_or_else(|| anyhow!("Expected 'leafCount' in ancestry proof"))?;
    let leaf_count = u64::from_str_radix(leaf_count.trim_start_matches("0x"), 16)?;

    let items = result["items"].as_array()
        .ok_or_else(|| anyhow!("Expected 'items' in ancestry proof"))?;
    let items: Vec<(u64, H256)> = items.iter()
        .filter_map(|item| {
            let arr = item.as_array()?;
            if arr.len() != 2 {
                return None;
            }
            let index_str = arr[0].as_str()?;
            let index = u64::from_str_radix(index_str.trim_start_matches("0x"), 16).ok()?;
            let hash_str = arr[1].as_str()?;
            let bytes = hex::decode(hash_str.trim_start_matches("0x")).ok()?;
            if bytes.len() == 32 {
                Some((index, H256::from_slice(&bytes)))
            } else {
                None
            }
        })
        .collect();

    info!(
        "Ancestry proof: prev_block={}, current_block={}, {} prev_peaks, {} items",
        prev_block_num, current_block_num, prev_peaks.len(), items.len()
    );

    Ok(crate::types::AncestryProof {
        prev_peaks,
        prev_leaf_count,
        leaf_count,
        items,
    })
}
