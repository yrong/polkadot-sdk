# XCMP MMD Tools

This directory contains tools and utilities for the XCMP MMD proof-of-concept.

---

## 📚 Documentation

**All documentation has been consolidated in:**

**`cumulus/docs/xcmp-mmd/`**

Please see:
- **[cumulus/docs/xcmp-mmd/README.md](../../cumulus/docs/xcmp-mmd/README.md)** - Main documentation index
- **[cumulus/docs/xcmp-mmd/DESIGN.md](../../cumulus/docs/xcmp-mmd/DESIGN.md)** - Architecture and design
- **[cumulus/docs/xcmp-mmd/IMPLEMENTATION.md](../../cumulus/docs/xcmp-mmd/IMPLEMENTATION.md)** - Implementation guide
- **[cumulus/docs/xcmp-mmd/TESTING.md](../../cumulus/docs/xcmp-mmd/TESTING.md)** - Testing guide
- **[cumulus/docs/xcmp-mmd/STATUS.md](../../cumulus/docs/xcmp-mmd/STATUS.md)** - Implementation status

---

## 🛠️ Tools in This Directory

### Relayer
**Location**: `relayer/`

Off-chain service that monitors source parachains and constructs three-tier proofs for destination parachains.

**Features**:
- Polls source parachain for new messages
- Constructs relay MMR proofs with ancestry proof support
- Generates para-heads Merkle proofs
- Generates outbox MMR proofs
- Signs and submits extrinsics to destination

**Usage**:
```bash
cd relayer
SKIP_WASM_BUILD=1 cargo build --release
./target/release/xcmp-mmd-relayer --config relayer.toml
```

See [relayer/README.md](relayer/README.md) for details.

---

### Zombienet Test Network
**Location**: `zombienet/`

Network configuration and test scripts for end-to-end testing.

**Contents**:
- `xcmp-mmd-poc.toml` - Network topology (relay chain + 2 parachains)
- `e2e-test.sh` - Automated test driver
- `README.md` - Manual verification instructions

**Usage**:
```bash
zombienet --provider native spawn zombienet/xcmp-mmd-poc.toml
cd zombienet && ./e2e-test.sh
```

See [zombienet/README.md](zombienet/README.md) for details.

---

### Development Utilities

**MMR Storage Key Calculator**:
- `calculate_mmr_key.rs` - Standalone script
- `mmr-key-calculator/` - Cargo project version

These tools were used during POC development to verify the storage key calculation for reading the relay chain's MMR root from the relay state proof.

**Storage key**: `0xa8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1`  
**Calculation**: `twox_128("Mmr") ++ twox_128("RootHash")`

---

## 🚀 Quick Start

1. **Read the documentation**: Start with [cumulus/docs/xcmp-mmd/README.md](../../cumulus/docs/xcmp-mmd/README.md)
2. **Build the components**: Follow [IMPLEMENTATION.md](../../cumulus/docs/xcmp-mmd/IMPLEMENTATION.md)
3. **Run the test**: Follow [TESTING.md](../../cumulus/docs/xcmp-mmd/TESTING.md)

---

## 📦 Directory Structure

```
bridges/xcmp-mmd/tools/
├── README.md                    (this file)
├── relayer/                     (off-chain relayer)
│   ├── src/
│   ├── Cargo.toml
│   ├── relayer.toml
│   └── README.md
├── zombienet/                   (test network config)
│   ├── xcmp-mmd-poc.toml
│   ├── e2e-test.sh
│   └── README.md
├── calculate_mmr_key.rs         (dev utility)
└── mmr-key-calculator/          (dev utility)
```

---

## ✅ Status

**POC Complete** - All components implemented and tested.

See [cumulus/docs/xcmp-mmd/STATUS.md](../../cumulus/docs/xcmp-mmd/STATUS.md) for detailed status.
