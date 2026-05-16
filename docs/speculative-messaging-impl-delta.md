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

## 5. `OutgoingMMRs` / `MMRState` — peaks-only storage matches inbox approach

**Design section:** §5.1

**Design:** Shows `MMRState { leaf_count, root, nodes: BTreeMap<u64, H256> }` with
root and all nodes stored inline.

**Implementation** (`cumulus/pallets/speculative-outbox/src/lib.rs`):
```rust
pub struct MMRState {
    pub leaf_count: u64,
    pub peaks: Vec<H256>,  // O(log n) — same approach as inbox
}
```

Both outbox and inbox use identical peaks-only storage with the same
`append_leaf_to_peaks` + `bag_peaks` helpers. `HistoricalSubtreeState` stores
`(root, peaks)` instead of `(root, size)` so extension proofs can be built
without a separate node store. `MMRNodes`, `DestMMRStore`, and the
`mmr_lib::MMR::push`/`commit` path are removed entirely.

**Tradeoff:** Per-message MMR inclusion proofs (proving a single leaf is in the
subtree root without the full batch) require the full internal node set. The
current design never needs them — the receiver reconstructs the entire subtree
from all messages and checks the root. When selective or relaxed delivery is
added, the right solution is off-chain node storage in the provider process,
which already maintains a local cache of batch data. The provider rebuilds the
full MMR from `outbound_messages()` and serves per-message proofs from its local
store; no on-chain storage changes are needed.

**Status:** Resolved — see commit `22cd03d3f3d`.

**Recommendation:** Update §5.1 to document the peaks-only layout and add a note
on per-message proof generation as a provider-side extension.

---

## 6. `SourceState` — peaks-only fields match design intent

**Design section:** §5.2

**Design:** Shows `SourceState` containing `local_subtree: MMRState` (full struct).

**Implementation** (`cumulus/pallets/speculative-inbox/src/lib.rs`):
```rust
pub struct SourceState {
    pub last_processed: u64,
    pub last_seen_provides_root: H256,
    pub last_seen_subtree_root: H256,
    pub mmr_size: u64,        // leaf count
    pub mmr_peaks: Vec<H256>, // O(log n) peaks
}
```

The outbox uses the identical peaks-only representation (`MMRState { leaf_count,
peaks }`), so both sides are consistent. `mmr_size` counts leaves, not mmr_lib's
internal node count.

**Status:** Resolved — consistent with item 5.

**Recommendation:** Update §5.2 to document the peaks-only fields and clarify
that `mmr_size` / `leaf_count` counts leaves.

---

## 7. `PendingSpeculativeData` — removed, commitments read directly

**Design section:** §4.2

**Design:** Describes the two-phase backing/enactment model. Does not define any
intermediate storage item — the implies reading from the candidate's commitments
directly at enactment time.

**Earlier implementation:** Had a `PendingSpeculativeData: StorageMap<CandidateHash, ...>`
that extracted speculative fields at backing time and stored them separately.

**Current implementation:** `PendingSpeculativeData` has been removed.
`CandidatePendingAvailability` already stores the full `CandidateCommitments`
including `provides` and `requires` fields, so the separate map was redundant.
It was introduced when the `SpeculativeCommitments` wrapper used compact
`(Id, Hash)` tuples that were awkward to access; after that wrapper was removed
(item 3), reading directly from commitments became natural.

- Availability check reads `candidate.commitments.requires` directly.
- `enact_candidate` reads `commitments.provides` / `commitments.requires` directly.
- `free_failed_cores` needs no cleanup loop — no separate storage to clear.

**Status:** Resolved — see commit `cc1266e84b0`.

**Recommendation:** Update §4.2 to remove the `PendingSpeculativeData` mention
and describe the direct-commitments access pattern.

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
| 5 | §5.1 | Peaks-only outbox storage; `MMRNodes` removed | Resolved | Update design |
| 6 | §5.2 | Peaks-only inbox storage; consistent with item 5 | Resolved | Update design |
| 7 | §4.2 | `PendingSpeculativeData` removed; commitments read directly | Resolved | Update design |
| 8 | §5.2 | Position-0 first-message bug in design pseudocode | Open | Fix design |
| 9 | §6.2 | Old subtree Merkle proof verified in PVF only, not runtime | Open | Update design |
| 10 | §2 | Version gating uses feature flag + version byte, not new struct family | Open | Update design |
