# XCMP MMD POC - Implementation Guide

**Last Updated:** 2026-04-22  
**Status:** Core POC Complete ✅

---

## 📊 Implementation Status

### ✅ Completed Phases

**Phase 0: MMR Root in Well-Known Keys**
- ✅ Added `MMR_ROOT_HASH` constant to `polkadot/primitives/src/v9/mod.rs`
- ✅ Added to `relevant_keys` in `cumulus/client/parachain-inherent/src/lib.rs`
- ✅ Storage key verified: `0xa8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1`
- ✅ Confirmed Westend uses "Mmr" as pallet name

**Phase 1: XcmpMmdOutbox Pallet**
- ✅ Location: `cumulus/pallets/xcmp-mmd-outbox/`
- ✅ Wraps `XcmpMessageSource` trait
- ✅ Direct MMR management using `mmr-lib` with `Keccak256Merge`
- ✅ Storage: `MmrLeafCount`, `OutboxLeaves`, `MmrRootHash`
- ✅ Deposits `DigestItem::PreRuntime(*b"xmmd", digest)` in `on_finalize()`
- ✅ Runtime API for proof generation
- ✅ 9 passing tests

**Phase 2: XcmpMmdInbox Pallet**
- ✅ Location: `cumulus/pallets/xcmp-mmd-inbox/`
- ✅ 8-step verification implemented:
  - Step 1: Get relay MMR root from RelayChainStateProof ✅
  - Step 2: Verify relay MMR proof using mmr-lib ✅
  - Step 3: Verify para-heads proof (simplified for POC) ✅
  - Step 4: Extract outbox MMR root from header digest ✅
  - Step 5: Verify outbox MMR proof using mmr-lib ✅
  - Step 6: Verify payload hash and destination ✅
  - Step 7: Replay protection ✅
  - Step 8: Dispatch to XcmpMessageHandler ✅
- ✅ Actual cryptographic verification for Steps 2, 5
- ✅ 4 passing tests

**Phase 3: Primitives**
- ✅ Location: `cumulus/primitives/xcmp-mmd/`
- ✅ `OutboxLeaf` struct with `MaxEncodedLen`
- ✅ `XcmpMmdDigest` struct with `MaxEncodedLen`
- ✅ Hard bounds constants module

**Phase 4: Integration Tests**
- ✅ Location: `cumulus/pallets/xcmp-mmd-integration-tests/`
- ✅ 7 passing integration tests:
  - OutboxLeaf encoding/decoding
  - MessageWithProof structure
  - Payload hash verification
  - End-to-end data flow
  - MMR leaf hash consistency
  - Replay protection key format
  - Message size bounds

### 🔄 Remaining for Production

**Phase 5: Relayer Tool** (Not required for POC)
- Proof generation and submission automation
- Source monitoring and payload fetching
- Estimated: 3-4 days

**Phase 6: Zombienet Testing** (Optional for POC)
- End-to-end network testing
- Estimated: 1-2 days

**Phase 7: Documentation** (Partial)
- ✅ Implementation guide (this document)
- ✅ TODO tracker
- ✅ Cross-verification report
- ⏳ Relayer setup guide
- ⏳ Architecture diagram

---

## 🎯 Design Decisions

### Option B: Well-Known Key Approach (Implemented)

**Choice:** Collator includes `Mmr::RootHash` in inherent proof via `KeyToIncludeInRelayProof`

**Rationale:**
- Cleaner extrinsic interface (no extra `relay_mmr_root_proof` field)
- Proof verified once per block (not per message)
- More aligned with Cumulus patterns
- Smaller extrinsic size

**Alternative (Option A):** Relayer carries `relay_mmr_root_proof` in extrinsic
- Simpler collator (no changes needed)
- Larger extrinsic (extra storage proof per batch)
- Not implemented in this POC

---

## 📐 Data Structures

### MessageWithProof (Option B)

```rust
struct MessageWithProof {
    source: ParaId,
    dest: ParaId,
    mmr_leaf_index: u64,
    relay_mmr_leaf_index: u64,
    payload: Vec<u8>,
    relay_mmr_proof: Vec<H256>,
    relay_mmr_leaf: Vec<u8>,        // BEEFY MMR leaf data
    relay_mmr_size: u64,            // MMR size for verification
    para_heads_proof: Vec<H256>,
    outbox_leaf: OutboxLeaf,        // Leaf data for verification
    outbox_mmr_proof: Vec<H256>,
    outbox_mmr_size: u64,           // MMR size for verification
}
```

### OutboxLeaf

```rust
struct OutboxLeaf {
    dest: u32,
    payload_hash: H256,  // Keccak256(payload)
}
```

### XcmpMmdDigest

```rust
struct XcmpMmdDigest {
    version: u8,
    root: H256,  // Outbox MMR root
}
```

---

## 🔍 Verification Flow

### Two-Step Relay MMR Root Access

1. **Anchor:** `relay_parent_storage_root` from `ValidationData` anchors the relay state proof
2. **Read:** `pallet_mmr::RootHash` is READ from that verified trie

```rust
// Step 1: Get validation data with relay_parent_storage_root
let validation_data = cumulus_pallet_parachain_system::ValidationData::<T>::get()?;

// Step 2: Construct RelayChainStateProof
let relay_state_proof = cumulus_pallet_parachain_system::RelayChainStateProof::new(
    T::SelfParaId::get(),
    validation_data.relay_parent_storage_root,
    relay_state_proof_storage,
)?;

// Step 3: Read pallet_mmr::RootHash from verified trie
let relay_mmr_root: H256 = relay_state_proof.read_entry(
    polkadot_primitives::well_known_keys::MMR_ROOT_HASH,
    None
)?;
```

### 8-Step Verification

1. **Get relay MMR root** from RelayChainStateProof
2. **Verify relay MMR proof** using mmr-lib, extract ParaHeadsRoot
3. **Verify para-heads proof** against ParaHeadsRoot (simplified for POC)
4. **Extract outbox MMR root** from source header digest
5. **Verify outbox MMR proof** using mmr-lib
6. **Verify payload hash** and destination match
7. **Replay protection** check and insert
8. **Dispatch** to XcmpMessageHandler

---

## 🔑 Key Implementation Details

### Well-Known Key

```rust
pub const MMR_ROOT_HASH: &[u8] = 
    &hex!["a8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1"];
```

Calculated as: `twox_128("Mmr") ++ twox_128("RootHash")`

### MMR Verification (Steps 2 & 5)

Uses `mmr-lib::MerkleProof` with `Keccak256Merge`:

```rust
struct Keccak256Merge;
impl Merge for Keccak256Merge {
    type Item = H256;
    fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
        let mut concat = [0u8; 64];
        concat[..32].copy_from_slice(lhs.as_ref());
        concat[32..].copy_from_slice(rhs.as_ref());
        Ok(sp_runtime::traits::Keccak256::hash(&concat))
    }
}

let proof = mmr_lib::MerkleProof::<H256, Keccak256Merge>::new(
    mmr_size,
    proof_items.to_vec(),
);

let calculated_root = proof.calculate_root(vec![(leaf_index, leaf_hash)])?;
```

### Replay Protection

Storage map: `(source_para_id: u32, mmr_leaf_index: u64) -> ()`

---

## 📦 Hard Bounds (POC)

```rust
pub const MAX_MESSAGES_PER_CALL: u32 = 4;
pub const MAX_PAYLOAD_BYTES: u32 = 256 * 1024;
pub const MAX_RELAY_MMR_PROOF_ITEMS: u32 = 128;
pub const MAX_PARA_HEADS_PROOF_ITEMS: u32 = 32;
pub const MAX_OUTBOX_MMR_PROOF_ITEMS: u32 = 64;
pub const MAX_TOTAL_CALL_BYTES: u32 = 768 * 1024;
```

---

## 🔗 Key Files

### Primitives
- `cumulus/primitives/xcmp-mmd/src/lib.rs`

### Outbox Pallet
- `cumulus/pallets/xcmp-mmd-outbox/src/lib.rs`
- `cumulus/pallets/xcmp-mmd-outbox/src/runtime_api_impl.rs`
- `cumulus/pallets/xcmp-mmd-outbox/runtime-api/src/lib.rs`

### Inbox Pallet
- `cumulus/pallets/xcmp-mmd-inbox/src/lib.rs`
- `cumulus/pallets/xcmp-mmd-inbox/src/types.rs`
- `cumulus/pallets/xcmp-mmd-inbox/src/verification.rs`

### Integration Tests
- `cumulus/pallets/xcmp-mmd-integration-tests/tests/integration.rs`

### Polkadot Primitives
- `polkadot/primitives/src/v9/mod.rs` (MMR_ROOT_HASH constant)

### Collator Client
- `cumulus/client/parachain-inherent/src/lib.rs` (relevant_keys)

---

## ✅ Verification Against Spec

**Blog Spec:** `/Users/yangrong/yrong-blog/content/post/2026-04-09-xcmp-mmd-minimal-poc.md`

### Consistent Elements
- ✅ Proof stack structure (nested proofs)
- ✅ 8-step verification algorithm
- ✅ Payload custody via `HrmpOutboundMessages`
- ✅ All hard bounds match exactly
- ✅ Digest encoding: `PreRuntime(*b"xmmd", SCALE((version, root)))`
- ✅ Keccak256 hashing for payload and MMR
- ✅ Replay protection: `seen((source, mmr_leaf_index))`
- ✅ XcmpMessageHandler integration

### Implementation Additions
- ✅ MMR leaf data and size fields for proper verification
- ✅ DecodeWithMemTracking trait implementations
- ✅ Actual mmr-lib integration for cryptographic verification
- ✅ Integration tests validating complete flow

---

## 📝 Known Limitations (POC Scope)

### By Design
- ✅ No pruning of MMR storage
- ✅ No pruning of payload storage
- ✅ No incentive mechanism for relayers
- ✅ No receipts/acknowledgments
- ✅ Unordered message delivery
- ✅ Best-effort (no guaranteed delivery)
- ✅ No channel management

### Implementation Notes
1. **MMR Rebuild:** Outbox pallet rebuilds entire MMR from leaves on each append (O(n) complexity)
2. **Para-heads proof:** Simplified for POC (Step 3), production needs full binary merkle tree verification
3. **Relay MMR leaf:** Simplified extraction of ParaHeadsRoot (last 32 bytes), production needs full BEEFY leaf decoding

---

## 🎯 Success Criteria

- ✅ Outbox pallet creates MMR leaves and deposits digests
- ✅ Inbox pallet verifies nested proofs
- ✅ Actual cryptographic verification for core MMR proofs
- ✅ Replay protection mechanism works
- ✅ All data structures encode/decode correctly
- ✅ Integration tests validate end-to-end flow
- ⏳ Relayer generates all proofs (not implemented)
- ⏳ Message executed on destination (requires full runtime integration)

---

## 📚 Related Documents

- `xcmp-mmd-todo.md` - Detailed task tracker with checkboxes
- `xcmp-mmd-cross-verification.md` - Verification against blog spec
- Blog post: `/Users/yangrong/yrong-blog/content/post/2026-04-09-xcmp-mmd-minimal-poc.md`

---

## 🚀 Next Steps for Production

1. **Relayer Tool:** Implement proof generation and submission automation
2. **Full Runtime Integration:** Wire pallets into test runtime with actual relay chain
3. **Para-heads Verification:** Implement full binary merkle tree verification (Step 3)
4. **BEEFY Leaf Decoding:** Proper extraction of ParaHeadsRoot from relay MMR leaf
5. **Zombienet Testing:** End-to-end network testing
6. **Performance Optimization:** Efficient MMR storage and proof generation
7. **Production Hardening:** Error handling, weight calculation, security audit
