# XCMP MMD - Proof of Concept

Trustless cross-chain message delivery between parachains using a three-tier cryptographic proof system.

**Status:** Complete ✅ | **Last Updated:** 2026-04-24

---

## 📚 Documentation

### Core Documentation

#### [DESIGN.md](DESIGN.md) - Architecture & Design
**Purpose**: Understand how the system works

**Contents**:
- Problem statement and motivation
- Three-tier proof system (Relay MMR → Para-heads → Outbox MMR)
- MMR ancestry proof mechanism
- Message flow and verification steps
- Security properties and performance characteristics
- Comparison to alternatives

**Audience**: Anyone wanting to understand the design

---

#### [IMPLEMENTATION.md](IMPLEMENTATION.md) - Implementation Guide
**Purpose**: Understand what was built and how to integrate it

**Contents**:
- Components built (outbox pallet, inbox pallet, relayer)
- Code structure and key files
- Runtime integration steps
- Building instructions and critical configuration
- MMR ancestry proof implementation details
- Known limitations and production considerations

**Audience**: Developers integrating the pallets or modifying the code

---

#### [TESTING.md](TESTING.md) - Testing Guide
**Purpose**: Run the end-to-end test

**Contents**:
- Prerequisites and setup
- Running zombienet network
- Running the e2e test script
- Manual verification steps
- Troubleshooting guide

**Audience**: Anyone testing the POC

---

### Reference Documentation

#### [STATUS.md](STATUS.md) - Implementation Status
Quick reference showing:
- Completion status (7/7 phases complete)
- Components built with test counts
- Data structures and key implementation details
- Known limitations and production considerations

---

#### [COMPARISON.md](COMPARISON.md) - Design Verification
Detailed comparison between design specification and implementation:
- Core design consistency verification
- Data structure comparison
- 8-step verification detailed comparison
- Implementation notes
- Overall assessment

---

## 🚀 Quick Start

1. **Understand the design**: Read [DESIGN.md](DESIGN.md)
2. **Build the components**: Follow [IMPLEMENTATION.md](IMPLEMENTATION.md) build section
3. **Run the test**: Follow [TESTING.md](TESTING.md)

---

## 🎯 Overview

XCMP MMD uses three nested cryptographic proofs to enable trustless message delivery:

1. **Relay MMR Proof** - Proves a relay block is finalized by BEEFY
2. **Para-heads Merkle Proof** - Proves source parachain header is in the relay block
3. **Outbox MMR Proof** - Proves message is committed in source parachain

Off-chain relayers construct these proofs and submit them to destination parachains, which verify them on-chain without trusting the relayer.

### Key Innovation: MMR Ancestry Proofs

The system uses **MMR ancestry proofs** to eliminate race conditions between proof generation and verification:

- **Problem**: Relayer generates proof at relay block 100, but destination advances to block 105 before verification
- **Solution**: Include ancestry proof showing MMR root at block 100 is ancestor of MMR root at block 105
- **Benefit**: Proofs remain valid even as the destination's relay parent advances

This leverages Substrate's built-in `pallet_mmr::verify_ancestry_proof` to derive historical MMR roots.

---

## 📦 Implementation Locations

### Pallets
- **Outbox**: `cumulus/pallets/xcmp-mmd-outbox/`
- **Inbox**: `cumulus/pallets/xcmp-mmd-inbox/`
- **Integration Tests**: `cumulus/pallets/xcmp-mmd-integration-tests/`

### Primitives
- **Core Types**: `cumulus/primitives/xcmp-mmd/`

### Relayer
- **Off-chain Tool**: `tools/xcmp-mmd/relayer/`

### Infrastructure
- **Well-Known Keys**: `polkadot/primitives/src/v9/mod.rs`
- **Collator Client**: `cumulus/client/parachain-inherent/src/lib.rs`

### Testing
- **Zombienet Config**: `tools/xcmp-mmd/zombienet/`

---

## ✅ Completion Status

**All phases complete:**
- ✅ Phase 0: MMR root in well-known keys
- ✅ Phase 1: XcmpMmdOutbox pallet (9 tests passing)
- ✅ Phase 2: XcmpMmdInbox pallet with ancestry proof support (6 tests passing)
- ✅ Phase 3: Primitives crate
- ✅ Phase 4: Integration tests (7 tests passing)
- ✅ Phase 5: Relayer tool with three-tier proof construction
- ✅ Phase 6: Zombienet config + e2e test script
- ✅ Phase 7: Documentation complete

See [STATUS.md](STATUS.md) for detailed implementation status.

---

## 🔍 Message Flow

### 1. Source Parachain (Outbox)
```
User submits XCM
    ↓
XcmpQueue enqueues message
    ↓
XcmpMmdOutbox intercepts
    ↓
Create OutboxLeaf { dest, payload_hash }
    ↓
Append to outbox MMR
    ↓
Deposit xmmd digest in header
```

### 2. Off-chain Relayer
```
Monitor source finalized headers
    ↓
Detect xmmd digest
    ↓
Fetch HRMP outbound messages
    ↓
Build three-tier proof:
  - Outbox MMR proof
  - Find relay block containing source header
  - Relay MMR proof (with ancestry proof if needed)
  - Para-heads Merkle proof
    ↓
Sign and submit to destination
```

### 3. Destination Parachain (Inbox)
```
Receive submit_xcmp_mmd(MessageWithProof)
    ↓
Read relay MMR root from ValidationData
    ↓
Handle ancestry proof if needed:
  - If anchor == current: use MMR root directly
  - If anchor < current: verify ancestry proof
  - If anchor > current: reject (invalid)
    ↓
Verify Tier 1: Relay MMR proof
    ↓
Verify Tier 2: Para-heads Merkle proof
    ↓
Verify Tier 3: Outbox MMR proof
    ↓
Verify payload hash
    ↓
Check replay protection
    ↓
Dispatch to XcmpQueue
```

---

## 📐 Data Structures

### MessageWithProof

```rust
struct MessageWithProof {
    source: ParaId,
    dest: ParaId,
    mmr_leaf_index: u64,
    relay_mmr_leaf_index: u64,
    payload: Vec<u8>,
    
    // Tier 1: Relay MMR proof
    relay_mmr_proof: Vec<H256>,
    relay_mmr_leaf: Vec<u8>,
    relay_mmr_size: u64,
    relay_anchor_number: u32,
    relay_ancestry_proof: Option<AncestryProof<H256>>,
    
    // Tier 2: Para-heads Merkle proof
    para_heads_proof: Vec<H256>,
    source_head: Vec<u8>,
    para_head_index: u32,
    para_heads_count: u32,
    
    // Tier 3: Outbox MMR proof
    outbox_leaf: OutboxLeaf,
    outbox_mmr_proof: Vec<H256>,
    outbox_mmr_size: u64,
}
```

---

## 🔧 Building

```bash
# From polkadot-sdk repo root

# Build relay chain
cargo build -p polkadot --release

# Build parachain
cargo build -p polkadot-parachain-bin --release

# Build relayer
cd tools/xcmp-mmd/relayer
SKIP_WASM_BUILD=1 cargo build --release
```

---

## 🧪 Testing

```bash
# Run pallet tests
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-outbox
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-inbox
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-integration-tests

# Run end-to-end test (requires zombienet)
zombienet --provider native spawn tools/xcmp-mmd/zombienet/xcmp-mmd-poc.toml
cd tools/xcmp-mmd/zombienet && ./e2e-test.sh
```

See [TESTING.md](TESTING.md) for detailed instructions.

---

## 📝 Known Limitations (POC Scope)

### By Design
- No pruning of MMR or payload storage
- No incentive mechanism for relayers
- No receipts/acknowledgments
- Unordered message delivery
- Best-effort (no guaranteed delivery)

### Implementation Notes
- MMR rebuild: O(n) complexity on each append
- Relayer: HTTP polling instead of WebSocket subscriptions
- BEEFY leaf decoding: Simplified extraction

See [STATUS.md](STATUS.md) for full details.

---

## 🚀 Production Considerations

For production use, consider:

1. **Efficient MMR Storage**: Incremental updates
2. **WebSocket Subscriptions**: Real-time updates
3. **Database**: Persistent relayer state
4. **Multi-destination**: Support multiple para pairs
5. **Retry Logic**: Exponential backoff
6. **Metrics**: Prometheus monitoring
7. **Economic Model**: Relayer incentives
8. **Proof Batching**: Multiple messages per proof

See [IMPLEMENTATION.md](IMPLEMENTATION.md) for full list.

---

## 🔗 References

- **Polkadot SDK**: https://github.com/paritytech/polkadot-sdk
- **Merkle Mountain Ranges**: https://github.com/opentimestamps/opentimestamps-server/blob/master/doc/merkle-mountain-range.md
- **BEEFY Finality**: https://spec.polkadot.network/sect-finality#sect-grandpa-beefy
- **XCMP Design**: https://wiki.polkadot.network/docs/learn-xcm

---

## 📧 Notes

This is a **Proof of Concept** implementation demonstrating the feasibility of MMR-based cross-chain messaging with ancestry proof support. The core protocol is complete and functional, with known limitations documented for production hardening.
