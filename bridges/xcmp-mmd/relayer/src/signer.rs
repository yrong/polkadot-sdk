// Extrinsic construction and SR25519 signing for the XCMP MMD relayer.
//
// Penpal runtime uses the V5 TransactionExtension model (FRAME V2).
// The signed extrinsic wire format is:
//
//   compact(length)
//   || version_byte (0x84 = V4 | signed)
//   || MultiAddress::Id (0x00 prefix + 32 bytes account)
//   || sig_scheme (0x01 = Sr25519)
//   || sig (64 bytes)
//   || encoded explicit extensions (see below)
//   || call bytes
//
// Penpal TxExtension tuple (in order):
//   AuthorizeCall          explicit: ()         implicit: ()
//   CheckNonZeroSender     explicit: ()         implicit: ()
//   CheckSpecVersion       explicit: ()         implicit: spec_version (u32 LE)
//   CheckTxVersion         explicit: ()         implicit: tx_version (u32 LE)
//   CheckGenesis           explicit: ()         implicit: genesis_hash (H256)
//   CheckMortality/Era     explicit: Era        implicit: block_hash(era_block) (H256)
//   CheckNonce             explicit: Compact(n) implicit: ()
//   CheckWeight            explicit: ()         implicit: ()
//   ChargeAssetTxPayment   explicit: (Compact(tip), Option<Location>=None)  implicit: ()
//   CheckMetadataHash      explicit: bool(false) implicit: Option<[u8;32]>=None (0x00)
//   SetOrigin              explicit: ()         implicit: ()
//   WeightReclaim          explicit: ()         implicit: ()
//
// The signing payload is:
//   call_bytes || all_explicit || all_implicit
// where all_implicit = spec_version || tx_version || genesis_hash || block_hash || 0x00

use anyhow::{anyhow, Result};
use codec::Encode;
use sp_core::{crypto::Pair, sr25519};

use crate::client::SubstrateClient;
use crate::types::MessageWithProof;

/// Signing context for submitting extrinsics to the destination chain.
pub struct ExtrinsicSigner {
    pair: sr25519::Pair,
    pallet_index: u8,
    call_index: u8,
}

impl ExtrinsicSigner {
    /// Create from a signer seed phrase (e.g. "//Alice" or a BIP-39 mnemonic).
    pub fn new(seed: &str) -> Result<Self> {
        let pair = sr25519::Pair::from_string(seed, None)
            .map_err(|e| anyhow!("Invalid signer seed '{}': {:?}", seed, e))?;

        // XcmpMmdInbox is index 71 in penpal's construct_runtime!
        // submit_xcmp_mmd is the first (and only) dispatchable = index 0
        let pallet_index: u8 = std::env::var("XCMP_MMD_PALLET_INDEX")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(71);
        let call_index: u8 = std::env::var("XCMP_MMD_CALL_INDEX")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(0);

        Ok(Self { pair, pallet_index, call_index })
    }

    pub fn public(&self) -> sr25519::Public {
        self.pair.public()
    }

    /// Build and sign a `submit_xcmp_mmd` extrinsic, returning hex-encoded bytes.
    pub async fn build_signed_extrinsic(
        &self,
        message: &MessageWithProof,
        client: &SubstrateClient,
    ) -> Result<String> {
        let genesis_hash = client.genesis_hash().await?;
        let (spec_version, tx_version) = client.runtime_version().await?;
        let account_id: [u8; 32] = self.pair.public().0;
        let nonce = client.account_nonce(&account_id).await?;

        // Call bytes
        let mut call_bytes = vec![self.pallet_index, self.call_index];
        call_bytes.extend_from_slice(&message.encode());

        // Explicit extension encoding (in TxExtension order):
        //   AuthorizeCall ()
        //   CheckNonZeroSender ()
        //   CheckSpecVersion ()
        //   CheckTxVersion ()
        //   CheckGenesis ()
        //   CheckMortality Era::Immortal = 0x00
        //   CheckNonce Compact(nonce)
        //   CheckWeight ()
        //   ChargeAssetTxPayment (Compact(0u128), None)
        //   CheckMetadataHash false (= 0x00)
        //   SetOrigin ()
        //   WeightReclaim ()
        let mut explicit = Vec::new();
        explicit.push(0x00u8);                          // Era::Immortal
        explicit.extend_from_slice(&codec::Compact(nonce).encode()); // nonce
        explicit.extend_from_slice(&codec::Compact(0u128).encode()); // tip
        explicit.push(0x00u8);                          // Option<Location> = None (asset)
        explicit.push(0x00u8);                          // CheckMetadataHash = false

        // Implicit extension data appended to the signing payload:
        //   spec_version (u32 LE)
        //   tx_version (u32 LE)
        //   genesis_hash (H256)
        //   block_hash (H256) — same as genesis for Immortal era
        //   CheckMetadataHash implicit = None = 0x00
        let mut implicit = Vec::new();
        implicit.extend_from_slice(&spec_version.to_le_bytes());
        implicit.extend_from_slice(&tx_version.to_le_bytes());
        implicit.extend_from_slice(&genesis_hash);
        implicit.extend_from_slice(&genesis_hash); // Immortal: block_hash == genesis
        implicit.push(0x00u8);                     // CheckMetadataHash implicit = None

        // Signing payload = call || explicit || implicit
        let mut payload = Vec::new();
        payload.extend_from_slice(&call_bytes);
        payload.extend_from_slice(&explicit);
        payload.extend_from_slice(&implicit);

        // If > 256 bytes, sign Blake2-256 hash instead (Substrate convention)
        let to_sign = if payload.len() > 256 {
            sp_core::hashing::blake2_256(&payload).to_vec()
        } else {
            payload
        };

        let signature = self.pair.sign(&to_sign);
        let sig_bytes: [u8; 64] = signature.0;

        // Assemble signed extrinsic body:
        //   version_byte || address || sig_scheme || sig || explicit || call
        let mut body = Vec::new();
        body.push(0x84u8);         // V4 | signed
        body.push(0x00u8);         // MultiAddress::Id prefix
        body.extend_from_slice(&account_id);
        body.push(0x01u8);         // Sr25519
        body.extend_from_slice(&sig_bytes);
        body.extend_from_slice(&explicit);
        body.extend_from_slice(&call_bytes);

        // Prepend compact-encoded byte length
        let mut extrinsic = codec::Compact(body.len() as u64).encode();
        extrinsic.extend_from_slice(&body);

        Ok(format!("0x{}", hex::encode(extrinsic)))
    }
}

// ─── Additional RPC helpers needed for signing ────────────────────────────────

impl SubstrateClient {
    /// Get the genesis block hash.
    pub async fn genesis_hash(&self) -> Result<[u8; 32]> {
        let result = self.rpc_call("chain_getBlockHash", serde_json::json!([0u32])).await?;
        parse_hash_bytes(result.as_str().ok_or_else(|| anyhow!("Expected string genesis hash"))?)
    }

    /// Get (spec_version, transaction_version) from state_getRuntimeVersion.
    pub async fn runtime_version(&self) -> Result<(u32, u32)> {
        let result = self.rpc_call("state_getRuntimeVersion", serde_json::json!([])).await?;
        let spec = result["specVersion"].as_u64()
            .ok_or_else(|| anyhow!("Missing specVersion"))? as u32;
        let tx = result["transactionVersion"].as_u64()
            .ok_or_else(|| anyhow!("Missing transactionVersion"))? as u32;
        Ok((spec, tx))
    }

    /// Get the next nonce for an account (system_accountNextIndex).
    /// `account` is the raw 32-byte AccountId.
    pub async fn account_nonce(&self, account: &[u8; 32]) -> Result<u64> {
        // Pass as 0x-prefixed hex; nodes accept raw AccountId32 hex for this RPC
        let hex = format!("0x{}", hex::encode(account));
        let result = self.rpc_call("system_accountNextIndex", serde_json::json!([hex])).await?;
        result.as_u64().ok_or_else(|| anyhow!("Expected u64 nonce, got: {}", result))
    }
}

fn parse_hash_bytes(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("Expected 32-byte hash, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}
