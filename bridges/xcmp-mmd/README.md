# XCMP MMD - Trustless Cross-Chain Messaging

A proof-of-concept implementation of trustless cross-chain message delivery between parachains using Merkle Mountain Ranges (MMR) and cryptographic proofs.

**Status:** Complete ✅ | **Last Updated:** 2026-04-24

---

## 📚 Quick Links

- **[Documentation](docs/README.md)** - Complete design, implementation, and testing guides
- **[Pallets](pallets/)** - Outbox, inbox, and integration tests
- **[Relayer](relayer/)** - Off-chain proof construction and submission
- **[Testing](testing/)** - Zombienet configs and development tools

---

## 🎯 Overview

XCMP MMD enables trustless message delivery between parachains without relying on validators. It uses a three-tier cryptographic proof system:

1. **Relay MMR Proof** - Proves relay block is finalized by BEEFY
2. **Para-heads Merkle Proof** - Proves source header is in relay block
3. **Outbox MMR Proof** - Proves message is committed in source parachain

Off-chain relayers construct these proofs and submit them to destination parachains, which verify them on-chain without trusting the relayer.

### Key Innovation: MMR Ancestry Proofs

The system uses **MMR ancestry proofs** to eliminate race conditions between proof generation and verification. When the destination's relay parent advances between proof generation and submission, the system uses `pallet_mmr::verify_ancestry_proof` to derive the historical MMR root, allowing proofs to remain valid across multiple relay blocks.

---

## 📦 Directory Structure

```
bridges/xcmp-mmd/
├── docs/                      # Complete documentation
│   ├── README.md             # Documentation index
│   ├── DESIGN.md             # Architecture and design
│   ├── IMPLEMENTATION.md     # Code structure and integration
│   ├── TESTING.md            # End-to-end testing guide
│   ├── STATUS.md             # Implementation status
│   └── COMPARISON.md         # Design verification
├── pallets/                   # Runtime pallets
│   ├── outbox/               # Source parachain pallet
│   ├── inbox/                # Destination parachain pallet
│   └── integration-tests/    # Integration tests
├── primitives/                # Shared types and constants
├── relayer/                   # Off-chain relayer tool
└── testing/                   # Testing infrastructure
    ├── runtime-integration/  # Test runtime (outdated)
    ├── zombienet/            # Test network configuration
    └── tools/                # Development utilities
```

---

## 🚀 Quick Start

### 1. Read the Documentation

Start with [docs/README.md](docs/README.md) for a complete overview.

### 2. Build the Components

```bash
# From polkadot-sdk repo root

# Build relay chain
cargo build -p polkadot --release

# Build parachain
cargo build -p polkadot-parachain-bin --release

# Build relayer
cd bridges/xcmp-mmd/relayer
SKIP_WASM_BUILD=1 cargo build --release
```

### 3. Run the Test

```bash
# Start zombienet network
zombienet --provider native spawn bridges/xcmp-mmd/testing/zombienet/xcmp-mmd-poc.toml

# Run end-to-end test
cd bridges/xcmp-mmd/testing/zombienet
./e2e-test.sh
```

See [docs/TESTING.md](docs/TESTING.md) for detailed instructions.

---

## 🧪 Testing

```bash
# Run pallet tests
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-outbox
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-inbox
SKIP_WASM_BUILD=1 cargo test -p cumulus-pallet-xcmp-mmd-integration-tests
```

---

## 📐 Architecture

### Message Flow

```
Source Parachain                 Relay Chain                 Destination Parachain
     │                                │                              │
     │ 1. Commit message to MMR       │                              │
     │    (OutboxLeaf)                │                              │
     │                                │                              │
     │ 2. Deposit digest in header    │                              │
     │    (XcmpMmdDigest)             │                              │
     │                                │                              │
     │                                │ 3. Include source header     │
     │                                │    in ParaHeadsRoot          │
     │                                │                              │
     │                                │ 4. BEEFY signs MMR root      │
     │                                │                              │
     │                                │                              │
     └────────────────────────────────┼──────────────────────────────┘
                                      │
                                      │ 5. Relayer constructs proofs
                                      │    - Relay MMR proof
                                      │    - Para-heads proof
                                      │    - Outbox MMR proof
                                      │    - Ancestry proof (if needed)
                                      │
                                      │ 6. Submit to destination
                                      │
                                      └──────────────────────────────▶
                                                                      │
                                                                      │ 7. Verify proofs
                                                                      │ 8. Dispatch message
```

### Components

**Outbox Pallet** (`pallets/outbox/`)
- Wraps `XcmpMessageSource` to intercept outbound messages
- Maintains MMR of message commitments
- Deposits MMR root in block headers
- Provides runtime API for proof generation

**Inbox Pallet** (`pallets/inbox/`)
- Accepts `MessageWithProof` via permissionless extrinsic
- Verifies three-tier proofs with ancestry proof support
- Replay protection via `SeenMessages` storage
- Dispatches verified messages to `XcmpQueue`

**Relayer** (`relayer/`)
- Monitors source parachain for new messages
- Constructs three-tier proof bundles
- Generates ancestry proofs when needed
- Signs and submits extrinsics to destination

**Primitives** (`primitives/`)
- `OutboxLeaf` - Message commitment structure
- `XcmpMmdDigest` - Header digest format
- Hard bounds constants

---

## 🔑 Key Features

- ✅ **Trustless** - No reliance on validators or relayers for security
- ✅ **Censorship Resistant** - Anyone can run a relayer
- ✅ **Cryptographically Verified** - All three proof tiers use proper verification
- ✅ **Race Condition Free** - MMR ancestry proofs eliminate timing issues
- ✅ **Replay Protected** - Messages identified by `(source, mmr_leaf_index)`

---

## 📝 Known Limitations (POC Scope)

### By Design
- No pruning of MMR or payload storage
- No incentive mechanism for relayers
- No receipts/acknowledgments
- Unordered message delivery
- Best-effort (no guaranteed delivery)

### Implementation
- MMR rebuild: O(n) complexity on each append
- BEEFY leaf decoding: Simplified extraction
- Relayer: HTTP polling instead of WebSocket subscriptions

See [docs/STATUS.md](docs/STATUS.md) for full details.

---

## 🚀 Production Considerations

For production use, consider:

1. **Efficient MMR Storage** - Incremental updates instead of full rebuild
2. **WebSocket Subscriptions** - Real-time updates instead of polling
3. **Database** - Persistent storage for relayer state
4. **Multi-destination** - Support multiple para pairs
5. **Retry Logic** - Exponential backoff for failures
6. **Metrics** - Prometheus monitoring
7. **Economic Model** - Relayer incentives
8. **Proof Batching** - Multiple messages per proof

See [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) for full list.

---

## 🔗 References

- **Design Document**: [docs/DESIGN.md](docs/DESIGN.md)
- **Implementation Guide**: [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md)
- **Testing Guide**: [docs/TESTING.md](docs/TESTING.md)
- **Merkle Mountain Ranges**: https://github.com/opentimestamps/opentimestamps-server/blob/master/doc/merkle-mountain-range.md
- **BEEFY Finality**: https://spec.polkadot.network/sect-finality#sect-grandpa-beefy

---

## 📧 Notes

This is a **Proof of Concept** demonstrating the feasibility of MMR-based cross-chain messaging with ancestry proof support. The core protocol is complete and functional, with known limitations documented for production hardening.
