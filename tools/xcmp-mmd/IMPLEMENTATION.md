# XCMP MMD - Implementation Guide

## Overview

This document describes what was built, the code structure, and how to integrate the XCMP MMD system into a parachain runtime.

## Components Built

### 1. Outbox Pallet

**Location**: `cumulus/pallets/xcmp-mmd-outbox/`

**Purpose**: Maintains an MMR of outbound XCMP messages and publishes the root in block headers.

#### Key Files

- `src/lib.rs` - Main pallet logic
- `src/mmr.rs` - MMR implementation using `mmr-lib`
- `runtime-api/src/lib.rs` - Runtime API definitions

#### Key Types

```rust
/// Outbox MMR leaf
pub struct OutboxLeaf {
    pub dest: u32,              // Destination para ID
    pub payload_hash: H256,     // Keccak256(payload)
}

/// Digest deposited in block headers
pub struct XcmpMmdDigest {
    pub version: u8,
    pub root: H256,             // Outbox MMR root
}

/// Proof returned by runtime API
pub struct OutboxProof {
    pub leaf: OutboxLeaf,
    pub proof: Vec<H256>,       // MMR proof items
    pub mmr_size: u64,
}
```

#### Runtime API

```rust
decl_runtime_apis! {
    pub trait XcmpMmdOutboxApi {
        fn generate_outbox_proof(leaf_index: u64) -> Option<OutboxProof>;
        fn mmr_root() -> H256;
        fn mmr_leaf_count() -> u64;
    }
}
```

#### Configuration

```rust
pub trait Config: frame_system::Config {
    type OutboundXcmpMessageSource: XcmpMessageSource;
    type MaxPendingOutboxLeaves: Get<u32>;
}
```

### 2. Inbox Pallet

**Location**: `cumulus/pallets/xcmp-mmd-inbox/`

**Purpose**: Verifies three-tier proofs and dispatches messages to XcmpQueue.

#### Key Files

- `src/lib.rs` - Pallet logic and extrinsic
- `src/types.rs` - `MessageWithProof` definition
- `src/verification.rs` - Proof verification functions

#### Key Types

```rust
pub struct MessageWithProof {
    pub source: ParaId,
    pub dest: ParaId,
    pub mmr_leaf_index: u64,
    pub relay_mmr_leaf_index: u64,
    pub payload: Vec<u8>,
    
    // Tier 1: Relay MMR proof
    pub relay_mmr_proof: Vec<H256>,
    pub relay_mmr_leaf: Vec<u8>,        // BEEFY MMR leaf
    pub relay_mmr_size: u64,
    
    // Tier 2: Para-heads Merkle proof
    pub para_heads_proof: Vec<H256>,
    pub source_head: Vec<u8>,           // Source header bytes
    pub para_head_index: u32,           // Leaf index in tree
    pub para_heads_count: u32,          // Total leaves
    
    // Tier 3: Outbox MMR proof
    pub outbox_leaf: OutboxLeaf,
    pub outbox_mmr_proof: Vec<H256>,
    pub outbox_mmr_size: u64,
}
```

#### Extrinsic

```rust
#[pallet::call]
impl<T: Config> Pallet<T> {
    #[pallet::weight(Weight::from_parts(1_000_000, 0))]
    pub fn submit_xcmp_mmd(
        origin: OriginFor<T>,
        message: MessageWithProof,
    ) -> DispatchResult {
        ensure_signed(origin)?;
        // Verify three-tier proof
        // Dispatch to XcmpQueue
        Ok(())
    }
}
```

#### Verification Steps

**Step 1: Read relay MMR root**
```rust
let relay_mmr_root = verification::read_mmr_root_from_relay_proof::<T>()?;
```

**Step 2: Verify relay MMR proof**
```rust
let para_heads_root = verification::verify_relay_mmr_proof::<T>(
    relay_mmr_root,
    message.relay_mmr_leaf_index,
    message.relay_mmr_size,
    &message.relay_mmr_leaf,
    &message.relay_mmr_proof,
)?;
```

**Step 3: Verify para-heads Merkle proof**
```rust
verification::verify_para_heads_proof::<T>(
    para_heads_root,
    message.source.into(),
    &message.source_head,
    message.para_head_index,
    message.para_heads_count,
    &message.para_heads_proof,
)?;
```

**Step 4: Extract outbox MMR root from source header**
```rust
let source_header = verification::decode_source_header::<T>(&message.source_head)?;
let outbox_mmr_root = verification::extract_outbox_mmr_root::<T>(&source_header)?;
```

**Step 5: Verify outbox MMR proof**
```rust
verification::verify_outbox_mmr_proof::<T>(
    outbox_mmr_root,
    message.mmr_leaf_index,
    message.outbox_mmr_size,
    &message.outbox_leaf,
    &message.outbox_mmr_proof,
)?;
```

**Step 6: Verify payload and dispatch**
```rust
verification::verify_payload_hash::<T>(&message.payload, message.outbox_leaf.payload_hash)?;
ensure!(!SeenMessages::<T>::contains_key((message.source, message.mmr_leaf_index)));
T::XcmpMessageHandler::handle_xcmp_messages(...);
```

#### Configuration

```rust
pub trait Config: frame_system::Config + cumulus_pallet_parachain_system::Config {
    type XcmpMessageHandler: XcmpMessageHandler;
    type SelfParaId: Get<ParaId>;
    type MaxRelayMmrProofItems: Get<u32>;
    type MaxParaHeadsProofItems: Get<u32>;
    type MaxOutboxMmrProofItems: Get<u32>;
    type MaxPayloadBytes: Get<u32>;
}
```

### 3. Relayer

**Location**: `tools/xcmp-mmd/relayer/`

**Purpose**: Off-chain service that monitors source parachains and constructs proofs.

#### Architecture

```
main.rs          CLI, config loading, starts relayer
config.rs        TOML configuration parsing
types.rs         Shared types (MessageWithProof, etc.)
client.rs        JSON-RPC wrappers
  ├─ SubstrateClient    Raw RPC calls
  ├─ SourceClient       Source para operations
  ├─ RelayClient        Relay chain operations
  └─ DestClient         Destination para operations
relayer.rs       Main event loop
proof.rs         Three-tier proof construction
signer.rs        SR25519 extrinsic signing
```

#### Event Loop (relayer.rs)

```rust
pub async fn run(&self) -> Result<()> {
    let mut last_processed: Option<H256> = None;
    let mut submitted: HashSet<(u32, u64)> = HashSet::new();
    
    loop {
        // Poll source finalized head
        let head = self.source.inner.finalized_head().await?;
        
        if last_processed == Some(head) {
            sleep(Duration::from_secs(6)).await;
            continue;
        }
        
        // Discover messages at this block
        let messages = self.discover_messages(head).await?;
        
        // Relay each message
        for msg in messages {
            if !submitted.contains(&(msg.source_para_id, msg.mmr_leaf_index)) {
                self.relay_message(&msg).await?;
                submitted.insert((msg.source_para_id, msg.mmr_leaf_index));
            }
        }
        
        last_processed = Some(head);
        sleep(Duration::from_secs(6)).await;
    }
}
```

#### Proof Construction (proof.rs)

```rust
pub async fn build_message_with_proof(
    message: &PendingMessage,
    source_client: &SourceClient,
    relay_client: &RelayClient,
) -> Result<MessageWithProof> {
    // Step 1: Generate outbox MMR proof
    let outbox_proof = build_outbox_proof(message, source_client).await?;
    
    // Step 2: Find relay block containing source header
    let (relay_block_hash, relay_block_num) = relay_client
        .find_relay_block_for_source(message.source_para_id, &source_header_bytes)
        .await?;
    
    // Step 3: Generate relay MMR proof
    let relay_mmr_proof = build_relay_mmr_proof(
        relay_leaf_index,
        relay_block_num,
        relay_client
    ).await?;
    
    // Step 4: Generate para-heads Merkle proof
    let para_heads_proof = build_para_heads_proof(
        message.source_para_id,
        &relay_mmr_proof.para_heads_root,
        relay_block_hash,
        relay_client,
    ).await?;
    
    // Assemble MessageWithProof
    Ok(MessageWithProof { /* all fields */ })
}
```

#### Extrinsic Signing (signer.rs)

Implements FRAME V2 `TxExtension` signing for penpal runtime:

```rust
pub async fn build_signed_extrinsic(
    &self,
    message: &MessageWithProof,
    client: &SubstrateClient,
) -> Result<String> {
    // Fetch runtime metadata
    let genesis_hash = client.genesis_hash().await?;
    let (spec_version, tx_version) = client.runtime_version().await?;
    let nonce = client.account_nonce(&account_id).await?;
    
    // Build call bytes
    let mut call_bytes = vec![self.pallet_index, self.call_index];
    call_bytes.extend_from_slice(&message.encode());
    
    // Build explicit extensions
    let mut explicit = Vec::new();
    explicit.push(0x00u8);                          // Era::Immortal
    explicit.extend_from_slice(&Compact(nonce).encode());
    explicit.extend_from_slice(&Compact(0u128).encode()); // tip
    explicit.push(0x00u8);                          // asset = None
    explicit.push(0x00u8);                          // metadata hash = false
    
    // Build implicit extensions
    let mut implicit = Vec::new();
    implicit.extend_from_slice(&spec_version.to_le_bytes());
    implicit.extend_from_slice(&tx_version.to_le_bytes());
    implicit.extend_from_slice(&genesis_hash);
    implicit.extend_from_slice(&genesis_hash);      // Immortal era
    implicit.push(0x00u8);                          // metadata hash implicit
    
    // Sign payload
    let mut payload = Vec::new();
    payload.extend_from_slice(&call_bytes);
    payload.extend_from_slice(&explicit);
    payload.extend_from_slice(&implicit);
    
    let to_sign = if payload.len() > 256 {
        blake2_256(&payload).to_vec()
    } else {
        payload
    };
    
    let signature = self.pair.sign(&to_sign);
    
    // Assemble extrinsic
    let mut body = Vec::new();
    body.push(0x84u8);                              // V4 | signed
    body.push(0x00u8);                              // MultiAddress::Id
    body.extend_from_slice(&account_id);
    body.push(0x01u8);                              // Sr25519
    body.extend_from_slice(&signature.0);
    body.extend_from_slice(&explicit);
    body.extend_from_slice(&call_bytes);
    
    let mut extrinsic = Compact(body.len() as u64).encode();
    extrinsic.extend_from_slice(&body);
    
    Ok(format!("0x{}", hex::encode(extrinsic)))
}
```

#### Configuration (relayer.toml)

```toml
source_ws = "ws://127.0.0.1:9945"
dest_ws = "ws://127.0.0.1:9956"
relay_ws = "ws://127.0.0.1:9901"
source_para_id = 1000
dest_para_id = 2000
signer_seed = "//Alice"
lookback_blocks = 5
```

#### Environment Variables

```bash
XCMP_MMD_PALLET_INDEX=71    # XcmpMmdInbox position in construct_runtime!
XCMP_MMD_CALL_INDEX=0       # submit_xcmp_mmd call index
```

## Runtime Integration

### Step 1: Add Dependencies

**Cargo.toml**:
```toml
[dependencies]
cumulus-pallet-xcmp-mmd-outbox = { workspace = true }
cumulus-pallet-xcmp-mmd-outbox-runtime-api = { workspace = true }
cumulus-pallet-xcmp-mmd-inbox = { workspace = true }

[features]
std = [
    "cumulus-pallet-xcmp-mmd-outbox/std",
    "cumulus-pallet-xcmp-mmd-outbox-runtime-api/std",
    "cumulus-pallet-xcmp-mmd-inbox/std",
]
```

### Step 2: Configure Pallets

**lib.rs**:
```rust
parameter_types! {
    pub const MaxPendingOutboxLeaves: u32 = 1024;
    pub const MaxRelayMmrProofItems: u32 = 128;
    pub const MaxParaHeadsProofItems: u32 = 128;
    pub const MaxOutboxMmrProofItems: u32 = 64;
    pub const MaxPayloadBytes: u32 = 256 * 1024;
}

impl cumulus_pallet_xcmp_mmd_outbox::Config for Runtime {
    type OutboundXcmpMessageSource = XcmpQueue;
    type MaxPendingOutboxLeaves = MaxPendingOutboxLeaves;
}

impl cumulus_pallet_xcmp_mmd_inbox::Config for Runtime {
    type XcmpMessageHandler = XcmpQueue;
    type SelfParaId = parachain_info::Pallet<Runtime>;
    type MaxRelayMmrProofItems = MaxRelayMmrProofItems;
    type MaxParaHeadsProofItems = MaxParaHeadsProofItems;
    type MaxOutboxMmrProofItems = MaxOutboxMmrProofItems;
    type MaxPayloadBytes = MaxPayloadBytes;
}
```

### Step 3: Update ParachainSystem

```rust
impl cumulus_pallet_parachain_system::Config for Runtime {
    // ... existing config ...
    type OutboundXcmpMessageSource = XcmpMmdOutbox;  // Wrap XcmpQueue
}
```

### Step 4: Add to construct_runtime!

```rust
construct_runtime!(
    pub enum Runtime {
        // ... existing pallets ...
        XcmpMmdOutbox: cumulus_pallet_xcmp_mmd_outbox = 70,
        XcmpMmdInbox: cumulus_pallet_xcmp_mmd_inbox = 71,
    }
);
```

### Step 5: Implement Runtime APIs

**KeyToIncludeInRelayProof**:
```rust
impl cumulus_primitives_core::KeyToIncludeInRelayProof<Block> for Runtime {
    fn keys_to_prove() -> cumulus_primitives_core::RelayProofRequest {
        use cumulus_primitives_core::RelayStorageKey;
        use polkadot_primitives::well_known_keys;
        
        cumulus_primitives_core::RelayProofRequest {
            keys: vec![RelayStorageKey::Top(
                well_known_keys::MMR_ROOT_HASH.to_vec()
            )],
        }
    }
}
```

**XcmpMmdOutboxApi**:
```rust
impl cumulus_pallet_xcmp_mmd_outbox_runtime_api::XcmpMmdOutboxApi<Block> for Runtime {
    fn generate_outbox_proof(leaf_index: u64) -> Option<OutboxProof> {
        XcmpMmdOutbox::generate_proof(leaf_index).map(|(leaf, proof, mmr_size)|
            OutboxProof { leaf, proof, mmr_size }
        )
    }
    
    fn mmr_root() -> sp_core::H256 {
        XcmpMmdOutbox::get_mmr_root()
    }
    
    fn mmr_leaf_count() -> u64 {
        XcmpMmdOutbox::get_mmr_leaf_count()
    }
}
```

## Step 3 Implementation Details

Step 3 (para-heads Merkle proof verification) was the final piece to complete the three-tier proof system.

### Changes Made

**1. Added fields to MessageWithProof**:
- `source_head: Vec<u8>` - The actual source parachain header bytes
- `para_head_index: u32` - Position in the sorted para-heads tree
- `para_heads_count: u32` - Total number of parachains

**2. Implemented real verification**:

Before (placeholder):
```rust
pub fn verify_para_heads_proof<T>(
    para_heads_root: H256,
    _source_para_id: u32,
    para_heads_proof: &[H256],
) -> Result<Vec<u8>, Error<T>> {
    // Just checked proof is non-empty
    // Returned fake header
}
```

After (real verification):
```rust
pub fn verify_para_heads_proof<T>(
    para_heads_root: H256,
    source_para_id: u32,
    source_head: &[u8],
    para_head_index: u32,
    para_heads_count: u32,
    para_heads_proof: &[H256],
) -> Result<(), Error<T>> {
    // Leaf encoding matches relay chain
    let leaf: Vec<u8> = (source_para_id, source_head.to_vec()).encode();
    
    let valid = binary_merkle_tree::verify_proof::<sp_core::KeccakHasher, _, _>(
        &para_heads_root,
        para_heads_proof.iter().copied(),
        para_heads_count,
        para_head_index,
        &leaf,
    );
    
    if valid {
        Ok(())
    } else {
        Err(Error::<T>::InvalidParaHeadsProof)
    }
}
```

**3. Updated relayer proof construction**:

```rust
// Build para-heads Merkle proof
let sorted_heads = relay_client.sorted_para_heads(relay_block_hash).await?;
let leaf_index = sorted_heads.iter()
    .position(|(pid, _)| *pid == source_para_id)?;

let leaves: Vec<Vec<u8>> = sorted_heads.iter()
    .map(|(pid, head)| encode_para_head_leaf(*pid, head))
    .collect();

let proof = merkle_proof::<sp_core::KeccakHasher, _, _>(
    leaves,
    leaf_index as u32
);

Ok(ParaHeadsProof {
    proof_items: proof.proof,
    head_bytes: sorted_heads[leaf_index].1.clone(),
    leaf_index: leaf_index as u32,
    number_of_leaves: sorted_heads.len() as u32,
})
```

## Building

### Relay Chain Binary
```bash
cargo build -p polkadot --release
```

### Parachain Binary
```bash
cargo build -p polkadot-parachain-bin --release
```

### Relayer
```bash
cd tools/xcmp-mmd/relayer
SKIP_WASM_BUILD=1 cargo build --release
```

**Note**: `SKIP_WASM_BUILD=1` is required because the relayer has path dependencies into the polkadot-sdk workspace, which would trigger parachain WASM builds.

## Critical Configuration

### Relay Chain Flags

```bash
--enable-offchain-indexing=true  # REQUIRED for mmr_generateProof
--pruning archive                # Keeps MMR leaf data
```

Without `--enable-offchain-indexing`, the `mmr_generateProof` RPC returns empty proof items because MMR leaf data is stored in the offchain database.

### Source Parachain Collator Flags

```bash
--enable-offchain-indexing=true  # For serving proofs
--pruning=archive                # Keep historical data
```

Only the primary collator (serving RPC requests) needs these flags.

### Chain Spec

- Use `westend-local` for relay chain (BEEFY enabled by default)
- No need for `--beefy` flag (redundant)

## Known Limitations

This is a proof-of-concept with several limitations:

1. **HTTP polling** - Relayer uses HTTP JSON-RPC polling instead of WebSocket subscriptions
2. **Para-heads reconstruction** - Relayer fetches all para heads from relay state (not scalable)
3. **Limited relay block lookup** - Scans only last 100 relay blocks
4. **No persistent state** - Relayer tracks submitted messages in memory only
5. **Single destination** - Relayer configured for one source→dest pair
6. **No retry logic** - Failed submissions are logged but not retried
7. **Simplified BEEFY parsing** - Uses last 32 bytes as ParaHeadsRoot

## Production Considerations

For production use, consider:

1. **WebSocket subscriptions** - Use `subxt` or `jsonrpsee` for real-time updates
2. **Database** - Persistent storage for relayer state
3. **Multi-destination** - Support multiple para pairs
4. **Retry logic** - Exponential backoff for failures
5. **Metrics** - Prometheus metrics for monitoring
6. **Proper BEEFY decoding** - Use `beefy_primitives::MmrLeaf`
7. **Economic model** - Fee mechanism for relayers
8. **Permissionless** - Allow anyone to submit proofs
9. **Proof batching** - Multiple messages in one proof

## Files Reference

### Pallets
- `cumulus/pallets/xcmp-mmd-outbox/src/lib.rs`
- `cumulus/pallets/xcmp-mmd-outbox/src/mmr.rs`
- `cumulus/pallets/xcmp-mmd-outbox/runtime-api/src/lib.rs`
- `cumulus/pallets/xcmp-mmd-inbox/src/lib.rs`
- `cumulus/pallets/xcmp-mmd-inbox/src/types.rs`
- `cumulus/pallets/xcmp-mmd-inbox/src/verification.rs`

### Relayer
- `tools/xcmp-mmd/relayer/src/main.rs`
- `tools/xcmp-mmd/relayer/src/config.rs`
- `tools/xcmp-mmd/relayer/src/types.rs`
- `tools/xcmp-mmd/relayer/src/client.rs`
- `tools/xcmp-mmd/relayer/src/relayer.rs`
- `tools/xcmp-mmd/relayer/src/proof.rs`
- `tools/xcmp-mmd/relayer/src/signer.rs`
- `tools/xcmp-mmd/relayer/Cargo.toml`
- `tools/xcmp-mmd/relayer/relayer.toml`

### Runtime Integration
- `cumulus/parachains/runtimes/testing/penpal/Cargo.toml`
- `cumulus/parachains/runtimes/testing/penpal/src/lib.rs`

### Testing
- `tools/xcmp-mmd/zombienet/xcmp-mmd-poc.toml`
- `tools/xcmp-mmd/zombienet/e2e-test.sh`
