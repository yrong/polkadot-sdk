# Speculative Messaging vs XCMP MMD: Comparative Analysis

**Date**: 2026-05-06  
**Author**: Analysis based on design documents  
**Related Documents**:
- [Speculative Messaging Design](speculative-messaging-design.md)
- [Speculative Messaging Implementation Design](speculative-messaging-impl-design.md)
- [XCMP MMD Minimal POC](xcmp-mmd-minimal-poc.md)

---

## Executive Summary

This document compares two approaches to cross-chain messaging in Polkadot: **XCMP MMD** (Merkle Mountain Range based messaging) and **Speculative Messaging**. While both use MMR-based commitments and off-chain message passing, they represent fundamentally different architectural philosophies.

One important scoping note: the current
`speculative-messaging-impl-design.md` is intentionally narrowed to a
**minimal happy-path inclusion-based POC**. Some of the lower-latency
acknowledgement and trust-domain ideas discussed here remain part of the
longer-term design, but are explicitly out of scope for the current Phase 1
implementation document.

At a high level:

- **XCMP MMD**: A trustless, finality-based bridge protocol using cryptographic proofs
- **Speculative Messaging**: A native messaging architecture whose current POC is inclusion-based, with later acknowledgement/trust-domain extensions

The key difference is **trust model and latency tradeoff**: XCMP MMD prioritizes trustlessness with ~30-45 second latency, while Speculative Messaging aims to move from an inclusion-based POC (~6-12 seconds) toward lower-latency acknowledgement-based operation in later phases.

---

## Main Differences

### 1. Trust and Verification Models

#### XCMP MMD Approach

**Philosophy**: Trustless verification through finality proofs

**Characteristics**:
- **Finality-based**: Messages only delivered after BEEFY finality (~30-45 seconds)
- **Three-tier cryptographic proof system**:
  1. **Tier 1**: Relay MMR proof (BEEFY signatures prove relay block finality)
  2. **Tier 2**: Para-heads Merkle proof (source header in finalized relay state)
  3. **Tier 3**: Outbox MMR proof (message in source parachain's committed MMR)
- **Trustless**: Receiver verifies everything against finalized relay chain state
- **Best-effort delivery**: Relies on off-chain relayers to submit proofs
- **Censorship resistant**: Anyone can run a relayer

**Trust assumptions**:
- BEEFY finality (2/3+ validators)
- Relay chain state correctness
- No trust in relayers (they can only submit valid proofs)

#### Speculative Messaging Approach

**Philosophy**: Native coordination through relay chain commitment matching

**Characteristics**:
- **Current POC**: Inclusion-based messaging with off-chain message transfer and on-chain commitment matching
- **Commitment matching on relay chain**: Relay chain verifies `provides` roots match `requires` roots at inclusion time
- **Hierarchical MMR structure**: Per-destination MMRs with top-level Merkle commitment
- **Phase 2+ direction**: Trust domains, acknowledgements, and super-chain style optimizations
- **Late block proofs**: Planned follow-up for sender/receiver timing mismatches; not part of the minimal POC
- **Message data stays off relay chain**: Only commitments verified on-chain

**Trust assumptions**:
- Relay chain coordination (always required)
- Current POC: relay chain inclusion only
- Future trust-domain mode: collator acknowledgements (slashable if dishonest)

### 2. Latency Comparison

| Approach | Latency | Bottleneck |
|----------|---------|------------|
| **HRMP (current)** | 12-18+ seconds | Relay chain storage + state lookup |
| **XCMP MMD** | 30-45 seconds | BEEFY finality + proof construction |
| **Speculative (Phase 1 POC)** | 6-12 seconds | Relay chain inclusion only |
| **Speculative (acknowledged)** | 6-12 seconds | Parachain block time + acknowledgement |
| **Speculative (super-chain)** | < 6 seconds | Intra-block messaging |

### 3. Data Structures

#### XCMP MMD

**Single global MMR**:
```rust
// One append-only MMR for all destinations
OutboxLeaf {
    dest: u32,
    payload_hash: H256,
}

// Proof bundle includes all three tiers
MessageWithProof {
    // Tier 1: Relay MMR
    relay_mmr_proof: Vec<H256>,
    relay_mmr_leaf: Vec<u8>,  // BEEFY MMR leaf
    
    // Tier 2: Para-heads Merkle
    para_heads_proof: Vec<H256>,
    source_head: Vec<u8>,
    
    // Tier 3: Outbox MMR
    outbox_mmr_proof: Vec<H256>,
    outbox_leaf: OutboxLeaf,
    
    payload: Vec<u8>,
}
```

**Benefits**:
- Simpler implementation (one accumulator)
- Globally monotonic `mmr_leaf_index` serves as message nonce
- Single root in parachain header digest

**Drawbacks**:
- Proof size grows with total message volume (not per-destination)
- No optimization for high-volume destinations

#### Speculative Messaging

**Hierarchical MMR structure**:
```rust
// Top-level Merkle tree over per-destination MMR roots
ProvidesCommitment {
    root: Hash,  // Merkle root over all per-destination MMR roots
}

// Per-destination MMR (internal to parachain)
OutgoingMessageState {
    per_destination: BTreeMap<ParaId, MMR>,
    current_root: Hash,
}

// Off-chain message batch
MessageBatch {
    provides_root: Hash,
    subtree_root: Hash,  // Per-destination MMR root
    subtree_inclusion_proof: Vec<Hash>,  // Prove subtree in top-level
    messages: Vec<OutgoingMessage>,
}
```

**Benefits**:
- Proof size: O(log D + log m) where D = destinations, m = messages to receiver
- Receiver only proves their subtree
- Late block proofs only grow with messages to specific receiver
- High volume to other chains doesn't affect proof size

**Drawbacks**:
- More complex implementation
- Requires per-destination state management

### 4. Relay Chain Integration

#### XCMP MMD

**Relay chain role**: Passive finality provider

- No changes to relay chain runtime
- Uses existing `pallet-mmr` and BEEFY
- Relay chain unaware of message passing
- Verification happens entirely on destination parachain

**Destination anchor mechanism**:
- Destination caches `(relay_parent_number, mmr_root)` in `LatestRelayMmr`
- Relayer must prove against this cached anchor
- Limits flexibility (can only verify against cached relay state)

#### Speculative Messaging

**Relay chain role**: Active coordinator

- **New storage**: `ProvidesRoots<ParaId, Hash>` tracks latest provides per chain
- **New validation logic**: In `process_candidates()`:
  1. Pre-collect all provides from current block
  2. For each candidate, verify all `requires` match available `provides`
  3. Update `ProvidesRoots` after inclusion
- **New error**: `UnsatisfiedRequires` if matching fails
- **Candidate commitments extended**: Add `provides` and `requires` fields

**Benefits**:
- Relay chain enforces message dependencies
- Enables speculative execution (build against unfinalized provides)
- Natural integration with parachain scheduling

**Drawbacks**:
- Requires relay chain runtime changes
- Adds complexity to inclusion logic

### 5. Message Ordering and Delivery

#### XCMP MMD

- **Unordered**: Messages can arrive in any order
- **Best-effort**: If nobody submits proof, nothing happens
- **No acknowledgements**: Fire-and-forget from sender perspective
- **Replay protection**: Destination tracks `(source, mmr_leaf_index)` to prevent duplicates
- **No pruning**: POC doesn't prune message stores or MMRs

#### Speculative Messaging

- **Ordered per source**: Receiver tracks `last_processed` position per source
- **Current POC delivery model**: Inclusion-based only; receiver candidates require matching source `provides`
- **Acknowledgement-based mode**: Future extension, not part of the minimal implementation doc
- **Late block proof handling**: Future extension, not part of the minimal implementation doc
- **Pruning**: Explicitly deferred from the minimal POC

### 6. Security Properties

#### XCMP MMD

**Strengths**:
- Fully trustless (only relies on BEEFY finality)
- Censorship resistant (anyone can relay)
- No new slashing conditions
- Works with any relay chain that has BEEFY

**Weaknesses**:
- High latency makes it unsuitable for time-sensitive applications
- Relayer incentive problem (no built-in fee mechanism)
- Destination must cache relay state (limits flexibility)

#### Speculative Messaging

**Strengths**:
- Removes message data from relay chain state in the Phase 1 design
- Relay chain enforces message dependencies
- Future phases can add lower-latency and trust-domain tradeoffs

**Weaknesses**:
- Full design is more complex than XCMP MMD
- Production-grade operation still needs late block proofs, pruning, and rate limits
- Future trust-domain / acknowledgement mode adds governance and security complexity

---

## Conceptual Positioning

### XCMP MMD: Trustless Bridge Protocol

XCMP MMD is essentially a **light client bridge** between parachains:
- Uses finality proofs (like Ethereum light clients)
- Trustless verification at the cost of latency
- Similar to cross-chain bridges (Snowbridge, IBC)
- Best for: Cross-domain messaging where trust is unavailable

### Speculative Messaging: Native Messaging Protocol

Speculative Messaging is a **native coordination mechanism**:
- Leverages relay chain's existing coordination role
- Current POC uses inclusion-based relay-chain matching
- Later phases can add trust/latency tradeoffs through acknowledgements and domains
- Replaces HRMP with a more scalable alternative
- Best long-term fit: High-frequency messaging once the later phases are added

---

## Feasibility Assessment

### Is Speculative Messaging Implementation Doable?

**Yes, but with significant complexity.** The implementation can be phased:

#### ✅ Feasible Components

1. **Basic MMR infrastructure**
   - Already exists: `pallet-mmr`, `sp-mmr-primitives`
   - Well-understood data structures

2. **Candidate commitments extension**
   - Straightforward addition to `CandidateCommitments`
   - Similar to existing fields (upward_messages, horizontal_messages)

3. **Relay chain matching logic**
   - The "happy path" in impl design is relatively simple
   - Pre-collect provides, verify requires, update storage

4. **Per-destination MMR structure**
   - Well-defined data structures
   - Clear verification logic

#### ⚠️ Complex Components

1. **Late Block Proofs** (explicitly excluded from impl design)
   - **What**: Prove that old `requires` are still valid under new `provides`
   - **Why needed**: Sender/receiver blocks arrive at different times
   - **Complexity**: 
     - MMR extension proof verification in PVF
     - Commitment transformation logic
     - Similar to Low-Latency v2's scheduling parent header chains
   - **Critical for production**: Without this, messages fail if blocks don't arrive simultaneously

2. **Trust Domain Configuration**
   - Needs governance/configuration mechanism
   - Cross-domain fallback logic
   - Security implications of trust relationships
   - Who decides which chains are in a trust domain?

3. **Acknowledgement Extensions** (from Low-Latency v2)
   - **Dependency**: Relevant only after the Phase 1 inclusion-based POC
   - Extended rules for message dependencies
   - Slashing conditions for invalid acknowledgements
   - Collator protocol changes

4. **Super Chains**
   - Coordinated multi-chain block production
   - Atomic inclusion/availability (all or nothing)
   - Cycle handling in intra-block messaging
   - Requires identical collator sets

#### 🔴 Missing Prerequisites

1. **Low-Latency Parachains v2** (for later acknowledgement-based phases, not Phase 1)
   - Speculative messaging's later fast-path builds on acknowledgement signatures
   - Decoupling of scheduling from relay parent helps the full design
   - The current Phase 1 inclusion-based POC does **not** require Low-Latency v2

2. **BEEFY finality** (for cross-domain)
   - Already exists but needs production-ready status
   - Required for trustless cross-domain messaging

3. **Collator protocol extensions**
   - New request/response protocols for off-chain message exchange
   - Message batch propagation
   - Future acknowledgement propagation for later phases

### Recommended Implementation Path

Based on the impl design document's scope (happy path only):

#### Phase 1: Inclusion-Based Messaging (Minimal POC Scope)

**What**:
- Implement basic provides/requires matching
- Per-destination MMR structure
- Off-chain message exchange between collators
- Relay chain commitment verification

**Latency**: ~1-2 relay blocks (~6-12 seconds)

**Benefit**: 
- Removes message data from relay chain state
- Proves the architecture works
- Foundation for later phases

**Complexity**: Medium

**Timeline**: 3-6 months (with existing MMR infrastructure)

#### Phase 2: Late Block Proofs

**What**:
- MMR extension proof verification
- PVF commitment transformation
- Handle sender/receiver timing mismatches

**Enables**: 
- Robust operation with older relay parents
- Works with Low-Latency v2's finalized relay parents

**Complexity**: High

**Timeline**: 2-4 months

#### Phase 3: Acknowledgement-Based (Requires Low-Latency v2)

**What**:
- Trust domain configuration
- Extended acknowledgement rules
- Slashing for invalid acknowledgements

**Latency**: ~1-2 parachain blocks within trust domains

**Benefit**: 
- Real low-latency messaging
- Competitive with monolithic chains

**Complexity**: Very High

**Timeline**: 4-6 months (after Low-Latency v2)

#### Phase 4: Super Chains

**What**:
- Coordinated block production
- Intra-block messaging
- Atomic inclusion

**Latency**: < 1 block for tightly coupled chains

**Benefit**: 
- Enables "sharded monolithic chain" experience
- Horizontal scaling with vertical integration feel

**Complexity**: Very High

**Timeline**: 3-6 months

### Total Timeline Estimate

- **Phase 1 only**: 3-6 months
- **Phase 1 + 2**: 6-10 months
- **Full implementation (all phases)**: 18-24 months

---

## Comparison Matrix

| Aspect | XCMP MMD | Speculative Messaging (Phase 1) | Speculative Messaging (Full) |
|--------|----------|--------------------------------|------------------------------|
| **Latency** | 30-45s | 6-12s | 6-12s (trust domain) |
| **Trust model** | Trustless | Relay chain only | Relay chain + collator ACKs |
| **Relay chain changes** | None | Medium | Medium |
| **Parachain changes** | Medium | Medium | High |
| **Collator changes** | Low (relayer) | Medium | High |
| **Implementation complexity** | Medium | Medium | Very High |
| **Prerequisites** | BEEFY | None | Low-Latency v2 |
| **Message ordering** | Unordered | Ordered per source | Ordered per source |
| **Delivery guarantee** | Best-effort | Inclusion-based | Acknowledgement-based |
| **Proof size** | ~500-1200 bytes | ~200-500 bytes | ~200-500 bytes |
| **Use case fit** | Cross-domain, trustless | HRMP replacement | High-frequency, low-latency |

---

## Recommendations

### For Near-Term (6-12 months)

**XCMP MMD** is more practical if:
- You need trustless messaging now
- You want to avoid relay chain changes
- You're okay with 30-45s latency
- You want a self-contained solution

**Speculative Messaging Phase 1** is better if:
- You want to replace HRMP's relay chain storage
- You're okay with inclusion-based latency initially
- You want to build toward low-latency messaging
- You can coordinate relay chain runtime changes

### For Long-Term (12-24 months)

**Speculative Messaging (Full)** is the strategic choice:
- Enables competitive latency with monolithic chains
- Supports trust domain flexibility
- Scales horizontally while feeling vertical
- Aligns with Polkadot's vision of composable parachains

**XCMP MMD** remains valuable for:
- Cross-domain messaging (untrusted parachains)
- Fallback when acknowledgements unavailable
- External chain bridges (with modifications)

### Hybrid Approach

The two approaches are not mutually exclusive:

1. **Implement Speculative Messaging** as the primary protocol
2. **Use XCMP MMD-style proofs** for cross-domain messaging
3. **Future trust-domain phases** use acknowledgement-based speculative messaging
4. **Untrusted or external domains** can fall back to finality-based proofs

This provides:
- Low latency where trust exists
- Trustless operation where it doesn't
- Graceful degradation
- Maximum flexibility

---

## Conclusion

### Key Takeaway

**XCMP MMD** and **Speculative Messaging** solve different problems:

- **XCMP MMD**: "How do we do trustless cross-chain messaging?"
- **Speculative Messaging**: "How do we do fast cross-chain messaging?"

The **minimal POC implementation design** for Speculative Messaging is doable as a first step, but it's essentially "HRMP with off-chain message passing" - still inclusion-based, just without relay chain storage.

The **real value proposition** (low-latency speculative messaging) requires:
- Late block proofs (Phase 2)
- Low-Latency v2 integration (Phase 3)
- Trust domain infrastructure (Phase 3)

### Strategic Decision

If Polkadot's goal is to enable **high-frequency cross-chain applications** (DeFi, gaming, real-time dApps), then **Speculative Messaging is the right long-term bet**, despite its complexity.

If the goal is to provide **trustless messaging with reasonable latency**, then **XCMP MMD is simpler and more self-contained**.

The **hybrid approach** (Speculative Messaging with XCMP MMD-style fallback for untrusted domains) offers the best of both worlds but requires implementing both systems.

---

## References

- [Speculative Messaging Design Document](speculative-messaging-design.md)
- [Speculative Messaging Implementation Design](speculative-messaging-impl-design.md)
- [XCMP MMD Minimal POC](xcmp-mmd-minimal-poc.md)
- [Low-Latency Parachains v2 Design](low-latency-v2-design.md) (referenced but not included)
- [XCMP Design Discussion Forum](https://forum.polkadot.network/t/xcmp-design-discussion/7328)
