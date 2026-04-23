// RPC client wrappers for source, destination, and relay chain

use anyhow::{anyhow, Context, Result};
use codec::Decode;
use serde_json::Value;
use sp_core::H256;
use tracing::{debug, info};

use crate::types::{OutboxLeaf, OutboxProof, XcmpMmdDigest};

/// Raw JSON-RPC client for a substrate node
pub struct SubstrateClient {
    pub url: String,
    client: reqwest::Client,
    request_id: std::sync::atomic::AtomicU64,
}

impl SubstrateClient {
    pub async fn new(url: &str) -> Result<Self> {
        // For production, use subxt or jsonrpsee. For this POC, use HTTP JSON-RPC.
        // If url is ws://, convert to http:// for simplicity.
        let http_url = url.replace("ws://", "http://").replace("wss://", "https://");
        Ok(Self {
            url: http_url,
            client: reqwest::Client::new(),
            request_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Send a JSON-RPC request and return the result value
    pub async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.request_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp = self.client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {}", self.url))?;

        let json: Value = resp.json().await?;

        if let Some(err) = json.get("error") {
            return Err(anyhow!("RPC error from {}: {}", method, err));
        }

        Ok(json["result"].clone())
    }

    /// Get finalized head hash
    pub async fn finalized_head(&self) -> Result<H256> {
        let result = self.rpc_call("chain_getFinalizedHead", serde_json::json!([])).await?;
        let hash_str = result.as_str().ok_or_else(|| anyhow!("Expected string hash"))?;
        parse_h256(hash_str)
    }

    /// Get block header at a given hash
    pub async fn header(&self, hash: H256) -> Result<Value> {
        let result = self.rpc_call(
            "chain_getHeader",
            serde_json::json!([format!("0x{}", hex::encode(hash))]),
        ).await?;
        Ok(result)
    }

    /// Get block number from header
    pub async fn block_number(&self, hash: H256) -> Result<u32> {
        let header = self.header(hash).await?;
        let num_hex = header["number"].as_str()
            .ok_or_else(|| anyhow!("No 'number' in header"))?;
        let num = u32::from_str_radix(num_hex.trim_start_matches("0x"), 16)?;
        Ok(num)
    }

    /// Subscribe to finalized blocks - returns hashes as they arrive
    /// For POC simplicity, polls via HTTP rather than WS subscription
    pub async fn poll_finalized_head(&self) -> Result<H256> {
        self.finalized_head().await
    }

    /// Read raw storage at a key
    pub async fn storage(&self, key_hex: &str, at: Option<H256>) -> Result<Option<Vec<u8>>> {
        let at_param = at.map(|h| format!("0x{}", hex::encode(h)));
        let params = match at_param {
            Some(h) => serde_json::json!([key_hex, h]),
            None => serde_json::json!([key_hex]),
        };
        let result = self.rpc_call("state_getStorage", params).await?;
        match result.as_str() {
            None => Ok(None),
            Some(hex_data) => {
                let bytes = hex::decode(hex_data.trim_start_matches("0x"))?;
                Ok(Some(bytes))
            }
        }
    }

    /// Read decoded storage value using SCALE
    pub async fn storage_decoded<T: Decode>(&self, key_hex: &str, at: Option<H256>) -> Result<Option<T>> {
        match self.storage(key_hex, at).await? {
            None => Ok(None),
            Some(bytes) => {
                let decoded = T::decode(&mut &bytes[..])
                    .with_context(|| format!("Failed to SCALE-decode storage at {}", key_hex))?;
                Ok(Some(decoded))
            }
        }
    }

    /// Get all storage keys with a given prefix
    pub async fn storage_keys_paged(&self, prefix: &str, at: Option<H256>) -> Result<Vec<String>> {
        let at_param = at.map(|h| format!("0x{}", hex::encode(h)));
        let params = match at_param {
            Some(h) => serde_json::json!([prefix, 1000, null, h]),
            None => serde_json::json!([prefix, 1000, null]),
        };
        let result = self.rpc_call("state_getKeysPaged", params).await?;
        let keys = result.as_array()
            .ok_or_else(|| anyhow!("Expected array of keys"))?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        Ok(keys)
    }

    /// Call a runtime API method
    pub async fn call_runtime_api(&self, method: &str, params_hex: &str, at: Option<H256>) -> Result<Vec<u8>> {
        let at_param = at.map(|h| format!("0x{}", hex::encode(h)));
        let params = match at_param {
            Some(h) => serde_json::json!([method, params_hex, h]),
            None => serde_json::json!([method, params_hex]),
        };
        let result = self.rpc_call("state_call", params).await?;
        let hex_str = result.as_str().ok_or_else(|| anyhow!("Expected string result"))?;
        Ok(hex::decode(hex_str.trim_start_matches("0x"))?)
    }

    /// Generate MMR proof via relay chain's mmr_generateProof RPC
    /// Returns (leaf_bytes, proof_items, mmr_size, leaf_index)
    pub async fn mmr_generate_proof(
        &self,
        block_numbers: Vec<u64>,
        best_known_block: Option<u64>,
    ) -> Result<Value> {
        let block_nums_hex: Vec<String> = block_numbers.iter()
            .map(|n| format!("0x{:x}", n))
            .collect();
        let best_known = best_known_block.map(|n| format!("0x{:x}", n));

        let params = match best_known {
            Some(b) => serde_json::json!([block_nums_hex, b]),
            None => serde_json::json!([block_nums_hex]),
        };

        let result = self.rpc_call("mmr_generateProof", params).await?;
        debug!("mmr_generateProof result: {:?}", result);
        Ok(result)
    }

    /// Submit an extrinsic (hex-encoded SCALE bytes)
    pub async fn submit_extrinsic(&self, extrinsic_hex: &str) -> Result<H256> {
        let result = self.rpc_call(
            "author_submitExtrinsic",
            serde_json::json!([extrinsic_hex]),
        ).await?;
        let hash_str = result.as_str().ok_or_else(|| anyhow!("Expected tx hash string"))?;
        parse_h256(hash_str)
    }
}

fn parse_h256(hex_str: &str) -> Result<H256> {
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("Expected 32-byte hash, got {}", bytes.len()));
    }
    Ok(H256::from_slice(&bytes))
}

/// Source parachain client - reads messages and digests
pub struct SourceClient {
    pub inner: SubstrateClient,
    pub para_id: u32,
}

impl SourceClient {
    pub async fn new(url: &str, para_id: u32) -> Result<Self> {
        Ok(Self {
            inner: SubstrateClient::new(url).await?,
            para_id,
        })
    }

    /// Parse the XCMP MMD digest root from a block header's logs
    pub fn parse_xmmd_digest(&self, header: &Value) -> Option<H256> {
        let logs = header.get("digest")?.get("logs")?.as_array()?;
        for log in logs {
            // DigestItem::PreRuntime is encoded as {"PreRuntime": ["0x786d6d64", "0x..."]}
            if let Some(pre_runtime) = log.get("PreRuntime") {
                let arr = pre_runtime.as_array()?;
                if arr.len() < 2 { continue; }
                let engine_id = arr[0].as_str()?;
                // *b"xmmd" as hex = 786d6d64
                if engine_id.to_lowercase().contains("786d6d64") {
                    let data_hex = arr[1].as_str()?;
                    let data = hex::decode(data_hex.trim_start_matches("0x")).ok()?;
                    // Decode XcmpMmdDigest: version (u8) + root (H256)
                    if data.len() >= 33 {
                        let root = H256::from_slice(&data[1..33]);
                        return Some(root);
                    }
                }
            }
        }
        None
    }

    /// Compute the storage key for HrmpOutboundMessages
    /// Key: twox_128("ParachainSystem") ++ twox_128("HrmpOutboundMessages")
    pub fn hrmp_outbound_messages_key() -> String {
        use sp_core::hashing::twox_128;
        let mut key = Vec::new();
        key.extend_from_slice(&twox_128(b"ParachainSystem"));
        key.extend_from_slice(&twox_128(b"HrmpOutboundMessages"));
        format!("0x{}", hex::encode(key))
    }

    /// Fetch outbound HRMP messages at a given block
    pub async fn hrmp_outbound_messages(&self, at: H256) -> Result<Vec<(u32, Vec<u8>)>> {
        let key = Self::hrmp_outbound_messages_key();
        let raw = self.inner.storage(&key, Some(at)).await?;
        match raw {
            None => Ok(vec![]),
            Some(bytes) => {
                // SCALE decode Vec<OutboundHrmpMessage { recipient: u32, data: Vec<u8> }>
                let messages: Vec<(u32, Vec<u8>)> = Decode::decode(&mut &bytes[..])
                    .with_context(|| "Failed to decode HrmpOutboundMessages")?;
                Ok(messages)
            }
        }
    }

    /// Call XcmpMmdOutboxApi::generate_outbox_proof
    /// Returns OutboxProof if the leaf exists
    pub async fn generate_outbox_proof(&self, leaf_index: u64, at: H256) -> Result<Option<OutboxProof>> {
        use codec::Encode;
        let params = leaf_index.encode();
        let params_hex = format!("0x{}", hex::encode(&params));
        let result = self.inner.call_runtime_api(
            "XcmpMmdOutboxApi_generate_outbox_proof",
            &params_hex,
            Some(at),
        ).await?;
        let decoded: Option<(u64, u64, OutboxLeaf, Vec<H256>)> = Decode::decode(&mut &result[..])
            .with_context(|| "Failed to decode outbox proof from runtime API")?;
        Ok(decoded.map(|(leaf_index, mmr_size, leaf, proof_items)| OutboxProof {
            leaf_index,
            mmr_size,
            leaf,
            proof_items,
        }))
    }

    /// Get the current MMR leaf count from the source runtime
    pub async fn mmr_leaf_count(&self, at: H256) -> Result<u64> {
        use codec::Encode;
        let result = self.inner.call_runtime_api(
            "XcmpMmdOutboxApi_mmr_leaf_count",
            "0x",
            Some(at),
        ).await?;
        let count: u64 = Decode::decode(&mut &result[..])
            .with_context(|| "Failed to decode mmr_leaf_count")?;
        Ok(count)
    }
}

/// Relay chain client - generates MMR proofs and reads para heads
pub struct RelayClient {
    pub inner: SubstrateClient,
}

impl RelayClient {
    pub async fn new(url: &str) -> Result<Self> {
        Ok(Self {
            inner: SubstrateClient::new(url).await?,
        })
    }

    /// Storage key for parachains_paras::Heads(para_id)
    /// Key: twox_128("Paras") ++ twox_128("Heads") ++ twox_64_concat(para_id)
    pub fn para_heads_key(para_id: u32) -> String {
        use sp_core::hashing::{twox_128, twox_64};
        use codec::Encode;
        let mut key = Vec::new();
        key.extend_from_slice(&twox_128(b"Paras"));
        key.extend_from_slice(&twox_128(b"Heads"));
        // Twox64Concat: hash + raw value
        let encoded_id = para_id.encode();
        key.extend_from_slice(&twox_64(&encoded_id));
        key.extend_from_slice(&encoded_id);
        format!("0x{}", hex::encode(key))
    }

    /// Storage key prefix for all parachains_paras::Heads entries
    pub fn para_heads_prefix() -> String {
        use sp_core::hashing::twox_128;
        let mut key = Vec::new();
        key.extend_from_slice(&twox_128(b"Paras"));
        key.extend_from_slice(&twox_128(b"Heads"));
        format!("0x{}", hex::encode(key))
    }

    /// Fetch all para heads at a given relay block, sorted by para_id
    /// Returns Vec<(para_id, head_bytes)> sorted ascending by para_id
    pub async fn sorted_para_heads(&self, at: H256) -> Result<Vec<(u32, Vec<u8>)>> {
        let prefix = Self::para_heads_prefix();
        let keys = self.inner.storage_keys_paged(&prefix, Some(at)).await?;

        let mut heads = Vec::new();
        for key in &keys {
            if let Some(raw) = self.inner.storage(key, Some(at)).await? {
                // Extract para_id from the key (last 4 bytes of twox64concat suffix after prefix+8)
                let key_bytes = hex::decode(key.trim_start_matches("0x"))?;
                // prefix (32 bytes) + twox64 (8 bytes) + para_id (4 bytes)
                if key_bytes.len() >= 44 {
                    let para_id_bytes = &key_bytes[40..44];
                    let para_id = u32::from_le_bytes(para_id_bytes.try_into()?);
                    let head_bytes: Vec<u8> = Decode::decode(&mut &raw[..])
                        .with_context(|| format!("Failed to decode head for para {}", para_id))?;
                    heads.push((para_id, head_bytes));
                }
            }
        }

        heads.sort_by_key(|(para_id, _)| *para_id);
        info!("Fetched {} para heads from relay at {:?}", heads.len(), at);
        Ok(heads)
    }

    /// Find the relay block hash that included a given source block
    /// by scanning backward from the relay finalized head
    pub async fn find_relay_block_for_source(&self, source_para_id: u32, source_head: &[u8]) -> Result<Option<(H256, u32)>> {
        let relay_head = self.inner.finalized_head().await?;
        let relay_num = self.inner.block_number(relay_head).await?;

        // Scan back up to 100 relay blocks looking for the source head
        for i in 0..100u32 {
            let block_num = relay_num.saturating_sub(i);
            // For simplicity, get block hash by number
            let hash_result = self.inner.rpc_call(
                "chain_getBlockHash",
                serde_json::json!([format!("0x{:x}", block_num)]),
            ).await?;
            let hash_str = hash_result.as_str()
                .ok_or_else(|| anyhow!("Expected hash string"))?;
            let block_hash = parse_h256(hash_str)?;

            let key = Self::para_heads_key(source_para_id);
            if let Some(raw) = self.inner.storage(&key, Some(block_hash)).await? {
                let stored_head: Vec<u8> = Decode::decode(&mut &raw[..])?;
                if stored_head == source_head {
                    return Ok(Some((block_hash, block_num)));
                }
            }
        }
        Ok(None)
    }

    /// Get the MMR leaf index for a relay block number
    /// In pallet_mmr, leaf_index = block_number - first_mmr_block_number
    /// For simplicity, we assume leaf_index ≈ block_number
    pub fn relay_block_to_leaf_index(block_number: u32) -> u64 {
        // In practice, call mmr_leafCount at genesis to get the offset
        block_number.saturating_sub(1) as u64
    }
}

/// Destination parachain client - submits MessageWithProof
pub struct DestClient {
    pub inner: SubstrateClient,
    pub para_id: u32,
}

impl DestClient {
    pub async fn new(url: &str, para_id: u32) -> Result<Self> {
        Ok(Self {
            inner: SubstrateClient::new(url).await?,
            para_id,
        })
    }
}
