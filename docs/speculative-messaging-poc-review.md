# Speculative Messaging POC — Code Review Findings

Branch: `ron/speculative-messaging-poc` vs `ron/speculative-messaging-poc-base`
Design reference: `docs/speculative-messaging-impl-design.md`

---

## What's Implemented (Matches Design)

- **Primitives (`polkadot/primitives/src/v10/mod.rs`)** — All required types: `ProvidesCommitment`, `RequiresCommitment`, `SpeculativeIngress`, `MessageBatch`, `OutgoingMessage`, `LateBlockProof`, `MMRExtensionProof`. `CandidateDescriptorVersion::V4`, feature bit `SpeculativeMessaging`, `SPECULATIVE_API_VERSION = 10`.

- **Backward compat** — `TrailingOption<SpeculativeCommitments>` on `v9::CandidateCommitments` preserves hash compatibility for pre-V4 candidates. V4 uses `version == 2` byte on the existing `CandidateDescriptorV2` struct — a pragmatic deviation from §2's "separate v10 descriptor" guidance but equivalent for version gating.

- **`pallet-speculative-inbox`** — `IncomingState`, `ConsumedSourcesThisBlock`, `ingest_verified_messages`, `ProvideInherent`, `get_requires_commitments`, and the MMR peaks-only storage pattern per §5.2.

- **`pallet-speculative-outbox`** — `OutgoingMMRState`, `MMRNodes`, `OutgoingMessages`, `HistoricalProvidesRoots`, `HistoricalSubtreeState`, `compute_provides_root`, `subtree_inclusion_proof`, `generate_late_block_proof`, `XcmpMessageSource` wrapper per §5.1. Adds `HistoricalSubtreeState` pruning in `on_finalize` (practical improvement not in the design).

- **Relay chain (`polkadot/runtime/parachains/src/inclusion/mod.rs`)** — `ProvidesRoots`, `PendingSpeculativeData`, `UnsatisfiedRequires`, `requires_satisfied`, `update_provides_root`, cleanup on drop per §4.1/§4.2.

- **PVF extension** — `ValidationResultExtension::V4` carries `provides_root` and `requires` from PVF back to node-side, propagated through `validate_block/implementation.rs`. `ParachainBlockDataV4` wrapper in `cumulus_primitives_core` per §6.2. Node-side (`candidate-validation/src/lib.rs`) unpacks the extension and builds `SpeculativeCommitments` for the commitments hash check.

- **Collator-side (`cumulus/client/consensus/aura/src/collators/speculative_ingress.rs`)** — Off-chain fetch, relay-chain root comparison, and late-block-proof detect path are sketched.

---

## Bugs Found

### Bug 1 — Double-hashing in `apply_messaging_proofs` (critical)

**File:** `cumulus/pallets/parachain-system/src/validate_block/implementation.rs:106,116`

`apply_messaging_proofs` pre-hashes the leaf with `Keccak256::hash(...)` before passing it to `binary_merkle_tree::verify_proof`, which hashes it **again** internally. This double-hash means the verification always fails, so Late Block Proof transformation in the PVF is never applied.

The inbox pallet does it correctly (raw SCALE bytes, no pre-hash). The fix is to match that pattern:
```rust
// Wrong (double-hash):
let old_leaf = Keccak256::hash(&(para_id, proof.old_subtree_root).encode());
// Correct (single hash inside verify_proof):
let old_leaf = (para_id, proof.old_subtree_root).encode();
```

---

### Bug 2 — `verify_mmr_extension` logic error: `None` treated as success (critical)

**Files:**
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs:64–70`
- `cumulus/pallets/speculative-inbox/src/lib.rs:367–378`

In `implementation.rs`, the check is:
```rust
let old_computed = bag_mmr_peaks::<Keccak256Merge>(&ext.old_peaks);
if old_computed.map_or(false, |r| r != old_root) {  // bug
    return false;
}
```
When `ext.old_peaks` is empty, `old_computed` is `None`. `map_or(false, ...)` returns `false`, so the guard is not entered — an empty peak set passes silently as a valid proof of any `old_root`.

Fix: compare directly against `Some(old_root)`:
```rust
if old_computed != Some(old_root) { return false; }
```

In `speculative-inbox/src/lib.rs`, `bag_peaks` returns `H256::zero()` for empty peaks (via `Default`), so an empty peak set passes if `old_root == H256::zero()`. Fix: add explicit empty-check before bagging.

Note: Both implementations also do not validate the ancestry relationship via `connecting_nodes` (acknowledged in the code comment as a POC limitation). This is a separate, lesser concern for the POC, but should be addressed before production.

---

### Bug 3 — `requires_satisfied` checked at backing time, contradicting §4.2

**File:** `polkadot/runtime/parachains/src/inclusion/mod.rs:748–762`

`process_candidates` (backing) calls `requires_satisfied` and returns `UnsatisfiedRequires` if it fails. The design (§4.2) explicitly states: satisfaction must only be checked at **enactment**, not backing. Checking at backing means valid candidates are rejected prematurely when their source hasn't been enacted yet — the common case during normal operation.

Fix: remove the `requires_satisfied` check and early-return from `process_candidates`. Keep `store_pending_speculative` so the data is available at enactment time.

---

### Bug 4 — Double `take_pending_speculative` / double `update_provides_root`

**File:** `polkadot/runtime/parachains/src/inclusion/mod.rs:611–618` and `1038–1048`

The eviction loop in `update_pending_availability_and_get_freed_cores` calls `take_pending_speculative` and `update_provides_root` for enacted candidates. Then `enact_candidate` tries to call `take_pending_speculative` again, gets `None`, and silently skips its defensive check and provides-root update.

Additionally, the eviction loop uses the outer loop variable `paraid` (the storage key) when calling `update_provides_root`, whereas `enact_candidate` correctly uses `receipt.descriptor.para_id()` — these can differ if the storage key doesn't match the descriptor field.

Fix: remove speculative handling from the eviction loop. `enact_candidate` already has the correct logic and is the right place to own it.

---

## Minor Issues / Not Bugs

- **`LateBlockProof.source` hardcoded to `0u32`** in `generate_late_block_proof` with a comment "Caller will fill". The client code in `speculative_ingress.rs` does not appear to fill it. The source field would be wrong for any generated proof until this is fixed.

- **Outbox wrapping granularity** — `XcmpMessageSource::take_outbound_messages` records each message as a single-element vec `vec![data.clone()]`. The `data` here is the full XCMP page, not an individual XCM message. The MMR leaf should be the hash of the individual XCM payload, not the entire page. Needs verification that sender and receiver hash the same unit.

- **Linear scan in `block_number_for_provides_root`** — `HistoricalProvidesRoots` is scanned linearly to find a block by its root value. This is O(N) in the retention window and can be made O(1) by adding a reverse index `(H256 → BlockNumber)`. Acceptable for POC but worth tracking.

- **`pos` variable unused in `record_outbound_messages`** — The `pos` binding at line 647 of `speculative-outbox/src/lib.rs` is assigned but never used; `leaf_idx` is what's actually stored. Minor cleanup.

---

## Missing Pieces (Not Yet Implemented)

These are acknowledged in the design as out-of-scope for the POC but are worth tracking:

- No runtime API wiring for `SpeculativeInboxApi` / `SpeculativeOutboxApi` in the penpal runtime (referenced by collator-side code but trait definitions and `impl_runtime_apis!` blocks are not visible in the diff).
- No `PSC::speculative_extension()` trait method definition visible in the parachain-system pallet changes — only the call site is present.
- No collator-side resubmission loop (§7.4 retry) — the design acknowledges this as minimal POC scope.
- No weight benchmarks for `ingest_verified_messages`.
- No test for the full end-to-end path including relay-chain enactment satisfaction.
