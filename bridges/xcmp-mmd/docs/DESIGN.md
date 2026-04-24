# XCMP MMD - Design Document

## Overview

XCMP MMD (Merkle Mountain Range based cross-chain messaging) is a proof-of-concept for trustless cross-chain message delivery between parachains. It uses a three-tier cryptographic proof system that leverages the relay chain's BEEFY finality gadget and Merkle structures.

## Problem Statement

**HRMP** (Horizontal Relay-routed Message Passing) stores message payloads on the relay chain, which is expensive in terms of storage and execution costs.

**XCMP MMD** replaces that with:
- Payloads kept off the relay chain
- Messages proven by nested Merkle proofs anchored to relay commitments

### How MMD XCMP Replaces HRMP

**HRMP approach:**
- Relay chain acts as a payload mailbox (`HrmpChannelContents`)
- Receiver reads relay state proofs and prunes via watermarks

**MMD XCMP approach:**
- Relay chain acts as a commitment anchor (no payload storage)
- Receiver accepts payload + proof bundle, verifies it, then executes the XCM

### POC Semantics

This minimal POC has the following characteristics:
- **Unordered**: Messages can arrive in any order
- **Best-effort**: If nobody submits the proof bundle, nothing happens
- **No pruning**: Of message stores or MMRs
- **No receipts/acknowledgments**
- **No incentive mechanism** for relayers
- **Replay protection required**: Prevents executing the same proven message repeatedly

### Design Rationale

This POC uses a **single global append-only `XcmpOutboxMmr`** that commits all outbound messages across all destinations. This differs from the forum sketch which proposed one `XcmpMessageMMR` per channel plus an `XcmpChannelTree` over those roots.

Benefits of the single global MMR approach:
- Simpler implementation (one accumulator)
- Globally monotonic `mmr_leaf_index` serves as message nonce
- Parachain header digest carries only `XcmpOutboxMmrRoot`
- No per-block Merkle snapshot needed as primary commitment

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Relay Chain                              │
│  ┌──────────────┐         ┌─────────────────┐                  │
│  │  BEEFY MMR   │────────▶│ ParaHeadsRoot   │                  │
│  │  (pallet-mmr)│         │ (Merkle tree)   │                  │
│  └──────────────┘         └─────────────────┘                  │
│         │                          │                             │
│         │ Proof 1                  │ Proof 2                    │
│         ▼                          ▼                             │
└─────────────────────────────────────────────────────────────────┘
          │                          │
          │                          │
┌─────────▼──────────┐      ┌───────▼────────────┐
│  Source Para 1000  │      │  Dest Para 2000    │
│                    │      │                     │
│  ┌──────────────┐  │      │  ┌──────────────┐  │
│  │ Outbox MMR   │  │      │  │ Inbox Pallet │  │
│  │ (messages)   │  │      │  │ (verifier)   │  │
│  └──────────────┘  │      │  └──────────────┘  │
│         │          │      │                     │
│         │ Proof 3  │      │                     │
│         ▼          │      │                     │
│  ┌──────────────┐  │      │                     │
│  │ xmmd digest  │  │      │                     │
│  │ in header    │  │      │                     │
│  └──────────────┘  │      │                     │
└────────────────────┘      └─────────────────────┘
         │                           ▲
         │                           │
         └───────────────────────────┘
                  Relayer
           (off-chain proof builder)
```

## Three-Tier Proof System

The system uses three nested proofs to establish a chain of trust from the destination parachain back to the source parachain's committed messages.

### Tier 1: Relay MMR Proof

**Purpose**: Prove that a relay chain block containing the source parachain's header is finalized by BEEFY.

**How it works**:
1. The relay chain maintains a Merkle Mountain Range (MMR) of all blocks via `pallet-mmr`
2. BEEFY validators sign MMR roots, providing finality guarantees
3. Each BEEFY MMR leaf contains a `ParaHeadsRoot` in its `leaf_extra` field
4. The relayer fetches an MMR proof for a specific relay block (the "anchor" block)
5. The destination parachain verifies this proof against the relay MMR root (available via relay state proof)

**Ancestry Proof Mechanism**:

To handle the race condition where the destination's relay parent advances between proof generation and verification, the system uses **MMR ancestry proofs**:

- **Problem**: Relayer generates proof at relay block 100, but destination is now at relay block 105
- **Solution**: Include an ancestry proof showing that MMR root at block 100 is an ancestor of MMR root at block 105
- **Verification**: Destination uses `pallet_mmr::verify_ancestry_proof(mmr_root_105, ancestry_proof)` to derive `mmr_root_100`, then verifies the original proof against it

This eliminates timing constraints - the relayer can wait for the destination to advance, and proofs remain valid across multiple relay blocks.

**Data structure**:
- MMR leaf: BEEFY `MmrLeaf` (version, parent_hash, authority_set, **ParaHeadsRoot**)
- Proof: Vector of sibling hashes (Merkle path)
- Ancestry proof (optional): `AncestryProof { prev_peaks, prev_leaf_count, leaf_count, items }`
- Verification: `mmr-lib` calculates root from leaf + proof, with ancestry proof deriving historical root if needed

**Security**: Relies on BEEFY finality - if 2/3+ validators signed the MMR root, the relay block is finalized. Ancestry proofs are cryptographically verified using the MMR structure itself.

### Tier 2: Para-heads Merkle Proof

**Purpose**: Prove that the source parachain's header is included in the `ParaHeadsRoot` from Tier 1.

**How it works**:
1. The relay chain builds a binary Merkle tree of all parachain headers each block
2. Leaves are `SCALE((para_id: u32, head_bytes: Vec<u8>))` sorted by para_id
3. The tree root is stored in the BEEFY MMR leaf as `ParaHeadsRoot`
4. The relayer reconstructs the Merkle proof by fetching all para heads from relay state
5. The destination parachain verifies the source header is in the tree

**Data structure**:
- Leaf: SCALE-encoded `(para_id, header_bytes)` tuple
- Proof: Vector of sibling hashes (Merkle path)
- Tree: Binary Merkle tree with KeccakHasher
- Verification: `binary_merkle_tree::verify_proof`

**Security**: If the header is in the ParaHeadsRoot, and the ParaHeadsRoot is in the finalized BEEFY MMR, then the header is finalized.

### Tier 3: Outbox MMR Proof

**Purpose**: Prove that a specific message is committed in the source parachain's outbox MMR.

**How it works**:
1. The source parachain maintains an MMR of all outbound messages via `pallet-xcmp-mmd-outbox`
2. Each message is hashed and appended to the MMR as a leaf
3. The MMR root is deposited in the block header as a `PreRuntime("xmmd", ...)` digest
4. The relayer calls a runtime API to generate a proof for a specific message
5. The destination parachain extracts the MMR root from the verified source header (Tier 2) and verifies the message proof

**Data structure**:
- Leaf: `OutboxLeaf { dest: u32, payload_hash: H256 }`
- Proof: Vector of sibling hashes (Merkle path)
- MMR root: Stored in source header digest
- Verification: `mmr-lib` calculates root from leaf + proof

**Security**: If the message is in the outbox MMR, and the MMR root is in the finalized source header, then the message was committed by the source parachain.

## BEEFY and Relay Chain Dependencies

### BEEFY MMR Implementation

The relay chain's `pallet_beefy_mmr` is configured with:
- `LeafExtra = H256` set to `ParaHeadsRoot`
- `ParaHeadsRootProvider` computes Merkle root over `sorted_para_heads()`
  - Leaves are `SCALE((para_id: u32, head_bytes: Vec<u8>))` sorted by para_id
  - Uses `binary_merkle_tree` with `KeccakHasher`

This defines the exact proof format and hashing that the inbox verifier must match.

### Relay MMR Root Access

The destination parachain obtains the relay MMR root trustlessly via:

1. **Trust anchor**: `ValidationData.relay_parent_storage_root` (already verified in `set_validation_data`)
2. **Storage key**: `pallet_mmr::RootHash` at `twox_128("Mmr") ++ twox_128("RootHash")`
3. **Collator integration**: Runtime implements `KeyToIncludeInRelayProof` to include this key in the inherent relay proof
4. **Verification**: Inbox pallet reads the value from `RelayChainStateProof` (no extra proof in extrinsic)

**Important**: The storage key must match the relay runtime's pallet name (e.g., "Mmr" on Westend).

### Data Availability

**On-chain commitment**: Only `payload_hash = Keccak256(payload)` is committed to the outbox MMR.

**Payload retrieval**: The relayer obtains the full payload bytes from:
- **Source parachain archival state** (recommended for POC): Fetch `HrmpOutboundMessages` from `cumulus_pallet_parachain_system` storage at the source block hash
- Archive nodes can read historical state to recover the exact `Vec<u8>` bytes that were hashed

**Cryptographic binding**: The hash commitment is on-chain; payload bytes are a data-availability problem solved off-chain.

## Message Flow

### 1. Message Commitment (Source Parachain)

```
User submits XCM
    ↓
XcmpQueue enqueues message
    ↓
XcmpMmdOutbox wraps XcmpQueue
    ↓
On block finalization:
  - Hash message payload
  - Create OutboxLeaf { dest, payload_hash }
  - Append to outbox MMR
  - Deposit xmmd digest in header
```

### 2. Proof Construction (Off-chain Relayer)

```
Monitor source finalized headers
    ↓
Detect xmmd digest
    ↓
Fetch HRMP outbound messages
    ↓
Build Tier 3 proof (outbox MMR)
  - Call XcmpMmdOutboxApi::generate_outbox_proof
    ↓
Find relay block containing source header
  - Scan relay chain for matching para head
    ↓
Read destination's current relay parent
  - Stabilize ValidationData reading
    ↓
Build Tier 1 proof (relay MMR)
  - Call mmr_generateProof RPC (anchored at source inclusion block)
  - Extract ParaHeadsRoot from BEEFY leaf
    ↓
Build ancestry proof (if needed)
  - If dest relay parent > source inclusion block:
    - Call mmr_generateAncestryProof RPC
    - Proves source block MMR root is ancestor of current MMR root
    ↓
Build Tier 2 proof (para-heads Merkle)
  - Fetch all para heads from relay state
  - Reconstruct Merkle proof
    ↓
Assemble MessageWithProof
  - Include ancestry proof if generated
    ↓
Sign and submit to destination
```

### 3. Verification (Destination Parachain)

```
Receive submit_xcmp_mmd(MessageWithProof)
    ↓
Read relay MMR root from relay state proof
    ↓
Check relay anchor vs current relay parent
  - If anchor == current: use MMR root directly
  - If anchor < current: verify ancestry proof
    - Derive historical MMR root at anchor block
  - If anchor > current: reject (invalid)
    ↓
Verify Tier 1 (relay MMR proof)
  - Verify against derived/current MMR root
  - Extract ParaHeadsRoot from BEEFY leaf
    ↓
Verify Tier 2 (para-heads Merkle proof)
  - Verify source header is in ParaHeadsRoot
    ↓
Extract outbox MMR root from source header digest
    ↓
Verify Tier 3 (outbox MMR proof)
  - Verify message leaf is in outbox MMR
    ↓
Verify payload hash matches
    ↓
Check message not already seen (replay protection)
    ↓
Dispatch message to XcmpQueue
```

## Components

### Outbox Pallet (Source Parachain)

**Role**: Commit outbound messages to an MMR and publish the root in block headers.

**Key features**:
- Wraps `XcmpQueue` as `OutboundXcmpMessageSource`
- Maintains an append-only MMR of message leaves
- Deposits `PreRuntime("xmmd", XcmpMmdDigest)` in headers
- Provides runtime API for proof generation

**Storage**:
- `OutboxMmr` - The MMR accumulator
- `NumberOfLeaves` - Current MMR size

### Inbox Pallet (Destination Parachain)

**Role**: Verify three-tier proofs and dispatch verified messages.

**Key features**:
- Accepts `MessageWithProof` via `submit_xcmp_mmd` extrinsic
- Verifies all three proof tiers
- Tracks seen messages by `(source_para_id, mmr_leaf_index)`
- Dispatches verified messages to `XcmpQueue`

**Storage**:
- `SeenMessages` - Set of `(ParaId, u64)` for replay protection

### Relayer (Off-chain)

**Role**: Monitor source parachains and construct proofs for destination parachains.

**Key features**:
- Polls source finalized headers for `xmmd` digests
- Fetches HRMP outbound messages
- Constructs three-tier proof bundles
- Signs and submits extrinsics to destination

**Architecture**:
- Event loop: Poll source every 6 seconds
- Proof builder: Orchestrates three proof tiers
- RPC clients: Source, destination, relay chain
- Signer: SR25519 extrinsic signing (FRAME V2)

## Technical Specifications

### Hard Bounds

The POC enforces the following limits:
- `MAX_PAYLOAD_BYTES = 256 * 1024` (256 KiB) - Maximum message payload size
- `MAX_RELAY_MMR_PROOF_ITEMS = 128` - Maximum proof items for relay MMR (grows with relay chain age)
- `MAX_PARA_HEADS_PROOF_ITEMS = 128` - Maximum proof items for para-heads Merkle tree
- `MAX_OUTBOX_MMR_PROOF_ITEMS = 64` - Maximum proof items for outbox MMR

These bounds ensure:
- Predictable weight calculation
- Protection against DoS via oversized proofs
- Reasonable extrinsic size (~768 KiB total)

### Data Structures

**OutboxLeaf**:
```rust
struct OutboxLeaf {
    dest: u32,              // Destination para ID
    payload_hash: H256,     // Keccak256(payload)
}
```

**XcmpMmdDigest**:
```rust
struct XcmpMmdDigest {
    version: u8,
    root: H256,             // Outbox MMR root
}
// Deposited as: DigestItem::PreRuntime(*b"xmmd", SCALE(digest))
```

**MessageWithProof**:
```rust
struct MessageWithProof {
    source: ParaId,
    dest: ParaId,
    mmr_leaf_index: u64,
    relay_mmr_leaf_index: u64,
    payload: Vec<u8>,
    
    // Tier 1: Relay MMR proof
    relay_mmr_proof: Vec<H256>,
    relay_mmr_leaf: Vec<u8>,        // BEEFY MMR leaf
    relay_mmr_size: u64,
    relay_anchor_number: u32,
    relay_ancestry_proof: Option<AncestryProof<H256>>,
    
    // Tier 2: Para-heads Merkle proof
    para_heads_proof: Vec<H256>,
    source_head: Vec<u8>,           // Source header bytes
    para_head_index: u32,
    para_heads_count: u32,
    
    // Tier 3: Outbox MMR proof
    outbox_leaf: OutboxLeaf,
    outbox_mmr_proof: Vec<H256>,
    outbox_mmr_size: u64,
}
```

### Hashing and Encoding

- **Payload hash**: `Keccak256(payload_bytes)`
- **MMR merge**: `Keccak256(left_hash || right_hash)`
- **Para-heads leaves**: `SCALE((para_id: u32, head_bytes: Vec<u8>))` sorted by para_id
- **Binary Merkle tree**: Uses `KeccakHasher` (matches relay chain)

### Verification Guards

The inbox pallet enforces:
- `dest == SelfParaId` (message is for this parachain)
- `relay_mmr_proof` contains exactly 1 leaf at `relay_mmr_leaf_index`
- `outbox_mmr_proof` contains exactly 1 leaf at `mmr_leaf_index`
- `leaf.dest == dest && leaf.payload_hash == Keccak256(payload)`
- `!seen((source, mmr_leaf_index))` (replay protection)

## Security Properties

### Trustlessness

The destination parachain does not trust:
- The relayer (can only submit valid proofs)
- The source parachain (proofs are verified against finalized state)
- Individual validators (relies on BEEFY 2/3+ threshold)

### Replay Protection

Messages are identified by `(source_para_id, mmr_leaf_index)`. Once processed, the inbox pallet rejects duplicate submissions.

### Finality

Messages are only delivered after:
1. Source parachain block is finalized (included in relay chain)
2. Relay chain block is finalized (BEEFY signatures)
3. Proofs are verified on destination

### Censorship Resistance

Anyone can run a relayer. If one relayer fails or censors messages, others can submit the same proof.

## Performance Characteristics

### Latency

Typical message delivery time: **30-45 seconds**

Breakdown:
- Source para block production: ~6s
- Relay chain inclusion: ~6-12s
- BEEFY finality: ~12-18s
- Relayer proof construction: ~1-2s
- Destination para processing: ~6s

### Proof Size

Approximate sizes:
- Relay MMR proof: ~5-10 sibling hashes (160-320 bytes)
- Relay ancestry proof (when needed): ~3-8 items (96-256 bytes)
- Para-heads Merkle proof: ~1-5 sibling hashes (32-160 bytes)
- Outbox MMR proof: ~5-15 sibling hashes (160-480 bytes)
- Source header: ~100-200 bytes
- Total without ancestry: **~500-1200 bytes** per message
- Total with ancestry: **~600-1500 bytes** per message

Note: Ancestry proofs are only needed when the destination's relay parent advances between proof generation and submission, which is common in practice.

### Scalability

**Bottlenecks**:
- Para-heads proof size grows with number of parachains (log₂(N))
- Relay MMR proof size grows with relay chain age (log₂(blocks))
- Relayer must fetch all para heads from relay state

**Optimizations**:
- Batch multiple messages in one proof (share relay/para-heads proofs)
- Cache relay MMR proofs for recent blocks
- Use state proof compression

## Comparison to Alternatives

### vs. Validator-based XCMP

**Advantages**:
- Trustless (no reliance on validators to deliver)
- Censorship resistant (anyone can relay)
- Verifiable on-chain

**Disadvantages**:
- Higher latency (requires finality)
- Larger proof size
- Requires off-chain relayers

### vs. Light Client Bridges

**Advantages**:
- Leverages existing relay chain infrastructure
- No need to track validator set changes
- Simpler verification logic

**Disadvantages**:
- Only works between parachains (not external chains)
- Requires relay chain to maintain MMR and ParaHeadsRoot

## Future Improvements

1. Batch multiple messages in one proof
2. WebSocket subscriptions instead of polling
3. Persistent relayer state (database)
4. Retry logic and error handling
5. Economic incentives for relayers (fee mechanism)
6. Proof compression (aggregate signatures, state proof compression)
7. Parallel proof construction (multiple relayers)

## References

- [Merkle Mountain Ranges](https://github.com/opentimestamps/opentimestamps-server/blob/master/doc/merkle-mountain-range.md)
- [BEEFY Finality Gadget](https://spec.polkadot.network/sect-finality#sect-grandpa-beefy)
- [Binary Merkle Trees](https://en.wikipedia.org/wiki/Merkle_tree)
- [XCMP Design](https://wiki.polkadot.network/docs/learn-xcm)
