# Speculative Messaging — Implementation Delta

This document records deliberate deviations between
`docs/speculative-messaging-impl-design.md` and the actual code on
`ron/speculative-messaging-poc`. Each entry identifies the design section,
describes what diverged and why, and flags whether the design doc should be
updated or the code brought closer to the design.

---

## 1. `MessageBatch` and `LateBlockProof` — missing `number_of_destinations` / `leaf_index` fields

**Design section:** §3.2, §3.5

**Design:**
```rust
pub struct MessageBatch {
    pub subtree_inclusion_proof: Vec<Hash>,
    // ... no leaf-count or leaf-index fields
}
```

**Implementation** (`polkadot/primitives/src/v10/mod.rs`):
```rust
pub struct MessageBatch {
    pub subtree_inclusion_proof: Vec<Hash>,
    pub number_of_destinations: u32,  // added
    pub leaf_index: u32,              // added
    // ...
}
pub struct LateBlockProof {
    pub number_of_destinations: u32,  // added
    pub leaf_index: u32,              // added
    // ...
}
```

**Why:** `binary_merkle_tree::verify_proof` requires both the total leaf count and
the 0-based leaf index as explicit parameters. The design's pseudocode used a
hypothetical `verify_merkle_proof` helper that absorbed those arguments internally.

**Recommendation:** Update §3.2 and §3.5 to add these two fields with a note
explaining the `binary_merkle_tree` API requirement.

---

## 2. Late block proofs live only in the PoV, not in the block body

**Design section:** §6.2

**Design and implementation agree:** Late block proofs are wrapped in
`ParachainBlockDataV4 { inner, late_block_proofs }` in the PoV. The PVF decodes
the wrapper, executes the block with `inner`, then independently verifies and
transforms requires via `apply_messaging_proofs`. The block body
(`SpeculativeIngress` / `MessageBatch`) carries no proof fields.

The collator generates proofs via the runtime API (`generate_late_block_proof`)
when it detects a root mismatch, and routes them exclusively into the PoV wrapper.
The runtime's `ingest_verified_messages` processes only message payloads and
root state — it never sees late block proof data.

**Status:** Resolved — an earlier draft of the implementation had proofs in both
locations, but that was corrected to match the design (see commit `283eba84`).

---

## 3. `CandidateCommitments` — direct fields match the design

**Design section:** §2, §6.1

**Design and implementation agree:** `CandidateCommitments` has `provides` and
`requires` as direct top-level fields:

```rust
// polkadot/primitives/src/v9/mod.rs
pub struct CandidateCommitments<N = BlockNumber> {
    // ... existing fields ...
    pub provides: Option<ProvidesCommitment>,
    pub requires: Vec<RequiresCommitment>,
}
```

`ProvidesCommitment` and `RequiresCommitment` are defined in v9 and re-exported
from v10. `v10::CandidateCommitments` is a direct re-export of the v9 type — no
separate struct, no `From` conversion.

Non-speculative candidates use `provides: None, requires: vec![]`.

**Status:** Resolved — an earlier draft used a `SpeculativeCommitments` wrapper
in v9, which was removed (see commit `3898ffe`).

---

## 4. No separate `speculative_messaging.rs` module

**Design section:** §4.1

**Design:** Specifies a new file:
```
polkadot/runtime/parachains/src/speculative_messaging.rs
```
hosting `ProvidesRoots`, `update_provides_root`, and `provides_root`.

**Implementation:** All speculative relay-chain logic (`ProvidesRoots`,
`PendingSpeculativeData`, and helpers) is inlined into
`polkadot/runtime/parachains/src/inclusion/mod.rs`, grouped under clearly
labelled `// ── Phase 1 Speculative Messaging (POC) ──` comment sections.

**Why (deliberate POC choice):** The storage items and helper functions are
tightly coupled to `process_candidates` and `enact_candidate`. Keeping them in
one file lets a reviewer follow the full backing→enactment flow without
switching files. A separate module only pays off in production when independent
testability and a clean extraction boundary matter. For the POC, inlining with
clear section headers achieves the same readability goal.

**Recommendation:** Update §4.1 to note that the POC inlines these into
`inclusion/mod.rs` and record the separate-module approach as the intended
production organization.

---

## 5. `OutgoingMMRs` / `MMRState` storage structure differs

**Design section:** §5.1

**Design:**
```rust
pub struct MMRState {
    pub leaf_count: u64,
    pub root: H256,
    pub nodes: BTreeMap<u64, H256>,
}

#[pallet::storage]
pub type OutgoingMMRs<T: Config> = StorageMap<_, Twox64Concat, ParaId, MMRState>;
```
(root and nodes stored inline in one map entry)

**Implementation** (`cumulus/pallets/speculative-outbox/src/lib.rs`):
```rust
pub struct MMRState {
    pub size: u64,        // MMR node count (mmr.mmr_size()), not leaf count alone
    pub leaf_count: u64,
}

#[pallet::storage]
pub type OutgoingMMRState<T: Config> = StorageMap<_, Twox64Concat, ParaId, MMRState>;

#[pallet::storage]
pub type MMRNodes<T: Config> = StorageDoubleMap<_, Twox64Concat, ParaId, Twox64Concat, u64, H256>;
```
The root is not stored — it is computed on demand from peak positions via
`compute_mmr_root_from_storage(dest, state.size)`. Nodes are in a separate
storage map, not inline in `MMRState`.

**Why:** FRAME storage maps have size constraints that make large inline
`BTreeMap` values impractical. A separate `StorageDoubleMap` is the idiomatic
approach. Computing the root on demand avoids a stale-root risk and is cheap
given that only O(log n) peak hashes need to be read.

**Recommendation:** Update §5.1 to describe the two-map layout and explain why
the root is computed on-demand rather than stored.

---

## 6. `SourceState.local_subtree` replaced with peaks-only fields

**Design section:** §5.2

**Design:**
```rust
pub struct SourceState {
    pub last_processed: u64,
    pub last_seen_provides_root: H256,
    pub last_seen_subtree_root: H256,
    pub local_subtree: MMRState,  // full MMRState struct
}
```

**Implementation** (`cumulus/pallets/speculative-inbox/src/lib.rs`):
```rust
pub struct SourceState {
    pub last_processed: u64,
    pub last_seen_provides_root: H256,
    pub last_seen_subtree_root: H256,
    pub mmr_size: u64,           // leaf count (not the mmr_lib node count)
    pub mmr_peaks: Vec<H256>,    // peaks only — no full node set
}
```

**Why:** The receiver only needs to reconstruct the subtree root from new
messages batch-by-batch. Storing only the peaks (O(log n) entries) is
sufficient — the full node set is never needed again after the root is
verified. This avoids growing on-chain storage proportionally with message
count.

**Recommendation:** Update §5.2 to document the peaks-only approach and clarify
that `mmr_size` here counts leaves (not mmr_lib's internal node count).

---

## 7. `PendingSpeculativeData` relay-chain storage not in design

**Design section:** §4.2

**Design:** Describes the two-phase backing/enactment model but does not define
any intermediate storage item to hold speculative data between the two phases.

**Implementation** (`polkadot/runtime/parachains/src/inclusion/mod.rs`):
```rust
#[pallet::storage]
pub(crate) type PendingSpeculativeData<T: Config> = StorageMap<
    _,
    Twox64Concat,
    CandidateHash,
    (Option<Hash>, Vec<(Id, Hash)>),  // (provides_root, requires)
>;
```
Written when a candidate enters `PendingAvailability` in `process_candidates`;
consumed (taken) by `enact_candidate` when the candidate is enacted.

**Why:** Without this storage item, `enact_candidate` has no access to the
speculative fields because `CandidatePendingAvailability` does not re-derive
them from the commitments — they are consumed at backing time and need to
survive until enactment.

**Recommendation:** Add `PendingSpeculativeData` to §4.1 (storage items) and
update §4.2 to describe the write-at-backing / read-at-enactment lifecycle.

---

## 8. First message position check — design has a latent bug for position 0

**Design section:** §5.2

**Design pseudocode:**
```rust
for msg in &batch.messages {
    ensure!(
        msg.position == local_state.last_processed + 1,
        VerificationError::NonConsecutiveMessage,
    );
    // ...
}
```
This fails for the first message at position 0: when `last_processed` defaults
to 0 (the zero-value), the expected position is 1, not 0.

**Implementation fix** (`cumulus/pallets/speculative-inbox/src/lib.rs`):
```rust
let expected_position = if state.mmr_size == 0 {
    0  // first-ever message must be at position 0
} else {
    state.last_processed + 1
};
```

**Recommendation:** Update §5.2 pseudocode to include the `mmr_size == 0` guard.

---

## 9. Late block proof — old subtree Merkle proof verified only in PVF

**Design section:** §6.2

**Design** (`verify_and_transform` pseudocode):
1. Verify `old_subtree_root` was in `old_provides_root` via Merkle proof.
2. Verify `new_subtree_root` is in `new_provides_root` via Merkle proof.
3. Verify MMR extension (old subtree is valid prefix of new).
4. Return transformed `RequiresCommitment`.

**Implementation** (`apply_messaging_proofs` in `validate_block/implementation.rs`):
All four steps are performed in full inside the PVF. The runtime
(`ingest_verified_messages`) never sees late block proofs and performs no
verification of them.

The old subtree Merkle proof (`old_subtree_proof`) is verified exclusively by
the PVF. Adding `proof.old_subtree_root == state.last_seen_subtree_root` as a
runtime defense-in-depth check was considered but is unnecessary for the POC
since the PVF is the consensus-critical path and always runs before backing.

**Recommendation:** Update §6.2 to note that the runtime performs no late-block-
proof verification; all four steps of `verify_and_transform` happen inside the
PVF's `apply_messaging_proofs`. Optionally note that adding a
`last_seen_subtree_root` equality check to the runtime would be a future
defense-in-depth improvement.

---

## 10. Version gating uses feature flag, not a new descriptor struct family

**Design section:** §2

**Design:** Proposes a new `CandidateDescriptorV4` struct to signal speculative
support, alongside `CandidateReceiptV4` / `CommittedCandidateReceiptV4`.

**Implementation:**
- No new descriptor struct is added.
- The existing `CandidateDescriptorV2::new_v4()` constructor writes version
  byte `2` in the descriptor's reserved version field.
- Speculative field population is gated on `FeatureIndex::SpeculativeMessaging`
  being set in node features (checked in `collation-generation` and
  `candidate-validation`).

**Why:** Adding a new top-level struct would require updating every site that
pattern-matches on descriptor versions. The existing version-byte mechanism
inside `CandidateDescriptorV2` already handles multi-version coexistence via
the `version()` method. A new literal struct is unnecessary for the POC.

**Recommendation:** Update §2 to clarify that "V4" refers to a new value of the
existing descriptor version byte, not a new struct family, and describe the
`FeatureIndex::SpeculativeMessaging` gate.

---

## Summary Table

| # | Design section | Deviation | Status | Action |
|---|---|---|---|---|
| 1 | §3.2, §3.5 | `number_of_destinations` / `leaf_index` missing from design types | Open | Update design |
| 2 | §6.2 | Late block proofs exclusively in PoV | Resolved | No action needed |
| 3 | §2, §6.1 | `CandidateCommitments` has direct fields matching design | Resolved | No action needed |
| 4 | §4.1 | No separate `speculative_messaging.rs`; inlined into `inclusion` | Open | Update design |
| 5 | §5.1 | `OutgoingMMRs` split into two storage maps; root computed on-demand | Open | Update design |
| 6 | §5.2 | `SourceState.local_subtree` replaced by peaks-only `mmr_size`/`mmr_peaks` | Open | Update design |
| 7 | §4.2 | `PendingSpeculativeData` storage item not described | Open | Update design |
| 8 | §5.2 | Position-0 first-message bug in design pseudocode | Open | Fix design |
| 9 | §6.2 | Old subtree Merkle proof verified in PVF only, not runtime | Open | Update design |
| 10 | §2 | Version gating uses feature flag + version byte, not new struct family | Open | Update design |
