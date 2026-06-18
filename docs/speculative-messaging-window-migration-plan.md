# Speculative Messaging — Window/UMP-Signal Migration Plan

Aligns our relay-side implementation with the parity-team design across three
issues. Created 2026-06-18. This file is the durable plan; the same items also
live in the session task tracker (#14–#27).

## Why
Our current POC relay side uses **provides/requires as `CandidateCommitments`
fields**, a **single latest provides root** per para, **Late Block Proofs as the
primary** staleness fix, and has **no dispute-revert handling**. The intended
design is:

- **[#12346](https://github.com/paritytech/polkadot-sdk/issues/12346)** — the
  `CommitmentSet` primitive (built ✓; lives in `polkadot-primitives::v9`).
- **[#12347](https://github.com/paritytech/polkadot-sdk/issues/12347)** — carry
  provides/requires via **`UMPSignal::ProvidesRoots(CommitmentSet)` /
  `RequiresRoots(CommitmentSet)`** (reuse `upward_messages`), **not** by extending
  `CandidateCommitments`. Migration caveat: older validators reject extra UMP
  signals (`TooManyUMPSignals`), so the UMP change must reach **⅔ of validators
  before** enabling the `SpeculativeMessaging` `node_features`.
- **[#12349](https://github.com/paritytech/polkadot-sdk/issues/12349)** — the
  relay keeps a **bounded window** of recent provides per `(source, dest)` and
  accepts a `requires` if its root is **in the window**; evict on dispute-reverts.

Design author's intent
([#10449 r3275526780](https://github.com/paritytech/polkadot-sdk/pull/10449#discussion_r3275526780)):
keep a few recent provides so the common "took a bit longer" case matches with
**no proof** — the window **reduces** Late Block Proofs, it does not eliminate
them. **Decision: keep LBP as a beyond-window fallback.**

What carries over (not wasted): the `CommitmentSet` primitive and the flat
per-destination subtree roots are exactly the `UMPSignal` payload (#12347) and the
window entries (#12349). The transport layer (fields → UMP signals) and the
matching layer (single root → window) are what change.

## Task breakdown

Status: ☐ not started · ◐ in progress · ☑ done

### Phase A — UMP-signal transport (#12347)
- **A1 (#14)** ☑ — Added `UMPSignal::ProvidesRoots/RequiresRoots(CommitmentSet)` in
  `polkadot/primitives/src/v9/mod.rs`; `CandidateUMPSignals` now has
  `provides_roots`/`requires_roots` (+accessors); `try_decode_signal` handles both;
  `ump_signals()` loops up to `MAX_UMP_SIGNALS = 4` (one per variant), still
  `TooManyUMPSignals` beyond that.
- **A2 (#15)** ☑ — `FeatureIndex::SpeculativeMessaging = 5`, `FirstUnassigned = 6`
  already in place (`v9/mod.rs:1757`). Verified.
- **A3 (#16)** ☑ ← A1 — `parachain-system::send_ump_signals` now appends
  `ProvidesRoots`/`RequiresRoots` UMP signals (non-empty only) from the existing
  `speculative_extension()` hook; `validate_block`'s signal-validation `match` gained
  pass-through arms for the two new variants (no cross-block consistency check).
  Signals flow through `upward_messages` and are reproduced by validate_block
  re-execution. (The old `ValidationResultExtension`/`speculative` field path is
  still present — A4 removes it.)
- **A4 (#17)** ☑ ← A3 — **Removed** `provides`/`requires` from `CandidateCommitments`
  and the node `Collation` struct; deleted the candidate-validation reconstruction
  (commitments rebuilt from the 6 standard fields — UMP signals are already inside
  `upward_messages`); collation-generation `has_speculative` now derived from the
  `ProvidesRoots`/`RequiresRoots` UMP signals; lookahead collator uses a boolean
  `is_speculative` (from the `requires_commitments` API) for fork suppression instead
  of patching the removed fields; stripped the two field initializers from ~14
  construction sites; cleaned unused imports + dead `dummy_*_commitment` helpers.
  **Kept** `ValidationResultExtension::V4` / `ValidationResult.speculative` — still
  feeds validate_block's LBP `apply_messaging_proofs`; **C1 removes them.**

### Phase B — Provides window + dispute eviction (#12349)
- **B1 (#18)** ☑ — `LatestProvides: StorageDoubleMap<source, dest,
  BoundedVec<ProvidesEntry<BlockNumber>, ConstU32<SPECULATIVE_PROVIDES_WINDOW=8>>>`
  with `ProvidesEntry { root, block }`, replacing the single-root `ProvidesRoots`.
  Helpers reworked to window semantics: `provides_contains` (membership),
  `requires_satisfied` (all-present-in-window), `update_provides` (push + evict
  oldest, block-tagged), `provides` (reconstruct latest set for the runtime API).
  `inclusion/mod.rs`. Note: call sites still read `commitments.provides/requires`
  fields — B2/B3 switch the input to the UMP signals.
- **B2 (#19)** ☑ ← A1, B1 — `enact_candidate` parses `commitments.ump_signals()` up
  front and pushes the `provides_roots()` set into the window via `update_provides`;
  the enactment-time `requires` re-check now reads `requires_roots()` (defensive only).
- **B3 (#20)** ☑ ← B1 — Added `speculative_requires_satisfied` in `paras_inherent`;
  the `sanitize_backed_candidates` filter drops a V4 candidate (when feature enabled)
  whose `RequiresRoots` signal isn't in the provides window. The
  inclusion/`process_candidates` availability check also switched off the
  `commitments.requires` field onto the UMP signal.
- **B4 (#21)** ☑ ← B1 — `inclusion::evict_provides_after(revert_to)` retains only
  window entries with `block <= revert_to` (removing emptied windows). Hooked in
  `paras_inherent` after dispute import: snapshots `is_frozen()` before, and if the
  import newly froze the chain, evicts using `DisputesHandler::frozen_block()` (new
  trait method returning `Frozen`). Note: this is **defense-in-depth** — Polkadot
  freezes on any concluded-invalid-against-included dispute and the node rolls back
  this storage via the state revert; the in-runtime eviction keeps the reverting
  block's own view consistent.
- **B5 (#22)** ☑ — `check_descriptor_version_and_signals` now drops any candidate
  carrying `ProvidesRoots`/`RequiresRoots` UMP signals while the feature is disabled
  (prevents window population at enactment with matching off).

### Phase C — LBP as beyond-window fallback
- **C1 (#23)** ☐ ← A3, B3 — Rework `validate_block::apply_messaging_proofs` to
  transform the `RequiresRoots` UMP signal (not the V4 field); applies only when
  the root is outside the window. Keep `SpecMerge`/`SubtreeExtension` +
  `verify_incremental`.
- **C2 (#24)** ☐ ← B3 — `speculative_ingress.rs`: fetch a `LateBlockProof` only
  when the batch root is **not in** the relay window for `(source, receiver)`
  (needs a window-read runtime API). Within-window → no LBP.

### Phase D/E — Tests + docs
- **D1 (#25)** ☐ — Relay isolation tests: window advances/bounds at `WINDOW_SIZE`,
  `evict_provides_after` rolls back on revert, membership match (in/out-of-window),
  feature-off drop, `UMPSignal` provides/requires encode/extract round-trip.
- **D2 (#26)** ☐ ← C1 — E2E LBP-fallback test (out-of-window root matches after
  PVF transform) + re-verify spec-messaging crate + outbox/inbox + cumulus-test
  runtime WASM `validate_block_works`.
- **E1 (#27)** ☐ — Update `docs/speculative-messaging-impl-design.md` to the
  UMP/window model + the #12347 migration caveat; refresh the §3.6 tracker.

## Critical path
`A1 → A3 → A4`; `A1,B1 → B2`; `B1 → B3 → C1 → D2`.
**Startable now:** A1, A2, B1, B5.

## Verified baseline (pre-migration, this session)
Crate 22→ (commitment_set moved out, now 12) · outbox · inbox 8/8 ·
`validate_block_works` ×2 on WASM · `cargo check --workspace` clean ·
no `polkadot → cumulus` dependency.
