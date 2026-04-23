# XCMP MMD - Proof of Concept

Trustless cross-chain message delivery between parachains using a three-tier cryptographic proof system.

## 📚 Documentation

### [DESIGN.md](DESIGN.md) - Architecture & Design
**Purpose**: Understand how the system works

**Contents**:
- Problem statement and motivation
- Three-tier proof system explanation
- Architecture diagrams and message flow
- Security properties and performance characteristics
- Comparison to alternatives

**Audience**: Anyone wanting to understand the design

---

### [IMPLEMENTATION.md](IMPLEMENTATION.md) - Implementation Guide
**Purpose**: Understand what was built and how to integrate it

**Contents**:
- Components built (outbox pallet, inbox pallet, relayer)
- Code structure and key files
- Runtime integration steps
- Building instructions and critical configuration
- Known limitations and production considerations

**Audience**: Developers integrating the pallets or modifying the code

---

### [TESTING.md](TESTING.md) - Testing Guide
**Purpose**: Run the end-to-end test

**Contents**:
- Prerequisites and setup
- Running zombienet network
- Running the e2e test script
- Manual verification steps and troubleshooting

**Audience**: Anyone testing the POC

---

## Component-Specific Documentation

- **Relayer**: `relayer/README.md` - Relayer configuration and usage
- **Zombienet**: `zombienet/README.md` - Network topology and manual verification

## Quick Start

1. **Understand the design**: Read [DESIGN.md](DESIGN.md)
2. **Build the components**: Follow [IMPLEMENTATION.md](IMPLEMENTATION.md) build section
3. **Run the test**: Follow [TESTING.md](TESTING.md)

## Overview

XCMP MMD uses three nested cryptographic proofs to enable trustless message delivery:

1. **Relay MMR Proof** - Proves a relay block is finalized by BEEFY
2. **Para-heads Merkle Proof** - Proves source parachain header is in the relay block
3. **Outbox MMR Proof** - Proves message is committed in source parachain

Off-chain relayers construct these proofs and submit them to destination parachains, which verify them on-chain without trusting the relayer.

## Status

✅ **Complete**:
- Three-tier proof system fully implemented
- Outbox and inbox pallets with verification
- Off-chain relayer with SR25519 signing
- Runtime integration (penpal)
- End-to-end test setup

## Tools

This directory also contains utility tools used during POC development:

- `calculate_mmr_key.rs` - Calculate storage key for `pallet_mmr::RootHash`
- `mmr-key-calculator/` - Cargo project version of the key calculator

These tools were used to verify the storage key calculation for reading the relay chain's MMR root from the relay state proof.
