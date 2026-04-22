# XCMP MMD Tools

Utility tools for XCMP MMD POC development and testing.

## 📦 Tools

### calculate_mmr_key.rs
**Purpose:** Calculate the storage key for `pallet_mmr::RootHash` on the relay chain.

**Usage:**
```bash
rustc calculate_mmr_key.rs && ./calculate_mmr_key
```

**Output:**
```
MMR_ROOT_HASH key: 0xa8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1
```

**How it works:**
- Computes `twox_128("Mmr") ++ twox_128("RootHash")`
- This key is used in `polkadot/primitives/src/v9/mod.rs` as `MMR_ROOT_HASH` constant
- The relay chain collator includes this key in the relay state proof

### calculate_mmr_key (binary)
Pre-compiled binary of `calculate_mmr_key.rs` for convenience.

### mmr-key-calculator/
Cargo project version of the MMR key calculator with proper dependencies.

**Usage:**
```bash
cd mmr-key-calculator
cargo run
```

## 🔑 Background

For XCMP MMD Option B (well-known key approach), the destination parachain needs to read the relay chain's MMR root from the relay state proof. This requires knowing the exact storage key where `pallet_mmr::RootHash` is stored.

The storage key is calculated as:
```
storage_key = twox_128(pallet_name) ++ twox_128(storage_item_name)
            = twox_128("Mmr") ++ twox_128("RootHash")
            = 0xa8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1
```

This key is then:
1. Added to `polkadot_primitives::well_known_keys::MMR_ROOT_HASH`
2. Included in `relevant_keys` in the collator client
3. Used by the inbox pallet to read the MMR root from `RelayChainStateProof`

## 📝 Notes

- These tools were used during POC development to verify the storage key calculation
- The calculated key is now hardcoded in the codebase
- Kept here for reference and verification purposes
- If the relay chain changes the pallet name from "Mmr" to something else, use these tools to recalculate the key
