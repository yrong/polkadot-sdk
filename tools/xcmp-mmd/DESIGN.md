# XCMP MMD - Design Document

## Overview

XCMP MMD (Merkle Mountain Range based cross-chain messaging) is a proof-of-concept for trustless cross-chain message delivery between parachains. It uses a three-tier cryptographic proof system that leverages the relay chain's BEEFY finality gadget and Merkle structures.

## Problem Statement

Current XCMP implementations rely on validators to deliver messages between parachains. This POC explores an alternative approach where:
- Messages are committed to cryptographic accumulators (MMRs)
- Off-chain relayers construct proofs of message inclusion
- Destination parachains verify proofs on-chain without trusting relayers

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
4. The relayer fetches an MMR proof for a specific relay block
5. The destination parachain verifies this proof against the relay MMR root (available via relay state proof)

**Data structure**:
- MMR leaf: BEEFY `MmrLeaf` (version, parent_hash, authority_set, **ParaHeadsRoot**)
- Proof: Vector of sibling hashes (Merkle path)
- Verification: `mmr-lib` calculates root from leaf + proof

**Security**: Relies on BEEFY finality - if 2/3+ validators signed the MMR root, the relay block is finalized.

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
Build Tier 1 proof (relay MMR)
  - Call mmr_generateProof RPC
  - Extract ParaHeadsRoot from BEEFY leaf
    ↓
Build Tier 2 proof (para-heads Merkle)
  - Fetch all para heads from relay state
  - Reconstruct Merkle proof
    ↓
Assemble MessageWithProof
    ↓
Sign and submit to destination
```

### 3. Verification (Destination Parachain)

```
Receive submit_xcmp_mmd(MessageWithProof)
    ↓
Read relay MMR root from relay state proof
    ↓
Verify Tier 1 (relay MMR proof)
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
- Para-heads Merkle proof: ~1-5 sibling hashes (32-160 bytes)
- Outbox MMR proof: ~5-15 sibling hashes (160-480 bytes)
- Source header: ~100-200 bytes
- Total: **~500-1200 bytes** per message

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

### Short-term
1. Batch multiple messages in one proof
2. WebSocket subscriptions instead of polling
3. Persistent relayer state (database)
4. Retry logic and error handling

### Long-term
1. Economic incentives for relayers (fee mechanism)
2. Proof compression (aggregate signatures, state proof compression)
3. Parallel proof construction (multiple relayers)
4. Integration with XCMP v3 (when available)
5. Support for external chains (via light client)

## References

- [Merkle Mountain Ranges](https://github.com/opentimestamps/opentimestamps-server/blob/master/doc/merkle-mountain-range.md)
- [BEEFY Finality Gadget](https://spec.polkadot.network/sect-finality#sect-grandpa-beefy)
- [Binary Merkle Trees](https://en.wikipedia.org/wiki/Merkle_tree)
- [XCMP Design](https://wiki.polkadot.network/docs/learn-xcm)
