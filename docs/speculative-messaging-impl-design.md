# Speculative Messaging — Minimal POC Implementation Design

Based on [speculative-messaging-design.md](speculative-messaging-design.md).

This is the **single source of truth** for the minimal speculative-messaging POC
on the current codebase, covering implementation design, end-to-end workflow,
off-chain networking, and the follow-up roadmap.

**Phase 1 scope — Inclusion-based messaging with Late Block Proofs.** Removes
message storage from relay chain state while keeping latency at ~6–12s (1–2 relay
blocks for inclusion). This is the first implementation slice of the broader
offchain-XCMP replacement direction — the conservative inclusion-based path of
the general commitment-driven speculative messaging model.

The POC includes Late Block Proofs (section 6.2) so that receivers can
successfully enact candidates even when the source chain's `provides_root` has
advanced between block building and enactment — a normal case under any realistic
backing pipeline, not just core-on-demand chains. A minimal collator resubmission
loop (section 7.4) provides basic eventual-delivery behavior: if a candidate is
rejected, the collator fetches fresh data and retries. Full eventual-delivery
guarantees (bounded catch-up, persistent queues, production retry policy) are
deferred to section 12.

> **Design revision (2026-06-17).** Two decisions supersede earlier drafts of
> this doc and are now canonical:
>
> 1. **Hashing is a type parameter `H`, instantiated to `blake2_256`.** The crate
>    primitives (`hash_leaf::<H>`, `SpecMerge<H>`, `root_from_peaks::<H>`) are
>    generic over the hash function; the protocol's single
>    concrete choice is `polkadot_primitives::v9::SpecHasher = BlakeTwo256`
>    (Substrate-native), so a future switch (e.g. to Keccak256) is one line. Any
>    remaining "Keccak256" mention below is legacy and should be read as
>    `SpecHasher`/`blake2_256`.
> 2. **The top-level commitment is flat, not a two-level Merkle tree.** A
>    sender's `provides` commitment is a canonical, sorted
>    `CommitmentSet` of `(destination, subtree_root)` entries — there is no
>    Merkle root bagging the per-destination subtree roots. The per-destination
>    MMR (messages → `subtree_root`) is unchanged. This removes top-level
>    inclusion proofs everywhere: `MessageBatch` loses `subtree_inclusion_proof`,
>    `LateBlockProof` loses its `*_subtree_proof` fields, and relay matching
>    becomes a lookup (`provides.get(receiver) == expected_root`) instead of a
>    hash comparison against a single root. Rationale and tradeoff analysis:
>    [paritytech/polkadot-sdk#10449 (discussion r3275189534)](https://github.com/paritytech/polkadot-sdk/pull/10449#discussion_r3275189534).
>
> The parachain-side primitives live in the `cumulus-primitives-spec-messaging`
> crate ([paritytech/polkadot-sdk#12368](https://github.com/paritytech/polkadot-sdk/pull/12368),
> closes #12346): `OutgoingMessage::hash_leaf`, the domain tags, and `SpecMerge`
> (the `mmr_lib::Merge` that backs the per-destination MMR). The relay-visible
> `CommitmentSet` lives in `polkadot-primitives::v9` (it's embedded in
> `CandidateCommitments`), so there is **no `polkadot → cumulus` dependency**.
>
> 3. **The per-destination MMR is built on `mmr_lib`** (`polkadot-ckb-merkle-mountain-range`)
>    via the crate's domain-tagged `SpecMerge<H>`: **inclusion** proofs
>    (`gen_proof`/`verify`) and append-only **extension** proofs (ancestry /
>    `verify_incremental`) come from `mmr_lib`. The crate retains a thin
>    `MmrAccumulator` trait (`append`/`root`/`size`) and a peaks-only `Mmr<H>` impl
>    (used by the sender's on-chain state), plus `root_from_peaks::<H>` — kept
>    consistent with `mmr_lib` (the `Mmr` root equals `mmr_lib`'s and its proofs
>    verify against it). PR #12368's single-shot `merge_peaks` bagging is replaced by
>    `mmr_lib`-consistent pairwise bagging. (The empty-MMR sentinel `empty_root` /
>    `EMPTY_TAG` and the `extension_proof` stub were **removed** — `mmr_lib` errors on
>    an empty MMR rather than producing a root, so a committed "empty root" is an
>    unverifiable value the protocol never emits; `root_from_peaks` requires a
>    non-empty peak set.)

> **Design revision (2026-06-18) — UMP-signal transport + provides window.**
> The relay side was migrated to the parity-team design. **Full task breakdown,
> dependencies, and status: [speculative-messaging-window-migration-plan.md](speculative-messaging-window-migration-plan.md).**
> The sections below that describe `CandidateCommitments.provides/requires`,
> a single `ProvidesRoots[source]` entry, or `apply_messaging_proofs` mutating a
> `ValidationResultExtension` are **superseded** by:
>
> 4. **Provides/requires travel as UMP signals, not `CandidateCommitments` fields**
>    ([#12347](https://github.com/paritytech/polkadot-sdk/issues/12347)). Block
>    execution emits `UMPSignal::ProvidesRoots(CommitmentSet)` /
>    `RequiresRoots(CommitmentSet)` into `upward_messages` (so they are covered by
>    the commitments hash); the relay reads them via `CandidateCommitments::
>    ump_signals()`. The `provides`/`requires` fields and the
>    `ValidationResult.speculative` extension field are **removed**. Migration
>    safety: a candidate carrying speculative UMP signals is dropped while the
>    `SpeculativeMessaging` node-feature is off, and the feature must reach ⅔ of
>    validators before enabling (older validators reject extra signals with
>    `TooManyUMPSignals`).
> 5. **The relay keeps a bounded provides *window***
>    ([#12349](https://github.com/paritytech/polkadot-sdk/issues/12349)).
>    `LatestProvides: (source, dest) → BoundedVec<ProvidesEntry{root, block},
>    MAX_PROVIDES_WINDOW_SIZE>` replaces the single latest root (operational length =
>    `HostConfiguration::provides_window_size`, default 8, clamped to the static max;
>    added via configuration migration v13→v14). A `requires`
>    matches if its root is **present anywhere in the window** (membership), so a
>    slightly-stale-but-recent root matches with no proof. The window is populated
>    at enactment from the `ProvidesRoots` signal, matched in
>    `sanitize_backed_candidates`, and `evict_provides_after(revert_to)` drops
>    entries from dispute-reverted blocks.
> 6. **Late Block Proofs transform the `RequiresRoots` UMP signal** (kept as a
>    *beyond-window* fallback). The shared
>    `cumulus_primitives_spec_messaging::apply_late_block_proofs(signals, proofs)`
>    rewrites `(source, old_root) → (source, new_root)` via
>    `SpecMerge`/`verify_incremental`. Both the collator (before computing
>    commitments) and the PVF (`validate_block`) call it on the identical signals,
>    so the resulting `upward_messages` — and the commitments hash — agree
>    byte-for-byte (a bad proof is rejected by the hash check). This keeps §6.2's
>    two-phase model; only the transform *target* changed (UMP signal, not the
>    removed extension). The collator is **window-aware**: a `provides_window(source,
>    dest)` runtime API (threaded through `RelayChainInterface`) lets
>    `speculative_ingress` skip the LBP whenever the batch root is already in the
>    relay window, fetching a proof only when it is older than the whole window.

### Why UMP signals, not a `CandidateCommitments` field?

Both approaches end up covered by the same `commitments_hash`, so the benefit is
**not** integrity — it is **migration safety and blast radius**:

1. **No change to the `CandidateCommitments` wire format.** That type is consensus
   core — it is in the candidate receipt that is gossiped, backed, included, and
   stored for disputes, and every validator decodes it. Adding fields is a hard
   format change requiring a lockstep runtime+node upgrade. UMP signals ride inside
   `upward_messages` (an existing field) after the established `UMP_SEPARATOR`, so the
   struct layout is unchanged; code that doesn't understand the new signals just sees
   extra upward messages instead of failing to decode the candidate.
2. **Clean feature-gating, no descriptor/commitments version bump.** Because the data
   lives in an existing field, rollout is a `node_features` flag plus the feature-off
   drop guard (B5): ship the signal-aware code, wait until ≥⅔ of validators run it,
   then enable. The only compat concern is older validators rejecting extra signals
   with `TooManyUMPSignals` — handled by that threshold. A dedicated field can't be
   feature-gated this cleanly.
3. **Reuses the existing UMP-signal channel.** `UMPSignal::SelectCore`/`ApprovedPeer`
   already establish "a parachain signals the relay about a candidate via
   `upward_messages`". provides/requires are the same kind of thing — two more
   variants, not a parallel field-based channel.
4. **The data is produced by block execution anyway.** provides/requires come from
   running the runtime (outbox/inbox state); the signals are emitted *during* that
   execution (`send_ump_signals` in `on_finalize`) straight into `upward_messages`, so
   `validate_block`'s re-execution reproduces them for free — no side-channel out of
   the PVF that has to be reconciled.
5. **Concretely smaller blast radius.** The field path required a
   `ValidationResultExtension::V4` side-channel out of the PVF, candidate-validation
   reconstructing the field and re-hashing it, collation-generation mapping, the node
   `Collation` fields, and ~14 construction sites — **all deleted in A4**. The signal
   path is "the runtime emits it during execution; the relay reads it with
   `CandidateCommitments::ump_signals()`." It also made the LBP rework (C1) cleaner:
   the transform rewrites the signal *in* `upward_messages`, and the existing
   commitments hash enforces collator↔PVF symmetry — no separate field to keep in sync.

**Trade-off:** the relay decodes `UMPSignal`s out of `upward_messages` (bounded by
`MAX_UMP_SIGNALS`) rather than reading a typed field, and older validators must be
upgraded past the `TooManyUMPSignals` rejection before the feature is enabled — cheap
and bounded, which is why the design accepts it. In one line: **UMP signals let
speculative messaging ship as a feature-gated, backward-compatible addition instead of
a consensus-breaking change to the candidate-receipt format.**

---

## 1. Core Concept and End-to-End Workflow

The POC keeps one critical rule: **nothing consensus-critical happens only
off-chain**. Off-chain logic may fetch, cache, and precheck batches, but
validators never trust that by itself. The actual consensus path is:

1. the sender runtime executes and produces a `provides` root
2. the receiver collator fetches candidate ingress data from a relayer/provider
3. the receiver embeds that ingress into the block body
4. the receiver runtime re-verifies and executes it
5. the PVF replays the same block deterministically
6. the relay chain checks `requires` against `provides` at enactment

That is what makes this design practical on the current architecture: it reuses
the existing parachain lifecycle instead of inventing a second execution path.

### 1.1 Workflow Diagram

```
 Chain A (Sender)                         Chain B (Receiver)
 ════════════════                         ══════════════════

 1. Execute block                         3. Pull MessageBatch off-chain
    - Produce outbound XCM                    - Fetch from relayer/provider
    - Update per-destination MMR              - Precheck proof + continuity
    - Derive cumulative provides root

 2. Emit ProvidesCommitment { root }      4. Build receiver block
    in CandidateCommitments                   - Embed SpeculativeIngress inherent
    and retain recent batch/proof data        - Re-verify in runtime
                                               - Dispatch through XCMP handler
                                               - Record requires for this block
                                           ═══════════════════════════════════════

                                           Relay Chain
                                           ═══════════════════════════════════════
                                           5. Backing / PVF
                                              - Replay block deterministically
                                              - Return provides / requires in v4 validation result

                                           6. Enactment / inclusion
                                              - Match requires against:
                                                latest persisted provides root
                                              - Update ProvidesRoots only after actual enactment
```

### 1.2 Detailed Walkthrough

**Step 1 — Sender block execution.** The source parachain collator builds a block
normally. During runtime execution, outbound sibling-parachain XCM is produced
through the existing path. A speculative outbox wrapper records the payloads into
per-destination MMR/subtree state, and the sender's cumulative top-level
`provides` root becomes derivable from the resulting runtime state. See section 5.1.

**Step 2 — Sender-side data is stored on-chain.** The sender chain stores
outbound payload bytes in `OutgoingMessages` and MMR state in `OutgoingMMRs`.
No additional work happens on the sender chain — the data persists in its
runtime storage naturally as a result of block execution (step 1).

An optional **provider** process (§7.2) monitors the sender chain, queries its
runtime APIs (`provides_root`, `destination_state`, `outbound_messages`,
`subtree_inclusion_proof`), and caches the results as `MessageBatch` structs.
This caching layer is a convenience — it decouples the receiver collator from
needing a direct RPC connection to the source chain. The receiver collator
could query the source chain directly; the provider just amortizes work across
multiple receivers and provides a simpler HTTP interface.

**Step 3 — Receiver collator fetches and prechecks.** Before proposing its own
block, the destination collator fetches recent batches from a provider and
performs a local precheck: verify the subtree inclusion proof, verify message
positions are consecutive, verify local subtree continuity. See section 7.4.

**Step 4 — Receiver embeds `SpeculativeIngress`.** The receiver collator converts
accepted batches into `SpeculativeIngress`, inserts it into `InherentData`, and
the runtime constructs an inherent-style call in the block body. See section 3.3.

**Step 5 — Receiver runtime re-verifies and dispatches.** The runtime re-verifies
each embedded batch against on-chain state: subtree proof, message ordering and
continuity, updates `IncomingState`, records consumed source roots, and
dispatches payloads through the existing XCMP handler. See section 5.2.

**Step 6 — Collator assembles `provides` and `requires`.** After execution, the
collator reads the speculative outputs from runtime state: sender-side cumulative
`provides` and receiver-side `requires`. These populate the candidate
commitments. See section 5.3.

**Step 7 — PVF replays the same block deterministically.** Backing validators
execute the wasm PVF over the candidate's `block_data`. Since `SpeculativeIngress`
was embedded in the block body, validators replay the same ingress call and
produce the same `provides`/`requires`. See section 6.

**Step 8 — Node-side candidate validation reconstructs commitments.** After the
PVF returns, candidate validation reconstructs commitments from the validation
outputs and checks the hash against the candidate receipt. See section 6.1.

**Step 9 — Relay-chain enactment checks dependency satisfaction.** At enactment
time, the relay chain checks every `RequiresCommitment` against the latest
persisted provides root, then updates `ProvidesRoots[source]` on success.
See section 4.2.

The detailed implementation order, including specific files and modules for each step, is in section 10.

### 1.3 Protocol Pipeline (End-to-End)

How our design maps onto the existing parachain–relay-chain communication flow.

**Phase 1 — Collator builds the block**

1. **Fetch off-chain data** (§7.4). Collator queries provider for `MessageBatch`es.
   Prechecks proofs and message continuity. If source root has advanced, also
   fetches and prechecks `LateBlockProof` (§6.2).
2. **Assemble inherents and PoV** (§3.3, §6.2). Collator creates `InherentData`:
   parachain-system data + `SpeculativeIngress` (batches). Wraps block data and
   `LateBlockProof`s in `ParachainBlockData::V2` and encodes as the PoV content.
3. **Execute block** (§5.1, §5.2). Runtime executes. Outbox wrapper records
   outbound XCM into `OutgoingMMRs`. `ingest_verified_messages` verifies batches,
   updates `IncomingState`, dispatches XCM, records consumed sources.
4. **Collect outputs** (§5.3). Collator calls `compute_provides_root()` and
   `requires_commitments()` via runtime API. Overrides requires with
   LateBlockProof transformed roots. Assembles `CandidateCommitments`.
5. **Build receipt**. Collator hashes commitments → `commitments_hash`. Builds
   `CommittedCandidateReceipt` with descriptor + hash + signature. Submits
   (PoV, receipt) to backing validators.

**Phase 2 — Backing**

6. **PVF execution** (§6, §6.2). Each backing validator spins up Wasm sandbox,
   loads the parachain's Wasm blob, calls `validate_block` with the PoV. PVF
   decodes `ParachainBlockData::V2` from the PoV bytes (getting both block data and
   `LateBlockProof`s in one call), executes the block deterministically —
   same inherents, same `ingest_verified_messages`, same outbox updates. Verifies
   each late block proof via `apply_messaging_proofs`, transforms requires in
   `ValidationResultExtension::V4`. Returns `ValidationResult` with populated
   `speculative` field.
7. **Commitments reconstruction** (§6.1). Node-side validation extracts
   `ValidationResultExtension::V4` from `result.speculative.0`, reconstructs
   `CandidateCommitments` from the outputs, hashes, checks against the
   receipt's `commitments_hash`. Match → commitments are valid. Validators sign,
   candidate enters `PendingAvailability`.

**Phase 3 — Inclusion / Enactment**

8. **Dependency check** (§4.2). Relay block author decides which pending
   candidates to include. For each v4 candidate, the relay chain checks every
   `RequiresCommitment.expected_root` against persisted `ProvidesRoots[source]`.
   Unmet → `UnsatisfiedRequires`, candidate dropped.
9. **Enact** (§4.1). `enact_candidate()` runs. For v4 candidates with
   `ProvidesCommitment`: update `ProvidesRoots[para_id]`.

**Phase 4 — Availability & Finality**

10. PoV is erasure-coded and distributed. Relay chain finality confirms the
    candidate is canonical. `ProvidesRoots[source]` is persisted and available
    for future receiver blocks until the source produces a new provides root.

---

## 2. Commitments Versioning Strategy

**Original intent:** new types go into a new `v10` primitives module, `v9` types frozen,
v4 candidates use `CandidateCommitments` while legacy candidates use `v9`.

**POC implementation (diverges from intent):** Speculative messaging primitives are
defined in `polkadot/primitives/src/v9/speculative.rs`. The existing
`v9::CandidateCommitments` was extended directly with `provides` and `requires` fields.
The separate `v10` primitives module was removed for simplicity.

This means `v9` is **not** frozen. The rationale: maintaining two separate commitment
structs would require updating every site that constructs or matches on
`CandidateCommitments`. Since enforcement is v4-descriptor-gated (the relay chain only
checks `provides`/`requires` for v4 candidates), correctness is preserved — pre-v4
candidates carry the new fields as `None`/empty but the relay chain ignores them.

In production, the right approach is to keep `v9::CandidateCommitments` frozen and
introduce a genuinely additive `CandidateCommitments` with a separate codec path.
For the POC the single-struct approach is a deliberate simplification.

```
polkadot/primitives/src/v9/speculative.rs  ← NEW MODULE
```

A `CandidateDescriptor` version bump signals that the parachain supports
speculative messaging. The current codebase centers on `CandidateDescriptorV2` /
`CandidateReceiptV2` / `CommittedCandidateReceiptV2` with a reserved-byte
pattern for backward-compatible version detection. "v4" in this document
should be read as **the next concrete speculative-capable descriptor/receipt
version** — the important point is the **version-gated coexistence model**, not
the literal version numeral.

**Implementation approach — version byte on existing V2 struct.** The POC reuses
the existing `CandidateDescriptorV2` struct rather than introducing a new
`CandidateDescriptorV4` type. The constructor `CandidateDescriptorV2::new_v4()`
writes version byte `2` (the next available value in the reserved version field
inside V2's layout), making `descriptor.version()` return
`CandidateDescriptorVersion::V4`. Speculative field population is gated on
`FeatureIndex::SpeculativeMessaging` being set in node features, checked in
`collation-generation` and `candidate-validation`.

This avoids updating every site that pattern-matches on descriptor versions and
keeps the diff minimal. The version-byte mechanism inside `CandidateDescriptorV2`
already handles multi-version coexistence via `descriptor.version()`. In
production, a separate struct would provide better type safety; for the POC the
version-byte approach is sufficient.

Concretely, the enforcement behavior:

- legacy (pre-v4) candidates carry `provides: None, requires: []` — relay chain ignores these fields
- v4 candidates use the extended commitments layout with speculative messaging fields enforced
- relay-chain inclusion only checks requires/provides matching for v4+ candidates
- node-side candidate validation reconstructs commitments according to the candidate descriptor version

Every component that touches commitments is version-aware:

- **Collator** (§5.3): branches on `api_version` to include speculative fields for
  v4 parachains, skips them for legacy.
- **Relay chain backing** (§4.2): `process_candidates` accepts both formats
  unchanged into `PendingAvailability`.
- **Relay chain enactment** (§4.2): only enforces `requires`/`provides` matching
  for `descriptor.version() >= V4` candidates.
- **Node-side validation** (§6.1): reconstructs commitments the same way for both
  (since only `v9` is used in the POC), but only extracts
  speculative fields from `ValidationResultExtension::V4` for v4 candidates.
- **Relay chain runtime API** (`check_validation_outputs`): accepts the extended
  type, ignoring speculative fields for pre-v4 candidates.

```rust
// In v9/mod.rs — extended in place for the POC:
pub struct CandidateCommitments<N = BlockNumber> {
    pub upward_messages: UpwardMessages,
    pub horizontal_messages: HorizontalMessages,
    pub new_validation_code: Option<ValidationCode>,
    pub head_data: HeadData,
    pub processed_downward_messages: u32,
    pub hrmp_watermark: N,

    // ── Speculative messaging fields (None/empty for pre-v4 candidates) ──
    pub provides: Option<ProvidesCommitment>,
    pub requires: Vec<RequiresCommitment>,
}
```

Additional structural rules for `CandidateCommitments` in v4:

- `requires` must be in a **canonical order**, sorted by `source: ParaId`
- there must be at most **one `RequiresCommitment` per source parachain**
- duplicate sources must be rejected before hashing / inclusion
- `requires` should be **bounded** at the type or protocol level for production code; the POC may start with `Vec` but should define a concrete maximum

These rules are important because commitments are hashed. Two semantically
equivalent but differently ordered `requires` vectors must not lead to different
candidate commitments hashes.

---

## 3. Primitives (polkadot-primitives v9::speculative)

### 3.1 Commitment Types

Both commitments are expressed as a `CommitmentSet` — a canonical, sorted,
bounded set of `(ParaId, Hash)` entries from `cumulus-primitives-spec-messaging`.
`CommitmentSet` keeps entries sorted by `ParaId` and rejects out-of-order or
duplicate entries **on decode**, so collator, PVF, and relay chain always produce
and accept the same bytes for the same logical set.

```rust
use cumulus_primitives_spec_messaging::commitment_set::CommitmentSet;

/// A parachain's outbound commitments for one block.
///
/// One `(destination, subtree_root)` entry per destination that received
/// messages this block, where `subtree_root` is the root of that destination's
/// per-destination MMR (§5.1). This flat set **is** the top-level commitment —
/// there is no second-level Merkle root bagging the subtree roots.
pub type ProvidesCommitment = CommitmentSet<MAX_DESTINATIONS_PER_BLOCK>;

/// A parachain's inbound dependencies for one block.
///
/// One `(source, expected_root)` entry per source parachain we consumed messages
/// from, where `expected_root` is the source's per-destination subtree root *for
/// this receiver* (the MMR root over the messages the source sent to us, in the
/// source block we built against). The relay chain matches each entry by looking
/// it up in the source's persisted `ProvidesCommitment`.
pub type RequiresCommitment = CommitmentSet<MAX_SOURCES_PER_BLOCK>;
```

`CandidateCommitments` carries `provides: Option<ProvidesCommitment>` (None when
the block sends nothing) and `requires: RequiresCommitment` (empty when the block
consumes nothing).

This shape is intentional: subtree roots are no longer hidden behind a top-level
Merkle root. Each `(destination, subtree_root)` entry is directly observable in
the sender's `provides` set, so the relay chain — and any future light-client —
can match a receiver's `expected_root` against the sender's committed subtree
root for that receiver by a simple lookup, with **no inclusion proof**.

Two invariants are now enforced by the `CommitmentSet` type itself rather than by
convention:

1. **Canonicalization** — entries are sorted by `ParaId` ascending with at most
   one entry per `ParaId`; the manual `Decode` impl rejects any other ordering.
   Semantically equivalent sets therefore produce identical `CandidateCommitments`
   hashes. (Subsumes the old "canonicalization of `requires`" rule.)

2. **No top-level root construction.** There is no keyed-leaf Merkle tree over the
   subtree roots anymore. The only hashing on the provides side is (a) the
   per-message leaf hash (`OutgoingMessage::hash_leaf`, §3.4) and (b) the
   per-destination MMR root (`mmr_lib` + `SpecMerge`, §5.1). Both use
   `blake2_256` with domain tags.

### 3.2 Off-Chain Types

```rust
/// A message batch sent off-chain between collators.
#[derive(Clone, Encode, Decode, Debug)]
pub struct MessageBatch {
    /// Source parachain
    pub source: ParaId,
    /// Source block hash that produced these messages
    pub source_block: Hash,
    /// Relay-chain block number associated with the source batch when dispatching
    /// through the existing `XcmpMessageHandler` interface.
    ///
    /// This is the source chain's relay parent block number at the time the source
    /// block executed — available in the sender runtime as
    /// `frame_system::Pallet::<T>::parent_number()` or equivalent.
    pub source_relay_parent_number: RelayChainBlockNumber,
    /// The per-destination MMR root for the receiver. This is the value the
    /// receiver puts in `RequiresCommitment.expected_root`; the relay chain
    /// matches it directly against the source's committed `(receiver, root)`
    /// entry — no top-level inclusion proof (flat commitment, §3.1).
    pub subtree_root: Hash,
    /// Total number of MMR nodes (positions, not leaves) in the per-destination
    /// subtree at the moment this batch was generated. Required to reconstruct
    /// the `mmr_lib::MerkleProof` for `messages_proof` verification.
    pub subtree_mmr_size: u64,
    /// Combined MMR inclusion proof over every leaf in `messages`, against
    /// `subtree_root`, using `mmr_lib` with the crate's domain-tagged `SpecMerge`
    /// (§5.1). Generated by `mmr_lib::MMR::gen_proof` and verified by
    /// `MerkleProof::<Hash, SpecMerge>::new(subtree_mmr_size, messages_proof)
    /// .verify(subtree_root, leaves)` where each leaf is
    /// `(mmr_lib::leaf_index_to_pos(msg.position), msg.hash_leaf())`.
    pub messages_proof: Vec<Hash>,
    /// The messages, sorted by `position` ascending. Verified collectively by
    /// `messages_proof` against `subtree_root`.
    pub messages: Vec<OutgoingMessage>,
}
```

`OutgoingMessage` is the canonical type from
`cumulus-primitives-spec-messaging`:

```rust
pub struct OutgoingMessage<MaxMsgLen: Get<u32>> {
    /// The parachain that sent this message.
    pub source: ParaId,
    /// The parachain this message is addressed to.
    pub destination: ParaId,
    /// Zero-based sequence number within the `(source, destination)` channel —
    /// the leaf index in the source's per-destination MMR.
    pub position: u64,
    /// Raw XCM message bytes, bounded to `MaxMsgLen`.
    pub payload: BoundedVec<u8, MaxMsgLen>,
}
```

`source`/`destination` are carried explicitly because they are bound into the
leaf preimage by `hash_leaf()` (§3.4), which is what cryptographically ties a
message to its `(source, destination, position)` and prevents cross-channel
replay/forgery.

For the minimal POC, this shape is sufficient. The flat commitment means the
batch no longer carries a top-level `provides_root` or a `subtree_inclusion_proof`
— the receiver's `subtree_root` is matched directly by the relay chain (§4). The
batch still lets the receiver verify per-source ordered continuity against local
state, verify each message is in the sender's subtree via `messages_proof`, and
dispatch the verified payloads through the existing XCMP batch handler.

Invariants:

1. **Subtree root is the commitment.** `subtree_root` is exactly what the source
   committed as its `(receiver, subtree_root)` entry in `provides` and what the
   relay chain looks up. The destination parachain is not carried in
   `MessageBatch` because the receiver already knows "this batch is for me," but
   it *is* bound into each message's `hash_leaf()`.

2. **Canonical message ordering** — `messages` must be ordered by ascending
   `position` with no duplicates. During verification, the receiver expects them
   to advance continuously from `last_processed + 1`.

3. **Batch-to-root consistency** — `subtree_root` commits via `messages_proof` to
   each message's `hash_leaf()` leaf. The receiver checks this link, then checks
   `subtree_root` against the relay-persisted provides entry (via `requires`).

4. **Practical bounds** — `messages_proof` and `messages` should have explicit
   bounds in a production implementation; `payload` is already bounded by
   `MaxMsgLen`. The POC pseudocode can leave the proof/list as `Vec`, but the
   implementation should define concrete maxima.

### 3.3 Deterministic Ingress Types

Off-chain fetch is only a transport step. For deterministic execution, the
verified batches that a collator wants to consume in a block must be embedded in
the block itself via an inherent-like call. Validators then replay that same
input when executing the block inside the PVF.

```rust
/// Block input carried in the parachain block body.
/// This is the canonical ingress payload for speculative messaging.
#[derive(Clone, Encode, Decode, Debug)]
pub struct SpeculativeIngress {
    /// Verified batches selected by the collator for this block.
    pub batches: Vec<MessageBatch>,
}
```

For Phase 1, `SpeculativeIngress.batches` follows simple canonical selection
rules: batches are grouped logically per `source`, for a given source they appear
oldest-to-newest, and duplicate or overlapping batches for the same source in a
single block should be rejected by both collator precheck and runtime
re-verification.

Phase 1 uses a single inherent-like dispatch, following the same pattern as
`ParachainSystem::set_validation_data`: a node-local component fetches batches
off-chain, `ProvideInherent` turns them into a block-body call, the runtime
re-verifies deterministically, and `validate_block` replays the same call.

```rust
SpeculativeInbox::ingest_verified_messages { ingress: SpeculativeIngress }
```

The wiring:

```rust
// Registration: defined in `cumulus-pallet-speculative-inbox` and re-exported
// from its `client` module.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"specingr";

// client-side before proposal
let mut inherent_data = other_inherent_providers.create_inherent_data().await?;
inherent_data.put_data(INHERENT_IDENTIFIER, &ingress)?;

// runtime-side during block construction
impl<T: Config> ProvideInherent for Pallet<T> {
    const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

    fn create_inherent(data: &InherentData) -> Option<Self::Call> {
        let ingress = data.get_data::<SpeculativeIngress>(&Self::INHERENT_IDENTIFIER)
            .ok()
            .flatten()?;
        Some(Call::ingest_verified_messages { ingress })
    }
}
```

Validators do not trust the collator's off-chain fetch — they only re-verify the
batch data present in the block body.

If `ingest_verified_messages` depends on fresh parachain-system state written by
`set_validation_data`, two ordering requirements apply:

1. **Extrinsic ordering.** The collator's inherent assembly must place
   `ParachainSystem::set_validation_data` before
   `SpeculativeInbox::ingest_verified_messages` in the block body. The inherents
   provider (§3.3) should inject `SpeculativeIngress` under its own identifier
   key, and the collator must ensure `set_validation_data` is submitted first.

2. **`construct_runtime!` ordering.** The POC runtime (e.g., Penpal) must list
   `SpeculativeInbox` **after** `ParachainSystem` in `construct_runtime!`. This
   ensures `on_initialize`/`on_finalize` hooks run in the correct order — in
   particular, `SpeculativeInbox::on_initialize` (which clears
   `ConsumedSourcesThisBlock`) runs after `ParachainSystem::on_initialize`.

### 3.4 Message Payload Format

`OutgoingMessage.payload` contains raw XCM bytes — the same blob that the
receiver wants to deliver. During ingress execution, the runtime re-batches the
verified messages into the aggregate XCMP wire format expected by the configured
`T::XcmpMessageHandler::handle_xcmp_messages` interface. No new message-execution
trait is introduced for Phase 1; speculative ingress adapts to the existing XCMP
batch handler shape.

**Leaf hashing (`hash_leaf`).** A message becomes an MMR leaf via
`OutgoingMessage::hash_leaf`, which `blake2_256`-hashes a domain-tagged,
versioned preimage:

```text
LEAF_TAG ++ LEAF_VERSION ++ source ++ destination ++ position.to_le_bytes()
         ++ (payload.len() as u32).to_le_bytes() ++ payload
```

The `LEAF_TAG` (distinct from `INNER_TAG`/`PEAK_TAG` used by the MMR
merges) prevents a leaf hash from being reinterpreted as an inner/peak node, and
`LEAF_VERSION` lets the leaf format evolve. Binding `source`/`destination`/
`position` into the preimage ties each message to its channel and ordinal,
guarding against cross-channel forgery and reorder/replay. All tags, the version,
and `hash_leaf` live in `cumulus-primitives-spec-messaging`.

For empty blocks (no outbound messages, no inbound messages):
- `provides: None`
- `requires: empty CommitmentSet`

### 3.5 Late Block Proof Types

When a receiver block is built against an older source `provides_root` than
what's now current on the relay chain, the receiver collator includes a
`LateBlockProof` in the PoV. The collator prechecks the proof and uses the
transformed root in the candidate commitments. The PVF independently verifies
the proof and transforms the `RequiresCommitment` during `validate_block`, before
the relay chain sees it.

With the flat commitment, a `LateBlockProof` collapses to just the
per-destination MMR extension: both the old and the new subtree roots are
directly observable as `(receiver, root)` entries in the source's old/new
`ProvidesCommitment`, so no top-level inclusion proof is carried. The proof only
needs to show that the source's per-destination MMR for this receiver was
**append-only** extended from `old_subtree_root` to `new_subtree_root`.

```rust
/// Included in the receiver candidate's PoV when the block was built against
/// an older source subtree root than what's persisted in ProvidesRoots.
#[derive(Clone, Encode, Decode, Debug)]
pub struct LateBlockProof {
    /// The source parachain this proof covers.
    pub source: ParaId,

    /// The per-destination subtree root the receiver block was built against
    /// (from the old source block). Without the proof, this is what would
    /// appear in `RequiresCommitment.expected_root`.
    pub old_subtree_root: Hash,
    /// The source's current per-destination subtree root for this receiver —
    /// must match the source's persisted `ProvidesCommitment.get(receiver)`.
    pub new_subtree_root: Hash,

    /// Proof that the receiver's per-destination MMR was only appended to
    /// between the old and new state. This is an `mmr_lib` **ancestry proof**:
    /// verified by `proof.verify_ancestor(new_subtree_root, old_subtree_root)`
    /// (or `verify_incremental` with the appended leaf hashes). `None` only when
    /// `old_subtree_root == new_subtree_root` (no new messages for this receiver,
    /// just other-destination churn).
    pub subtree_extension: Option<AncestryProof<Hash, SpecMerge>>,
}
```

`AncestryProof` is `mmr_lib`'s append-only proof type (re-exported via
`sp-mmr-primitives`), parameterised with the crate's `SpecMerge` (§5.1). There is
no top-level proof leaf format anymore (the commitment is flat, §3.1); the only
proof here is the per-destination MMR ancestry proof, which `mmr_lib` generates
(`gen_ancestry_proof`) and verifies natively. This replaces the previously
hand-rolled `MMRExtensionProof` + `verify_mmr_extension`.

---

## 3.6 Primitives crate & integration tracker

The hardened primitives from issue
[#12346](https://github.com/paritytech/polkadot-sdk/issues/12346) are **done** in
the `cumulus-primitives-spec-messaging` crate
([PR #12368](https://github.com/paritytech/polkadot-sdk/pull/12368)):
`CommitmentSet` (canonical sorted decode), `OutgoingMessage::hash_leaf::<H>`
(domain-tagged, versioned, generic over the hasher), the domain tags, the
`MmrAccumulator` trait + peaks-only `Mmr<H>`, and `SpecMerge<H>`/`root_from_peaks`.
Inclusion and extension proofs come from **`mmr_lib`** (the `Mmr`
accumulator is kept consistent with it); PR #12368's single-shot `merge_peaks`
bagging is replaced by `mmr_lib`-consistent pairwise bagging. The crate types are
`no_std` and unit tested.

What remains is **integrating** the POC onto that crate and onto the three design
revisions at the top of this doc (blake2_256 + flat commitment + `mmr_lib`/
`SpecMerge`). The POC code still uses Keccak256, a two-level Merkle tree, and the
old `polkadot-primitives::v9::speculative` types. Integration checklist:

Legend: ☐ not started · ◐ partial · ☑ done

| # | Integration step | Status | File(s) to change |
|---|---|---|---|
| 1 | Layered type homes (no `polkadot → cumulus` edge): **relay-visible** types live in `polkadot-primitives::v9` — `CommitmentSet` (`v9/commitment_set.rs`, since it's embedded in `CandidateCommitments`), the `ProvidesCommitment`/`RequiresCommitment` aliases + bounding constants (`v9/speculative.rs`). The **parachain machinery** lives in `cumulus-primitives-spec-messaging`: the MMR/`hash_leaf` primitives + (in `message.rs`) the off-chain types `MessageBatch`, `LateBlockProof`, `SubtreeExtension`, `SpeculativeIngress`, `SourceState`, the `OutgoingMessage` alias, `MaxSpeculativeMessageLen`, `SpecHasher` — re-exported from `cumulus-primitives-core` (the parachain hub). The crate depends only on polkadot-core/parachain-primitives (clean cumulus→polkadot direction). | ☑ | `polkadot/primitives/src/v9/{commitment_set,speculative,mod}.rs`, `cumulus/primitives/spec-messaging/src/message.rs`, `cumulus/primitives/core/src/lib.rs` |
| 2 | `provides`/`requires` in `CandidateCommitments` become `Option<CommitmentSet<MAX_DESTINATIONS_PER_BLOCK>>` / `CommitmentSet<MAX_SOURCES_PER_BLOCK>` | ☑ | `polkadot/primitives/src/v9/mod.rs` |
| 3 | Outbox pallet: replace `Keccak256` leaf/merge with `OutgoingMessage::hash_leaf` + `SpecMerge`; keep peaks-only append but root via `spec_mmr::root_from_peaks`; `compute_provides` returns a `CommitmentSet` (no top-level merkle root). Added `Config::SelfParaId`; dropped top-level-root storage/APIs; `generate_late_block_proof(dest, old_subtree_root)` builds a `SubtreeExtension`. | ☑ | `cumulus/pallets/speculative-outbox/src/lib.rs` |
| 4 | Inbox pallet: drop `subtree_inclusion_proof` verification; hash leaves with `hash_leaf`; verify `messages_proof` via `mmr_lib::MerkleProof::<_, SpecMerge>`; `requires_commitments` returns a `CommitmentSet`. Also updated `client.rs` batch builder + the `SpeculativeOutboxApi`/`SpeculativeInboxApi` traits (`compute_provides`, dropped `subtree_inclusion_proof`, `generate_late_block_proof(dest, old_subtree_root)`, `block_hash_for_subtree_root`). 8/8 tests pass. | ☑ | `cumulus/pallets/speculative-inbox/src/{lib,client,mock,integration_tests}.rs`, `cumulus/primitives/core/src/lib.rs` |
| 5 | `validate_block`: replaced `Keccak256Merge`/`bag_mmr_peaks`/`append_mmr_leaf`/`verify_mmr_extension` with `SpecMerge` + `mmr_lib` `MerkleProof::verify_incremental`; dropped top-level inclusion verification + the `self_para_id` capture; rewrote the unit tests. Verified: cumulus-test-runtime WASM builds (compiles `implementation.rs`) and `validate_block_works` passes end-to-end. | ☑ | `cumulus/pallets/parachain-system/src/validate_block/implementation.rs` |
| 6 | Relay enactment: `ProvidesRoots` stores `ProvidesCommitment` (the set); `requires_satisfied` becomes a per-receiver `get()` lookup; `update_provides`. Also `provides_root` runtime API returns the set. | ☑ | `polkadot/runtime/parachains/src/inclusion/mod.rs`, `runtime_api_impl/vstaging.rs`, `primitives/src/runtime_api.rs`, rococo/westend |
| 7 | `ValidationResultExtension::V4` carries the flat `provides` set; candidate-validation rebuilds `CommitmentSet`s and reconstructs `CandidateCommitments` | ☑ | `polkadot/parachain/src/primitives.rs`, `polkadot/node/core/candidate-validation/src/lib.rs` |
| 8 | `MessageBatch`/`LateBlockProof` lost the top-level proof fields (§3.2/§3.5); off-chain batch builders, `OutboxQuery` trait + `RpcOutboxClient`, `RelayChainInterface::provides_root` (→ `ProvidesCommitment` set), lookahead/speculative_ingress collation patching, penpal + `cumulus/test/runtime` + `fake_runtime_api` API impls + `speculative_extension` all updated. | ☑ | `polkadot/primitives/src/v9/speculative.rs`, `cumulus/pallets/speculative-inbox/src/client.rs`, `cumulus/client/consensus/aura/src/collators/*`, `cumulus/client/relay-chain-*-interface`, penpal, omni-node `fake_runtime_api` |

> Load-bearing correctness points carried over from #12346, now guaranteed by the
> crate: canonical `CommitmentSet` encoding, domain-tagged/versioned leaf hashing,
> and a single shared MMR implementation (no collator-vs-PVF drift from duplicated
> merge logic).

> **Superseded by the 2026-06-18 window/UMP migration** (revisions 4–6 above):
> rows **6** (single `ProvidesRoots[source]` + `get()` lookup) and **7**
> (`ValidationResultExtension::V4` reconstruction into `CandidateCommitments`) are
> replaced by the UMP-signal transport (`ProvidesRoots`/`RequiresRoots` signals,
> fields removed) and the bounded provides **window** (`LatestProvides`, membership
> matching, `evict_provides_after`). LBP now transforms the `RequiresRoots` signal
> via the shared `apply_late_block_proofs`. Live status, per-task files, and
> remaining items (C2 fetch optimization, D2 e2e) are tracked in
> [speculative-messaging-window-migration-plan.md](speculative-messaging-window-migration-plan.md).
> `ValidationResultExtension` is retained only as the `speculative_extension()`
> hook's carrier (it emits the UMP signals during block execution).

---

## 4. Relay Chain Runtime Changes

### 4.1 Speculative Messaging Storage and Helpers

**POC implementation note:** The POC inlines all speculative relay-chain logic
(`ProvidesRoots` storage and helpers) directly into
`polkadot/runtime/parachains/src/inclusion/mod.rs`, grouped under clearly labelled
`// ── Phase 1 Speculative Messaging (POC) ──` comment sections. This avoids
the boilerplate of registering a new pallet for what are effectively a handful of
storage items and helper functions that are tightly coupled to `process_candidates`
and `enact_candidate`. A production implementation would extract these into a
separate `speculative_messaging.rs` module for independent testability.

```
// POC: inlined into polkadot/runtime/parachains/src/inclusion/mod.rs
// Production target: polkadot/runtime/parachains/src/speculative_messaging.rs
```

With the flat commitment the relay chain persists the source's whole
`ProvidesCommitment` (the sorted `(destination, subtree_root)` set), not a single
root, so a receiver's `expected_root` can be matched by looking up its own
destination entry.

```rust
/// Latest provides commitment per parachain.
/// Updated each time a v4 candidate with a provides commitment is included.
/// Only the most recent set is stored — old sets are overwritten.
#[pallet::storage]
pub type ProvidesRoots<T: Config> =
    StorageMap<_, Twox64Concat, ParaId, ProvidesCommitment>;

impl<T: Config> Pallet<T> {
    /// Read the latest provides commitment for a parachain.
    pub fn provides(para_id: &ParaId) -> Option<ProvidesCommitment> {
        ProvidesRoots::<T>::get(para_id)
    }

    /// The source's committed subtree root for a specific destination, if any.
    pub fn provided_subtree_root(source: &ParaId, dest: ParaId) -> Option<Hash> {
        ProvidesRoots::<T>::get(source)?.get(dest).copied()
    }

    /// Update the provides commitment after a candidate is included.
    pub fn update_provides(para_id: ParaId, provides: ProvidesCommitment) {
        ProvidesRoots::<T>::insert(para_id, provides);
    }
}
```

Register in `polkadot/runtime/parachains/src/lib.rs`.

### 4.2 Enactment-Time Matching

The relay-chain integration must distinguish **backing/pending-availability**
from **actual inclusion/enactment**. In the current architecture,
`inclusion::process_candidates()` handles newly backed candidates and moves them
into `PendingAvailability`, while `inclusion::enact_candidate()` is the
inclusion-time path that applies relay-visible messaging effects.

For speculative messaging:

- persisted `ProvidesRoots` must be updated only when a candidate is actually enacted/included
- requires/provides dependency satisfaction is checked only against the relay parent's state
  (i.e., roots persisted by prior relay blocks), not against candidates being enacted in
  the current block

This simplification avoids in-block candidate ordering tracking at the cost of at most
one relay block of additional latency in the rare case where both the providing and
consuming candidate land in the same relay block. The providing candidate is enacted in
relay block N, its `ProvidesRoots` entry persists, and the consuming candidate succeeds
when resubmitted in relay block N+1.

```rust
// Stage 1: backing / pending-availability admission
pub(crate) fn process_candidates<GV>(...) -> Result<..., Error> {
    for (para_id, backed_list) in candidates.iter() {
        for (candidate, core_index) in backed_list {
            // ... existing candidate checks ...
            // Store commitments (including provides/requires) unchanged in
            // CandidatePendingAvailability. No requires satisfaction check here.
            // PendingAvailability already stores full CandidateCommitments,
            // so no separate speculative storage map is needed.
        }
    }
}

// Stage 2: availability check — gate enactment on requires satisfaction
// (called from update_pending_availability_and_get_freed_cores)
if candidate.availability_votes.count_ones() >= threshold {
    let can_enact = /* predecessor check ... */
        && requires_satisfied(&candidate.commitments.requires);
    // If requires not satisfied, skip enactment for this candidate and
    // all its descendants in this relay block.
}

// Stage 3: inclusion / enactment
fn enact_candidate(receipt: CommittedCandidateReceipt, ...) {
    // Read provides/requires directly from the commitments stored in
    // CandidatePendingAvailability — no separate storage lookup needed.
    let receiver = receipt.descriptor.para_id();
    if !requires_satisfied(receiver, &commitments.requires) {
        defensive!("requirements no longer satisfied at enactment");
    }
    if let Some(ref p) = commitments.provides {
        update_provides(receiver, p.clone());
    }
}

// Matching is now a per-source lookup in the source's persisted provides set,
// keyed by the *receiver's* ParaId, instead of a single-hash comparison.
fn requires_satisfied(receiver: ParaId, requires: &RequiresCommitment) -> bool {
    requires.iter().all(|(source, expected_root)| {
        Pallet::<T>::provided_subtree_root(source, receiver) == Some(*expected_root)
    })
}
```

The relay chain is not asked to verify message proofs again. It only needs to
inspect the already-validated `provides` / `requires` fields, look up each
required source's committed subtree root for this receiver, and persist the
newest provides set. This is a relay-runtime inclusion rule change, not a new
protocol stage.

**Simplification versus the original design.** The original high-level proposal
included a same-block enacted matching path (checking against in-block candidate
ordering). The POC deliberately drops this for simplicity: the collator always
reads from the relay parent's state, which doesn't contain roots that will only
be written later in the same block. The same-block optimization can be added
later without breaking existing candidates — it only changes what the relay
chain accepts, not how the collator builds candidates.

**Relation to late block proofs.** When the source root has advanced beyond what
the receiver built against, Late Block Proofs (§6.2) transform the
`RequiresCommitment` to reference the current root before the relay chain sees
it. From the relay chain's perspective, the rule is always "the `expected_root`
must equal the source's committed subtree root for this receiver, i.e.
`ProvidesRoots[source].get(receiver)`" — the PVF handles the transformation.

Note that this problem is asymmetric: LateBlockProofs are only needed when the
source chain outpaces the destination (i.e., the source produces more blocks, or
the destination's candidate is delayed in the backing pipeline). If the
destination produces blocks faster than the source, the source root remains
stable across multiple destination blocks, and each can match against the
unchanged `ProvidesRoots[source]` without a proof. Faster destination production
is not a problem; slower destination *inclusion* is.

**What causes a mismatch.** The source's `ProvidesCommitment` set changes
whenever the source produces outbound messages to **any** destination. But
because matching is now a per-destination lookup (`get(receiver)`), churn from
*other* destinations no longer disturbs this receiver: this receiver's entry only
changes when the source sends new messages **to this receiver**. If the source
sends only to other parachains, `get(receiver)` is unchanged and the receiver's
candidate still matches — no LateBlockProof needed. A LateBlockProof is required
only when the source produced new messages *for this receiver* between the batch
being built and the receiver's candidate reaching enactment, advancing this
receiver's committed subtree root. (This is strictly better than the old
single-root scheme, where any-destination churn forced a proof.)

### 4.3 New Error

```rust
/// A requires commitment could not be matched to any provides.
UnsatisfiedRequires,
```

### 4.4 What the Relay Chain Does *Not* Do

A common misconception is that the relay chain must verify cryptographic proofs.
It does not. The division of labor is:

- **No MMR verification.** All MMR proof verification (subtree inclusion, message
  continuity, subtree extension) happens in the parachain runtime and is replayed
  deterministically by the PVF. The relay chain only compares 32-byte hashes.
- **No message storage.** Message payloads never touch relay chain state. They
  flow off-chain via the relayer/provider, are embedded in the receiver's block
  body as `SpeculativeIngress`, and are verified by the receiver runtime.
- **No history.** `ProvidesRoots` stores one `ProvidesCommitment` set per
  parachain (the current `(destination, subtree_root)` entries), overwritten each
  time a candidate with a new provides commitment is enacted. There is no
  per-block history, no MMR of roots, no retention of old values. The relay chain
  only needs the latest set for dependency matching.
- **No new protocol stage.** The relay chain still backs candidates, admits them
  to pending availability, and enacts them. Speculative messaging adds one
  inclusion-time check: for each `(source, expected_root)` in `requires`,
  `ProvidesRoots[source].get(receiver)` must equal `expected_root`. That check is
  a sorted-set lookup plus hash comparison, not a cryptographic verification.

In short: all cryptographic work lives in the PVF; the relay chain only adds
lookup-and-compare checks on already-validated commitment fields.

**Forward-looking: JAM 48KB refine→accumulate budget.** In the JAM protocol,
the data flowing from refine (PVF) to accumulate (enactment) is capped at ~48KB
per service per slot. Our design aligns naturally: the relay chain only handles
a single `ProvidesCommitment.root` (32 bytes) per producer and a list of
`RequiresCommitment` entries (~36 bytes each) per consumer. Full message
payloads never cross the refine→accumulate boundary — they flow off-chain
through the provider and are verified inside the PVF. This means the speculative
messaging model is JAM-compatible without rearchitecting, unlike HRMP where
payload bytes in relay chain state would compete for the 48KB budget.

---

## 5. Parachain Runtime Changes

### 5.1 Outgoing Message MMR (Sender Side)

Pattern: **wrap the runtime's configured `OutboundXcmpMessageSource`** (typically
`XcmpQueue`) by implementing the `XcmpMessageSource` trait such that each
outbound message is both recorded in the speculative outbox and forwarded to the
inner source. The wrapping type then replaces `XcmpQueue` as the `type
OutboundXcmpMessageSource` in the parachain runtime's `ParachainSystem` config.
The exact interception point in `cumulus/pallets/parachain-system/src/lib.rs`
(around line ~409) is where `T::OutboundXcmpMessageSource::take_outbound_messages`
is called to drain messages for inclusion in the relay-chain-candidate.

This sender-side flow must be produced by **normal runtime block execution** so
validators can replay the same state transition during `validate_block`. The
intended execution model:

1. Runtime execution emits outbound sibling-parachain XCM through the existing `SendXcm`/`XcmpQueue` path.
2. The speculative outbox wrapper intercepts those outbound payloads during that same runtime execution and appends them into per-destination MMR state.
3. After block execution finishes, the collator reads the resulting `provides_root` from runtime state via runtime API.

For a minimal POC, a new `pallet-speculative-outbox` should:

- hook into the runtime path that currently sends sibling-parachain XCM through `XcmpQueue`
- hash each outbound payload and append it to `OutgoingMMRs[destination]`
- preserve the normal XCMP delivery path so HRMP/XCMP output behavior remains intact
- expose `compute_provides_root()` as a runtime API for the collator after execution

```rust
/// Per-destination MMRs for outgoing messages.
#[pallet::storage]
pub type OutgoingMMRs<T: Config> = StorageMap<
    _, Twox64Concat, ParaId, MMRState,
>;

#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
    /// Number of leaves inserted so far into this destination's subtree MMR.
    pub leaf_count: u64,
    /// MMR peaks — O(log n) hashes sufficient to reconstruct the subtree root
    /// and to build append-only extension proofs.
    /// The full internal node set is NOT stored on-chain. Per-message inclusion
    /// proofs (proving a single leaf without the full batch) require the full
    /// node set; for Phase 1 these are generated off-chain by the provider
    /// process which rebuilds the MMR from `outbound_messages()` payload bytes.
    pub peaks: Vec<H256>,
}

/// Payload bytes for outgoing messages, keyed by destination and leaf position.
/// Stored on-chain for the POC to keep the relayer simple — no event indexing
/// or off-chain indexer needed. The relay chain is unaffected (this is
/// parachain-local storage). A production implementation may move payloads
/// off-chain with a pruning strategy; for the POC, bounded storage growth is
/// acceptable.
///
/// Pruning: entries can be removed after a configurable retention window (e.g.,
/// N blocks past the point where the destination has acknowledged consumption
/// via ProvidesRoots advancement). The POC may start without automated pruning
/// and add it when retention bounds are defined.
///
/// For long-lived testnets, a simple `on_initialize` hook that prunes entries
/// older than a fixed number of blocks (e.g., 10,000) can prevent unbounded
/// storage growth without committing to a full production pruning strategy.
#[pallet::storage]
pub type OutgoingMessages<T: Config> = StorageDoubleMap<
    _,
    Twox64Concat,
    ParaId,
    Twox64Concat,
    u64,
    Vec<u8>,
>;
```

The important distinction: `OutgoingMMRs[destination].leaf_count` is the
authoritative leaf count for that destination's subtree MMR,
`OutgoingMessage.position` refers to that per-destination counter, the
`ProvidesCommitment` set holds one `(destination, subtree_root)` entry per
destination, and a single sender-wide counter does **not** define the
proof/position space used by receivers.

**MMR implementation approach.** The hierarchical accumulator structure uses two
different constructions:

- **Per-destination subtrees** are MMRs that grow over time, built on **`mmr_lib`**
  (`polkadot-ckb-merkle-mountain-range`, already a workspace dependency,
  `no_std`) parameterised with the crate's `SpecMerge`. `SpecMerge` is an
  `mmr_lib::Merge<Item = Hash>` whose `merge` hashes inner nodes as
  `blake2_256(INNER_TAG ‖ l ‖ r)` and whose overridable `merge_peaks` hashes
  peak-bagging as `blake2_256(PEAK_TAG ‖ l ‖ r)`; leaves are
  `OutgoingMessage::hash_leaf` (LEAF_TAG). So `subtree_root` is `mmr_lib`'s bagged
  root. The outbox stores peaks-only state (`leaf_count` + `peaks`) in
  `OutgoingMMRs` and computes the root with the crate's
  `mmr::root_from_peaks(&peaks)` (equivalent to, but reachable unlike,
  `mmr_lib`'s internal peak bagging); inclusion and ancestry proofs are generated
  offchain by rebuilding the subtree from stored payloads into an `mmr_lib::MMR`. We reuse `mmr_lib`'s audited inclusion proofs
  (`gen_proof`/`verify`) and ancestry proofs (`gen_ancestry_proof`/
  `verify_ancestor`) rather than hand-rolling any proof code.
- **The top level** is just the current set of `(destination_para_id,
  subtree_root)` pairs, gathered into a `CommitmentSet` (sorted by
  `destination_para_id`). There is **no** top-level Merkle tree: the flat set is
  the commitment, and the relay chain looks up a receiver's entry directly (§4).

The per-destination MMR roots and message leaves use **`blake2_256`** with the
domain tags from `cumulus-primitives-spec-messaging` (`LEAF`/`INNER`/`PEAK`).
The domain tags supply the second-preimage separation that the old keyed-leaf
Merkle scheme relied on (a leaf hash can never be reinterpreted as an inner/peak
node); flattening the top level removes the only place the old design needed an
EVM-native top-level hash.

**Computing the provides commitment** — called by the collator after block
execution to populate `CandidateCommitments.provides`. Phase 1 uses **cumulative
latest-root semantics**: the set commits to the sender's full current speculative
outbox state after executing this block, not merely "the delta produced by this
block."

```rust
pub fn compute_provides() -> Option<ProvidesCommitment> {
    let entries = OutgoingMMRs::<T>::iter()
        .filter(|(_, state)| state.leaf_count > 0)
        .map(|(dest, state)| {
            let root = spec_mmr::root_from_peaks(&state.peaks)
                .expect("non-empty peaks bag to a root; qed");
            (dest, root)
        });

    // try_from_iter sorts by ParaId and rejects duplicates; an empty input
    // yields an empty set, which we treat as "nothing to provide".
    let set = CommitmentSet::try_from_iter(entries).ok()?;
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}
```

### 5.2 Incoming Message State (Receiver Side)

```rust
/// Per-source tracking (defined in `polkadot-primitives::v9::speculative`).
///
/// Only `last_processed` is required: subtree authentication flows through the
/// relay chain matching `batch.subtree_root` against the source's committed
/// `ProvidesRoots[source].get(receiver)` entry (the flat commitment, §3.1/§4),
/// and message authentication flows through the per-batch MMR inclusion proof
/// (`MessageBatch::messages_proof`) against `batch.subtree_root`. The receiver
/// runtime never reconstructs the sender's MMR — it only verifies proofs.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo, Default)]
pub struct SourceState {
    /// Last processed message leaf index in the source's subtree MMR.
    pub last_processed: u64,
}

#[pallet::storage]
pub type IncomingState<T: Config> = StorageMap<
    _, Twox64Concat, ParaId, SourceState,
>;

/// Per-block sources actually consumed during THIS block.
/// Cleared in `on_initialize`, populated by `ingest_verified_messages`,
/// then read by a runtime API after block execution to populate
/// `CandidateCommitments.requires`.
#[pallet::storage]
pub type ConsumedSourcesThisBlock<T: Config> = StorageValue<
    _,
    Vec<(ParaId, H256)>, // (source, expected per-receiver subtree root)
    ValueQuery,
>;
```

**Message batch verification** has two phases:

1. **Collator-local precheck** before block building — uses a collator-local
   cache of the receiver's latest finalized `IncomingState` snapshot and does not
   mutate runtime storage. An optimization for selecting batches, not
   consensus-critical.
2. **Runtime verification** inside `ingest_verified_messages` — replays the same
   checks against on-chain state and updates pallet storage deterministically.
   The consensus-critical path that validators replay.

**Collator-local precheck:**

```rust
struct LocalIncomingSnapshot {
    per_source: BTreeMap<ParaId, SourceState>,
}

pub fn precheck_message_batch(
    snapshot: &mut LocalIncomingSnapshot,
    batch: &MessageBatch,
) -> Result<(), VerificationError> {
    // 1. (Flat commitment) No subtree-inclusion proof: `batch.subtree_root` is
    //    matched directly by the relay chain against the source's committed
    //    `ProvidesRoots[source].get(LOCAL_PARA_ID)` entry. The collator only
    //    needs to confirm the messages hash up to `batch.subtree_root` (step 3)
    //    and that the relay has committed this root for the source (root guard,
    //    §7.4) — there is nothing to verify against a top-level root here.

    // 2. Verify message continuity against collator-local state.
    let mut local_state = snapshot.per_source
        .get(&batch.source)
        .cloned()
        .unwrap_or_default();
    let mut next_expected = if snapshot.per_source.contains_key(&batch.source) {
        local_state.last_processed.saturating_add(1)
    } else {
        0
    };
    let mut leaves = Vec::with_capacity(batch.messages.len());
    for msg in &batch.messages {
        ensure!(
            msg.position == next_expected,
            VerificationError::NonConsecutiveMessage,
        );
        leaves.push((mmr_lib::leaf_index_to_pos(msg.position), msg.hash_leaf()));
        next_expected = next_expected.saturating_add(1);
    }

    // 3. Verify the combined inclusion proof against subtree_root (mmr_lib + SpecMerge).
    if !leaves.is_empty() {
        let proof = mmr_lib::MerkleProof::<Hash, SpecMerge>::new(
            batch.subtree_mmr_size,
            batch.messages_proof.clone(),
        );
        proof.verify(batch.subtree_root, leaves)
            .ok().filter(|ok| *ok)
            .ok_or(VerificationError::InvalidMessagesProof)?;
    }

    // 4. Persist updated collator-local snapshot.
    if let Some(last) = batch.messages.last() {
        local_state.last_processed = last.position;
        snapshot.per_source.insert(batch.source, local_state);
    }

    Ok(())
}
```

**On-chain ingress execution** — the consensus-critical path:

```rust
fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
    ConsumedSourcesThisBlock::<T>::kill();
    Weight::zero()
}

#[pallet::call]
impl<T: Config> Pallet<T> {
    pub fn ingest_verified_messages(
        origin: OriginFor<T>,
        ingress: SpeculativeIngress,
    ) -> DispatchResult {
        ensure_none(origin)?;

        let mut consumed: Vec<(ParaId, H256)> = Vec::new();

        for batch in ingress.batches {
            // 1. (Flat commitment) No subtree-inclusion proof to verify here:
            //    `batch.subtree_root` is what the receiver records in `requires`
            //    and the relay chain matches it against the source's committed
            //    `ProvidesRoots[source].get(SelfParaId)` entry at enactment. The
            //    runtime authenticates messages against `batch.subtree_root`
            //    (steps 4–5); the root→source binding is the relay's job.

            // 2. Intra-block dedup: same source must not appear with two
            // different subtree_roots in the same block. Use the local
            // `consumed` list — no cross-block state needed.
            if let Some((_, prior_root)) =
                consumed.iter().find(|(source, _)| source == &batch.source)
            {
                ensure!(
                    *prior_root == batch.subtree_root,
                    Error::<T>::MultipleRootsPerSourceInOneBlock,
                );
            }

            // 3. Continuity check based on last_processed.
            let mut next_expected = match IncomingState::<T>::get(&batch.source) {
                Some(state) => state.last_processed.saturating_add(1),
                None => 0,
            };

            // 4. Collect MMR leaves for proof verification.
            let mut leaves: Vec<(u64, H256)> = Vec::with_capacity(batch.messages.len());
            for msg in &batch.messages {
                ensure!(msg.position == next_expected, Error::<T>::NonConsecutiveMessage);
                let leaf_hash = msg.hash_leaf();
                leaves.push((mmr_lib::leaf_index_to_pos(msg.position), leaf_hash));
                next_expected = next_expected.saturating_add(1);
            }

            // 5. Verify the combined inclusion proof against subtree_root (mmr_lib + SpecMerge).
            if !leaves.is_empty() {
                let proof = mmr_lib::MerkleProof::<H256, SpecMerge>::new(
                    batch.subtree_mmr_size,
                    batch.messages_proof.clone(),
                );
                let verified = proof.verify(batch.subtree_root, leaves).unwrap_or(false);
                ensure!(verified, Error::<T>::InvalidMessagesProof);
            }

            // 6. Update last_processed if any messages were consumed.
            if let Some(last) = batch.messages.last() {
                IncomingState::<T>::insert(
                    batch.source,
                    SourceState { last_processed: last.position },
                );
            }
            consumed.push((batch.source, batch.subtree_root));

            // 7. Dispatch each payload directly through the existing XCMP
            // handler. Each payload is already a full XCMP page from
            // XcmpQueue::take_outbound_messages (format byte + versioned XCM
            // bytes), so no additional re-encoding is needed.
            let max_weight = T::ReservedXcmpWeight::get();
            for msg in &batch.messages {
                T::XcmpMessageHandler::handle_xcmp_messages(
                    core::iter::once((
                        batch.source,
                        batch.source_relay_parent_number,
                        msg.payload.as_slice(),
                    )),
                    max_weight,
                );
            }
        }

        ConsumedSourcesThisBlock::<T>::mutate(|v| v.extend(consumed));
        Ok(())
    }
}
```

**Why no re-batching?** Each `OutgoingMessage::payload` is already a full XCMP
page produced by the sender's `XcmpQueue::take_outbound_messages` (format byte
prefix + versioned XCM bytes). Passing each payload directly to
`handle_xcmp_messages` preserves the page boundary the sender chose. Re-encoding
would either prepend a duplicate format byte (causing the receiver's handler to
reject the page) or fuse multiple sender pages into one — both subtly wrong.

### 5.3 Producing Commitments

After block execution, the collator reads the provides/requires from runtime
storage and populates `CandidateCommitments`. Phase 1 enforces at most one
`RequiresCommitment` per source parachain per block.

**Codebase integration.** The lookahead collator assembles speculative commitments
**after** `build_collation` returns, by querying the runtime of the newly built
block and patching the collation directly. This avoids threading these fields
through `build_multi_block_collation` or `ServiceInterface`:

```rust
// In cumulus/client/consensus/aura/src/collators/lookahead.rs,
// after collator_service.build_collation() returns Ok(Some((mut collation, block_data))):
let runtime_api = para_client.runtime_api();
let provides = runtime_api.compute_provides_root(new_block_hash).unwrap_or(None);
let requires = runtime_api.requires_commitments(new_block_hash).unwrap_or_default();
collation.provides = provides;
collation.requires = requires;
```

`build_multi_block_collation` and `ServiceInterface::build_multi_block_collation`
are unchanged from the non-speculative baseline — they set `provides: None` and
`requires: Vec::new()`. Speculative fields are only ever written by the lookahead
collator's post-build patch. No `CollationInfo` fields or pipeline changes needed.

```rust
pub fn requires_commitments() -> RequiresCommitment {
    // (source, expected per-receiver subtree root) pairs consumed this block.
    // try_from_iter sorts by source and rejects duplicate sources, producing
    // the canonical `CommitmentSet` encoding.
    let consumed = ConsumedSourcesThisBlock::<T>::get();
    CommitmentSet::try_from_iter(consumed).unwrap_or_default()
}

If late block proofs are needed (§6.2), the collator does **not** override
`requires` on the collation directly. Instead, the LBPs are attached to
`ParachainBlockData::V2.late_block_proofs` (in the PoV scaffolding, not the
runtime block body). The PVF reads them after block execution and transforms
the requires entries via `apply_messaging_proofs` before returning the
`ValidationResultExtension::V4`. See §6.2 and §10.7.

```rust
// cumulus/client/consensus/aura/src/collators/speculative_ingress.rs
//   fn fetch_ingress_for_block(...) -> (SpeculativeIngress, Vec<LateBlockProof>)
//
// cumulus/client/consensus/aura/src/collators/lookahead.rs
//   let (speculative_ingress, late_block_proofs) =
//       fetch_ingress_for_block(...).await;
//   collator.collate(..., late_block_proofs).await
//
// cumulus/client/collator/src/service.rs
//   build_multi_block_collation_with_late_block_proofs(...)
//   -> ParachainBlockData::new_v2(blocks, compact_proof, late_block_proofs)
```

---

## 6. PVF Validation Entry Point

Phase 1 requires a **small validation ABI extension**. Rather than introducing a
new `ValidationResultV4` struct, the POC extends the existing `ValidationResult`
with a `speculative: TrailingOption<ValidationResultExtension>` trailing field.
`TrailingOption` provides backward compatibility: old decoders that don't know
about the extension simply see zero remaining bytes and return `None`, while new
decoders decode the speculative fields. This avoids updating every code path that
constructs or matches on `ValidationResult`.

The extension enum:

```rust
/// Versioned extension appended to `ValidationResult` for speculative messaging.
/// Encoded as `TrailingOption<ValidationResultExtension>` — the trailing field
/// of `ValidationResult`. Old decoders see zero bytes and return None.
pub enum ValidationResultExtension {
    /// V4: speculative messaging provides/requires commitments (both flat sets).
    V4 {
        // Flat provides set: (destination, subtree_root) pairs (sender side).
        // None when the block sent nothing. Carries the whole set, not a single
        // root — the relay chain looks up entries by destination (§4).
        provides: Option<Vec<(ParaId, Hash)>>,
        // (source, expected per-receiver subtree root) pairs (receiver side).
        requires: Vec<(ParaId, Hash)>,
    },
}

pub struct ValidationResult {
    pub head_data: HeadData,
    pub new_validation_code: Option<ValidationCode>,
    pub upward_messages: UpwardMessages,
    pub horizontal_messages: HorizontalMessages,
    pub processed_downward_messages: u32,
    pub hrmp_watermark: RelayChainBlockNumber,
    /// Speculative messaging extension (v4+). Must be the LAST field.
    /// TrailingOption<T> greedily consumes all remaining bytes on decode.
    pub speculative: TrailingOption<ValidationResultExtension>,
}
```

Non-speculative candidates use `speculative: TrailingOption(None)`. Version-gating
happens on the node side — candidate validation branches on descriptor version to
decide whether to read speculative fields from the extension. The relay-chain
runtime API (`check_validation_outputs`) continues to accept the existing type,
ignoring the trailing extension for pre-speculative candidates.

**Current-codebase embedding:**

1. In `polkadot/parachain/src/primitives.rs`: `ValidationResultExtension` and `TrailingOption` defined; `speculative` field added as last field of `ValidationResult`. Note: the same file also defines `ValidationParamsExtension::V4` (the *input* side, carrying `relay_parent`/`scheduling_parent`) — this is a separate enum with the same variant index `4` but a different purpose. When reading the file, search for `ValidationResultExtension` specifically.
2. In `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`: after block execution, call `PSC::speculative_extension()` to read `ValidationResultExtension::V4` from runtime state; pass to `apply_messaging_proofs`; set `speculative` field in the returned `ValidationResult`.
3. In `polkadot/parachain/src/wasm_api.rs`: no separate entrypoint needed — `validate_block` returns `ValidationResult` with the speculative extension populated.
4. In `polkadot/node/core/candidate-validation`: decode `speculative` field; extract `provides`/`requires` from `ValidationResultExtension::V4` (rebuilding each as a `CommitmentSet`); reconstruct `CandidateCommitments` for v4 candidates.
5. Keep older descriptor versions on the legacy path (extension is `None`).

```rust
// In validate_block/implementation.rs — after executing all blocks:
let mut extension = PSC::speculative_extension();
if let Some(proofs) = messaging_proofs {
    apply_messaging_proofs(PSC::SelfParaId::get(), &mut extension, proofs);
}

ValidationResult {
    head_data: head_data.expect("HeadData not set"),
    new_validation_code: new_validation_code.map(Into::into),
    upward_messages,
    processed_downward_messages,
    horizontal_messages,
    hrmp_watermark,
    speculative: TrailingOption(extension),
}
```

The wasm PVF does **not** read candidate commitments as an input. It executes the
block, derives the full validation outputs via the speculative extension, and returns
them in `ValidationResult`. The node-side candidate-validation pipeline then
reconstructs `CandidateCommitments` from those outputs and checks the commitments
hash.

The full implementation (section 6.2) additionally reads `LateBlockProof` data from
the PoV after block execution, verifies each proof via `apply_messaging_proofs`, and
transforms the `requires` entries in the extension before the result is returned.

### 6.1 Candidate Commitments Reconstruction

After the PVF returns a `ValidationResult`, the node-side candidate validation
subsystem extracts speculative fields from `result.speculative.0` (a
`ValidationResultExtension::V4`), reconstructs `CandidateCommitments` from the
returned outputs, hashes them, and checks the hash against the candidate receipt's
`commitments_hash`. This is a **hash comparison only** — it ensures the PVF produced
the same commitments the collator claimed. If the PVF produced different `provides`
or `requires` (e.g., the collator lied, or a LateBlockProof verification failed
upstream inside the PVF), the hash won't match and the candidate is rejected.

LateBlockProof verification itself happens earlier, inside `validate_block`
(§6.2) — the PVF reads proofs from the PoV, verifies them via `apply_messaging_proofs`,
transforms requires in the extension, and returns the result. The hash check here is
the downstream safety net that catches any mismatch between the PVF's output and what
the collator put in the receipt.

Once validated, these commitments flow to the relay chain (§4.2) where
`requires` / `provides` matching happens. The relay chain trusts the commitments
because they've already been PVF-verified and hash-checked here.

Node-side candidate validation already reconstructs commitments for legacy fields
today. For the POC, update that logic to branch on candidate descriptor
version:

```rust
match candidate_receipt.descriptor.version() {
    V1 | V2 | V3 => {
        let commitments = v9::CandidateCommitments {
            head_data, upward_messages, horizontal_messages,
            new_validation_code, processed_downward_messages, hrmp_watermark,
        };
        ensure!(commitments.hash() == candidate_receipt.commitments_hash, ...);
    }
    V4 => {
        let commitments = CandidateCommitments {
            head_data, upward_messages, horizontal_messages,
            new_validation_code, processed_downward_messages, hrmp_watermark,
            provides, requires,
        };
        ensure!(commitments.hash() == candidate_receipt.commitments_hash, ...);
    }
}
```

The corresponding implementation work:

1. add `CandidateCommitments` and speculative types in `polkadot/primitives`
2. extend candidate receipt / descriptor version handling so v4 candidates use the new commitments layout
3. update `polkadot/node/core/candidate-validation` to reconstruct the correct commitments type per descriptor version
4. keep all pre-v4 candidates on the unchanged legacy reconstruction path

### 6.2 Late Block Proofs (PoV Approach)

When a receiver block was built against an older source root than what's now in
`ProvidesRoots`, the receiver collator includes a `LateBlockProof` in the PoV.
The proof verifies that the old root the block was built against is a valid
ancestor of the current root, so the relay chain can accept the dependency.

**Two-phase verification.** Late block proofs use the same two-phase model as
message batches (§5.2):

1. **Collator precheck.** Before building the candidate, the collator fetches the
   proof from the provider, verifies it locally (same logic as the PVF), and uses
   the transformed root (`proof.new_provides_root`) in the candidate
   commitments. This precheck is for efficiency — it prevents submitting a
   candidate with a bad proof.

2. **PVF verification.** During `validate_block`, the PVF independently reads the
   proof from the PoV, verifies it, and confirms the transformation. If the PVF
   produces a different transformed root than the collator put in the candidate
   commitments, the commitments hash won't match and the candidate is rejected —
   the same safety model as every other commitment field.

**When this triggers.** The collator detects the mismatch before block proposal:
it reads `ProvidesRoots[source]` from the relay parent's state and compares it to
the `provides_root` of the fetched batch. If they differ, the collator fetches a
`LateBlockProof` from the provider, prechecks it, and:

1. Uses `proof.new_provides_root` (not `batch.provides_root`) in the candidate
   commitments via the standard `requires_commitments()` path.
2. Wraps both block data and proofs in `ParachainBlockData::V2` and encodes
   the full struct as the PoV content.

**`ParachainBlockData::V2` wrapper type.** Instead of appending raw proof bytes after
the SCALE-encoded block data and parsing them with a manual cursor, the POC defines
a new versioned variant in `ParachainBlockData`:

```rust
pub enum ParachainBlockData<Block> {
    V0 { ... },
    V1 { ... },
    /// Speculative Messaging version.
    V2 {
        blocks: Vec<Block>,
        proof: CompactProof,
        late_block_proofs: Vec<LateBlockProof>,
    },
}
```

The wire format is a single SCALE-encoded `ParachainBlockData::V2` — no manual
length-prefixed sections, no cursor arithmetic. The collator constructs it via
`ParachainBlockData::new_v2`:

```rust
// cumulus/client/collator/src/service.rs
let block_data = ParachainBlockData::<Block>::new_v2(
    blocks,
    compact_proof,
    late_block_proofs,
);
let pov_bytes = block_data.encode();
```

**PVF decode.** The `validate_block` entry point decodes `ParachainBlockData::V2`
from the PoV bytes in one SCALE decode call. If the decode fails (wrong format,
truncated data, etc.), the candidate is invalid — same error model as any other
SCALE decode:

```rust
fn validate_block(params: ValidationParams) -> Result<ValidationResultV4, ValidationError> {
    // Single decode: block data + late block proofs in one call.
    let pov_v4 = ParachainBlockData::V2::decode(&mut &params.pov.block_data[..])
        .map_err(|_| ValidationError::InvalidBlockData)?;

    // 1. Execute the block with the inner block data
    let mut result = execute_block(&pov_v4)?;

    // 2. Verify each late block proof and transform requires
    let mut transformed_requires = Vec::new();
    for proof in &pov_v4.late_block_proofs() {
        let transformed = verify_and_transform(&result.requires, proof)?;
        transformed_requires.push(transformed);
    }
    // Keep non-transformed requires for sources without proofs
    for req in &result.requires {
        if !pov_v4.late_block_proofs().iter().any(|p| p.source == req.source) {
            transformed_requires.push(req.clone());
        }
    }

    result.requires = transformed_requires;
    Ok(result)
}
```

No SCALE cursor tricks, no manual offset tracking, no `parse_late_block_proofs`
function. The `ParachainBlockData::V2::decode` call either succeeds with both block
data and proofs, or fails cleanly.

**Version gating.** The collator and PVF must agree on the wire format. The
descriptor version (`candidate.descriptor.version()`) distinguishes the two cases:

- `V4` candidates: the PoV content is a SCALE-encoded `ParachainBlockData::V2`
- Pre-`V4` candidates: the PoV content is a plain `ParachainBlockData` (unchanged)

The collator branches on the candidate version when constructing the PoV:

```rust
if candidate_version >= V4 {
    let pov_v4 = ParachainBlockData::V2 { blocks, proof, late_block_proofs };
    pov.0 = pov_v4.encode();
} else {
    pov.0 = block_data.encode();
}
```

The PVF similarly branches:

```rust
fn validate_block(params: ValidationParams) -> Result<..., ValidationError> {
    // Version is communicated via the candidate descriptor (out of band of
    // the PoV bytes). In practice the PVF runtime knows its own version
    // from the runtime API version or descriptor header.
    if is_v4_candidate() {
        let pov = ParachainBlockData::V2::decode(&mut &params.pov.block_data[..])
            .map_err(|_| ValidationError::InvalidBlockData)?;
        let result = execute_block(&pov)?;
        // ... verify late_block_proofs from pov ...
    } else {
        let block_data = ParachainBlockData::decode(&mut &params.pov.block_data[..])
            .map_err(|_| ValidationError::InvalidBlockData)?;
        let result = execute_block(&block_data)?;
        // ... no proof verification ...
    }
}
```

**Why the wrapper is better than the cursor.** The cursor approach had several
problems:

1. **Fragile decoding.** The `u32::decode(&mut &cursor[..])` pattern creates a new
   borrowed slice each time instead of advancing the outer cursor. The correct
   pattern requires `let mut sub = &cursor[..]; u32::decode(&mut sub)`, which is
   easy to get wrong. A bug here silently produces garbage verification.
2. **No compile-time structure.** The wire format exists only as a comment diagram
   and the order of manual decode calls. `ParachainBlockData::V2` gives the compiler
   a single struct to check, and SCALE derive handles the encoding/decoding.
3. **Cleaner version gating.** With the wrapper, the version distinction is "use
   this type or that type" rather than "decode this struct, then manually parse
   trailing bytes of unknown format."

No PVF host changes are needed — the PoV is already passed to the PVF as opaque
bytes via `params.pov.block_data`. The only difference is what type we decode from
those bytes.

The relay chain never sees the proofs and never verifies them. The entire
pipeline is: collator wraps proofs in `ParachainBlockData::V2` → PVF decodes and
verifies proofs in one SCALE decode → transforms requires → node-side validation
reconstructs commitments from the transformed result → relay chain matches
`expected_root` against `ProvidesRoots`. See §4.4 for what the relay chain does
*not* do, and §6.1 for commitments reconstruction.

**Runtime performs no late-block-proof verification.** All four steps of
`verify_and_transform` (old subtree Merkle proof, new subtree Merkle proof, MMR
extension proof, root transformation) happen exclusively inside the PVF's
`apply_messaging_proofs`. The relay-chain runtime (`ingest_verified_messages`)
never sees `LateBlockProof` data and performs no verification of it. The PVF
is the consensus-critical path; the receiver runtime's `SourceState` carries
only `last_processed` and is not used for proof verification.

With the flat commitment there is **no top-level inclusion proof** to verify —
both subtree roots are directly observable as `(receiver, root)` entries in the
source's old/new `ProvidesCommitment`. The PVF only checks the append-only MMR
extension, then transforms the requires entry to reference the current subtree
root (which the relay chain will match by lookup).

```rust
fn verify_and_transform(
    block_requires: &RequiresCommitment, // CommitmentSet of (source, expected_root)
    proof: &LateBlockProof,
) -> Result<(ParaId, Hash), ValidationError> {
    // 1. Old root must equal what the block actually built against for this
    //    source (the entry already in `requires`).
    ensure!(
        block_requires.get(proof.source) == Some(&proof.old_subtree_root),
        ValidationError::LateProofOldRootMismatch,
    );

    // 2. Subtrees must be identical, or old must be an append-only prefix of new.
    if proof.old_subtree_root != proof.new_subtree_root {
        let ext = proof.subtree_extension
            .as_ref()
            .ok_or(ValidationError::SubtreeChangedWithoutProof)?;
        verify_mmr_extension(
            proof.old_subtree_root,
            proof.new_subtree_root,
            ext,
        )?;
    }

    // 3. Transformed entry references the current subtree root; the relay chain
    //    confirms it equals ProvidesRoots[source].get(receiver) at enactment.
    Ok((proof.source, proof.new_subtree_root))
}
```

**How the collator pre-transforms commitments.** The collator's precheck
produces the same transformed root. When building commitments (§5.3), the
collator uses the transformed subtree root directly — `ConsumedSourcesThisBlock`
still stores the original root from batch processing, but the collator overrides
it with the proof-verified root when constructing `CandidateCommitments`. The PVF
confirms this override independently.

**What the relay chain sees.** No change from section 4.2. The relay chain always
matches each `requires` entry against `ProvidesRoots[source].get(receiver)`. The
transformation happens before commitments are finalized, so the relay chain never
knows whether a proof was needed.

**Proof size.** No top-level proofs at all now. For a sender with m messages to
this receiver, the only cost is the subtree extension, O(log m) (~10 hashes for
1000 messages) — well under 1 KB in typical cases. The PoV size budget should
reserve a small allowance for these proofs (e.g., 50 KB).

**Serving extension proofs.** The provider serves `LateBlockProof` data via the
same HTTP endpoint (section 7.3), returning proofs alongside or instead of
batches when the source's committed subtree root for this receiver has advanced
beyond the receiver's last-seen root.

---

## 7. Off-Chain Networking

### 7.1 Model

**POC implementation: `OutboxQuery` trait abstraction.** The receiver collator
holds a list of type-erased sender-chain query handles via `SpeculativeMessageSources`:

```rust
pub struct SpeculativeMessageSources {
    pub sources: Vec<(ParaId, Arc<dyn OutboxQuery>)>,
    pub max_messages_per_source: u32,
}
```

`OutboxQuery` is an `async_trait` with a single concrete implementation in the
POC:

- **`RpcOutboxClient`** — connects to a remote sender node via JSON-RPC
  WebSocket (`jsonrpsee` ws-client). Each method translates to a `state_call`
  RPC request, e.g.
  `state_call("SpeculativeOutboxApi_compute_provides_root", at, args)`.
  Supports running sender and receiver as independent OS processes.
  Constructed via `RpcOutboxClient::connect(url).await`.

A future HTTP provider client (§12) would be a second `OutboxQuery`
implementation without touching collator logic.

This abstraction lives in:
- `cumulus/client/consensus/aura/src/collators/outbox_client.rs` —
  `OutboxQuery` trait, `RpcOutboxClient`, and the
  `build_message_batch_from_query` helper that produces a `MessageBatch`
  end-to-end (queries the sender for provides root, destination state,
  messages-with-proof, and subtree inclusion proof).
- `cumulus/client/consensus/aura/src/collators/speculative_ingress.rs` —
  `SpeculativeMessageSources` and `fetch_ingress_for_block()` (fully async):
  queries the relay chain for the current provides root, queries each
  configured sender via `OutboxQuery`, assembles `MessageBatch`es **and**
  any required `LateBlockProof`s (see §10.7).
- `cumulus/pallets/speculative-inbox/src/client.rs` — `build_message_batch()`
  lower-level batch construction helper for in-process scenarios where the
  caller has a direct `ProvideRuntimeApi<Block>` handle.

The transport is a data-fetch path, not a consensus path. Consensus depends only
on `SpeculativeIngress` being embedded in the block body and re-verified
deterministically during PVF execution. If no sender client is configured
(`SpeculativeMessageSources::disabled()`), the collator produces empty ingress
and falls back to HRMP.

### 7.2 Sender-Side: Batch Construction and Retention

The sender chain's runtime stores all speculative messaging data on-chain as a
result of normal block execution (§5.1). The sender runtime exposes APIs for
reading this data:

```rust
#[runtime_api]
pub trait SpeculativeOutboxApi {
    /// The sender's flat provides commitment: the sorted set of
    /// `(destination, subtree_root)` entries, or None if no destination has
    /// outbound messages yet. (Flat commitment — no top-level Merkle root.)
    fn compute_provides() -> Option<ProvidesCommitment>;
    /// (subtree_root, leaf_count) for a single destination.
    fn destination_state(dest: ParaId) -> Option<(Hash, u64)>;
    /// Read payload bytes from on-chain storage for a destination starting at
    /// `from_position`. Returns up to `max_messages` entries. Use this when
    /// the caller does not need an MMR proof.
    fn outbound_messages(
        dest: ParaId,
        from_position: u64,
        max_messages: u32,
    ) -> Vec<(u64, Vec<u8>)>;
    /// Read a slice of outbound messages with a combined MMR inclusion proof
    /// against the per-destination subtree root. Rebuilds the subtree from
    /// stored payloads into an `mmr_lib::MMR<_, SpecMerge, _>` (the peaks-only
    /// on-chain state cannot generate proofs) and calls `gen_proof`. Returns
    /// `(messages, subtree_mmr_size, messages_proof)`. Used by collators to build
    /// a self-contained `MessageBatch`.
    fn outbound_messages_with_proof(
        dest: ParaId,
        from_position: u64,
        max_messages: u32,
    ) -> Option<(Vec<(u64, Vec<u8>)>, u64, Vec<Hash>)>;
    // (Flat commitment) `subtree_inclusion_proof` is gone — the destination's
    // subtree root is directly observable in `compute_provides()`, so no
    // top-level inclusion proof is ever generated or verified.
    /// Generate a late block proof connecting the receiver's old subtree root to
    /// the sender's current subtree root for that receiver. See §6.2 and §10.7.
    fn generate_late_block_proof(
        dest: ParaId,
        old_subtree_root: Hash,
    ) -> Option<LateBlockProof>;
    /// Reverse lookup: which sender block produced this destination's subtree root.
    fn block_hash_for_subtree_root(dest: ParaId, subtree_root: Hash) -> Option<Hash>;
}
```

**Payload bytes are read from on-chain storage.** The outbox pallet stores full
payload bytes in `OutgoingMessages` (see §5.1). These runtime APIs can be
queried by any RPC client that knows the source chain's endpoint.

**Self-contained batches.** `outbound_messages_with_proof` returns the
messages, the MMR size at proof generation time, and a combined inclusion
proof — exactly the three fields needed to populate `MessageBatch`. The
receiver verifies all messages in one call to
`MerkleProof::<Hash, SpecMerge>::new(subtree_mmr_size, proof).verify(subtree_root,
leaves)`. No re-derivation of the sender's MMR happens on the receiver side.

**Optional caching layer (provider).** A future provider process could monitor
the sender chain, query these runtime APIs, and cache pre-assembled
`MessageBatch` structs keyed by `(destination_para_id, provides_root)`. For
the POC, the receiver collator queries the sender chain directly via
`RpcOutboxClient`. Adding a provider does not change any of the consensus
logic — it would just be a third `OutboxQuery` implementation.

### 7.3 Transport: HTTP API

> **Pre-revision (as-built).** The cursor/JSON below still reference the
> top-level `provides_root` and `subtree_inclusion_proof`. Under the flat
> commitment the cursor becomes the receiver's last-seen **subtree root** and the
> `subtree_inclusion_proof`/`provides_root` fields drop out (§3.2). Shown as-is
> for the current POC.

For the POC, a simple HTTP endpoint:

```
GET /batches/{destination_para_id}?since_provides_root={hash}
```

- `destination_para_id` (path): the parachain requesting batches.
- `since_provides_root` (query, optional): the last provides root the receiver
  has accepted. If omitted or unrecognized, returns batches from the oldest
  retained root (cold-start). If no new batches exist, returns an empty list.

Response (JSON):

```json
{
  "source": 1000,
  "batches": [
    {
      "source_block": "0x...",
      "source_relay_parent_number": 12345,
      "provides_root": "0x...",
      "subtree_root": "0x...",
      "subtree_inclusion_proof": ["0x...", "0x..."],
      "messages": [
        { "position": 42, "payload": "0x..." },
        { "position": 43, "payload": "0x..." }
      ]
    }
  ]
}
```

The provider is a separate process that connects to the source chain's node,
subscribes to finalized blocks, extracts outbox state via the runtime API, and
serves the HTTP endpoint.

### 7.4 Receiver-Side: Fetch, Precheck, Inject

**Fetch.** Before building a block, the collator's inherent-data provider
iterates over configured source parachains, reads
`SpeculativeInboxApi::next_expected_message_position(source)` from the
receiver's own runtime, and queries each sender outbox for messages from
that position onward. Timeouts (e.g., 2 seconds per source) prevent hanging.

**Precheck.** Each fetched batch goes through the collator-local precheck
described in section 5.2: verify subtree inclusion proof, verify message
continuity, verify the combined MMR inclusion proof against `subtree_root`.

The collator then compares the batch's `provides_root` against the current
`ProvidesRoots[source]` by calling the relay chain runtime API
`ParachainHost::provides_root(source, relay_parent)` via the
`RelayChainInterface` handle. This call is a **best-effort precheck**, not a
consensus path:

- If `batch.provides_root == ProvidesRoots[source]`: the dependency is already
  satisfied — no proof needed, candidate will pass enactment.
- If they differ: the collator fetches and prechecks a `LateBlockProof`
  (section 6.2), verifying it locally and recording the transformed root for
  use in commitment assembly. The candidate will match against the current root
  at enactment.
- If the RPC read is stale or wrong: the candidate is rejected at enactment
  with `UnsatisfiedRequires` — no state corruption, just a retry.

Batches and proofs that fail precheck are discarded.

**Selection.** Batches are ordered by source priority (configurable) then by age
(oldest first). The collator selects greedily until block weight or size limits
are met. At most one distinct `provides_root` per source per block.

**Injection.** Selected batches are encoded into `SpeculativeIngress` and
injected into `InherentData` under `INHERENT_IDENTIFIER`. Prechecked
`LateBlockProof` data and the block data are wrapped in `ParachainBlockData::V2`
for the PoV.

**Resubmission.** After submitting the candidate, the collator watches the relay
chain for a configurable window (e.g., 6 relay blocks). If the candidate is not
enacted within the window — either because a dependency was unsatisfied
(`UnsatisfiedRequires`), a LateBlockProof was stale, or the candidate was
dropped from the pipeline — the collator fetches fresh data from the provider
(updated batches and/or proofs), rebuilds the block, and resubmits. This minimal
retry loop converts transient failures into eventual success:

```
loop {
    fetch fresh batches + proofs from provider
    precheck → select → inject → build candidate → submit
    wait for enactment (configurable N relay blocks)
    if enacted { break; }
}
```

The production-grade retry policy (exponential backoff, persistent message
queues, bounded catch-up) is deferred to §12. The POC only needs enough
resilience to survive the normal backing-pipeline variability on a testnet.

### 7.5 Provider Discovery

For the POC, static configuration:

```toml
[speculative_messaging_providers]
1000 = ["http://provider-a.example:9100"]
2000 = ["http://provider-b.example:9100"]
```

The collator tries providers in order until one responds. Native collator
discovery / request-response is deferred past the POC.

### 7.6 Error Handling and Retry

```
For each source chain:
  1. Try to connect to any known provider
  2. Request MessageBatch data with since_provides_root cursor
  3. If response received → precheck each batch → encode accepted batches
  4. If timeout or error → log warning → SKIP this source for this block
```

Skipped sources are retried in the next block. No block production is ever
blocked by networking failures. The block can still be produced without
speculative ingress — consensus remains correct.

### 7.7 Boundedness and Failure Modes

**Catch-up window.** The provider retains a sliding window. A destination that
falls behind by more than the retention window cannot fetch the missing batches
(the provider has pruned them). The receiver's precheck rejects batches where
`source_relay_parent_number` is too far behind the current relay parent. Within
the retention window, Late Block Proofs (§6.2) handle the case where the source
root has advanced.

**Provider failure.** If all providers for a source are unreachable, speculative
messages from that source are skipped. The collator continues with HRMP messages
if configured. No block production is blocked.

**Stale batches from forked source blocks.** If a provider serves a batch where
the corresponding sender candidate was never included (forked), the receiver
block's `RequiresCommitment` will reference a `provides_root` that never appears
in `ProvidesRoots`. At enactment time, the relay chain rejects with
`UnsatisfiedRequires`. The candidate is not included; no state corruption. The
receiver collator can reduce the chance by only fetching batches for finalized
source blocks, but finalized does not mean included.

**Malicious provider.** The transport is untrusted. The receiver re-verifies all
proofs in the runtime. A malicious provider can serve invalid proofs (runtime
rejects), stale batches (continuity check rejects), or withhold batches
(receiver skips). No new trust assumptions are introduced.

### 7.8 Tradeoffs

The relayer/provider-first approach is a practical POC simplification:

- **Advantages**: simpler transport implementation, easier debugging, clear separation between consensus and transport logic, natural place for bounded recent history.
- **Tradeoffs**: a single provider can become a latency bottleneck, adds an extra operational component, more centralized than the peer-native end-state.

This is **not** a consensus-safety bottleneck — an unavailable provider means the
collator skips speculative ingress for that block.

### 7.9 Native Collator Transport (Future)

Direct collator request/response is a later native fast path:

```rust
pub const SPECULATIVE_MSG_PROTOCOL: &str = "/polkadot/speculative-messaging/1";

#[derive(Encode, Decode, Debug)]
pub struct MessageBatchRequest {
    pub source: ParaId,
    pub destination: ParaId,
    pub from_block: Hash,
    pub to_block: Option<Hash>,
}

#[derive(Encode, Decode, Debug)]
pub struct MessageBatchResponse {
    pub batches: Vec<MessageBatch>,
}
```

For a later native implementation, `cumulus/client/bootnodes` is a good example
of a small request/response protocol. The relayer/provider path can remain as the
fallback/catch-up layer even after native collator transport is added.

---

## 8. HRMP Coexistence

Phase 1 runs **alongside HRMP**. Both paths produce/consume messages. The receiver
deduplicates: if the same message arrives via both HRMP and speculative
messaging, the second dispatch attempt is ignored (replay protection by
`(source, position)` or message hash).

**Collator block building order:**
1. Fetch pending messages via HRMP (from relay parent, as before)
2. Fetch pending messages via speculative messaging (off-chain)
3. Locally precheck speculative batches and encode them into `SpeculativeIngress`
4. Both sets of messages are executed in the same block
5. Both HRMP watermark and provides/requires are emitted in `CandidateCommitments`

The `horizontal_messages` field in `CandidateCommitments` continues to carry HRMP
messages. Speculative messaging messages are NOT carried in `horizontal_messages`
— they are carried in the block body's `SpeculativeIngress` call.

**Weight accounting.** Both HRMP (called from
`ParachainSystem::set_validation_data`) and speculative ingress call
`handle_xcmp_messages`, each consuming from the same
`ReservedXcmpWeight`/`ReservedXcmpWeightOverride` budget. The simplest POC
approach: set the total reserved XCMP weight high enough to cover both paths in
the worst case, and let each call consume what it needs.

**Benchmarking requirement.** Because both paths share the same budget, concrete
benchmarking is required to ensure speculative ingress does not starve standard
HRMP/XCMP traffic. For the POC, we recommend monitoring the `ReservedXcmpWeight`
utilization and potentially introducing a separate configurable weight limit for
speculative ingress if starvation is observed.

---

## 9. Feature Gating & Upgrade Path

### 9.1 Per-Parachain Enablement

A parachain signals speculative messaging support by upgrading to a v4
`CandidateDescriptor`. The relay chain only enforces requires/provides for v4
candidates; v3 (and v2) candidates skip the new validation entirely.

The upgrade order:
1. Parachain runtime upgrades to maintain speculative inbox/outbox state and expose runtime APIs
2. Collator nodes upgrade to support v4 descriptors and the new protocol
3. Relay chain runtime upgrades to recognize v4 descriptors and perform commitment matching
4. Once all three are deployed, messages begin flowing through the new path

### 9.2 Per-Channel Gating (Optional)

For finer control, a parachain runtime config can list which source chains to use
speculative messaging with:

```rust
parameter_types! {
    pub SpeculativeMessagingSources: Vec<ParaId> = vec![
        ParaId(1000),
        // ParaId(2000),  // still use HRMP for para 2000
    ];
}
```

Sources not in this list continue to receive messages via HRMP only.

### 9.3 Storage Migrations & Runtime Integration

Adding `pallet-speculative-outbox` and `pallet-speculative-inbox` to an existing
runtime (e.g., Penpal) requires updating `construct_runtime!`. While new pallets
typically start with empty storage and do not strictly require a migration for
*existing* data, the integration process should consider:

- **Storage Layout:** The addition of new pallets will change the storage
  prefixes. Ensure no conflicts exist with existing pallets.
- **Initial State:** If the speculative pallets depend on any existing state from
  `pallet-xcmp-queue` or `parachain-system`, an `OnRuntimeUpgrade` hook should be
  implemented to initialize the speculative state (e.g., setting the initial
  `last_processed` indices based on the current HRMP watermark).
- **Execution Order:** Maintain the `construct_runtime!` order specified in §3.3
  to ensure correct `on_initialize`/`on_finalize` sequencing.

---

## 10. Implementation Plan

Implement in the following order.

### 10.1 Step 1: Primitives and Version Gating

**Files:**
- `polkadot/primitives/src/v9/speculative.rs`
- `polkadot/primitives/src/lib.rs`
- `polkadot/primitives/test-helpers/src/lib.rs`

Add `ProvidesCommitment`, `RequiresCommitment`, `MessageBatch`, `OutgoingMessage`,
`SpeculativeIngress`, and `CandidateCommitments`. Extend descriptor-version
handling for v4 speculative candidates. Update test helpers.

### 10.2 Step 2: Receiver Runtime Ingress Path

**Files:**
- new `cumulus/pallets/speculative-inbox/`
- `cumulus/pallets/parachain-system/src/lib.rs`
- chosen POC runtime (e.g., `cumulus/parachains/runtimes/testing/penpal/src/lib.rs`)

Add `IncomingState`, `ConsumedSourcesThisBlock`, `ingest_verified_messages`,
`ProvideInherent`. Re-verify subtree proofs, message continuity, subtree-root
reconstruction, and the one-root-per-source-per-block invariant. Dispatch through
`T::XcmpMessageHandler::handle_xcmp_messages(...)`. Expose
`requires_commitments()` runtime API.

### 10.3 Step 3: Sender Runtime Outbox Path

**Files:**
- new `cumulus/pallets/speculative-outbox/`
- `cumulus/pallets/parachain-system/src/lib.rs`
- chosen POC runtime

Wrap the existing outbound XCMP path. Maintain per-destination `OutgoingMMRs`.
Implement canonical top-level root construction. Expose `compute_provides_root()`
runtime API.

### 10.4 Step 4: Collator-Side Inherent Injection and Commitment Assembly

**Files:**
- `cumulus/client/consensus/aura/src/collator.rs`
- `substrate/primitives/inherents/src/client_side.rs`

Add node-local speculative fetch/precheck component. Extend inherent-data
creation to inject `SpeculativeIngress`. After block execution, read
runtime-produced `provides` and `requires` and construct v4 commitments.

### 10.5 Step 5: PVF / Wasm Validation ABI

**Files:**
- `polkadot/parachain/src/primitives.rs`
- `polkadot/parachain/src/wasm_api.rs`
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`

Extend the wasm validation result shape for v4 speculative candidates. In
`validate_block`, assemble speculative outputs from post-execution runtime state.
Ensure wasm result serialization returns the extended shape.

### 10.6 Step 6: Node-Side Candidate Validation

**Files:**
- `polkadot/node/core/candidate-validation/src/lib.rs`

Decode the extended validation result for v4 candidates. Reconstruct
`CandidateCommitments` from returned outputs. Keep pre-v4 candidates on the
legacy path. Continue hash-checking against the candidate receipt.

### 10.7 Step 7: Late Block Proofs (PVF + Collator)

**Files:**
- `polkadot/primitives/src/v9/speculative.rs` (`LateBlockProof`, `MMRExtensionProof` types)
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`
  (`apply_messaging_proofs`, `verify_mmr_extension`)
- `cumulus/pallets/speculative-outbox/src/lib.rs` (`generate_late_block_proof`
  runtime helper)
- `cumulus/primitives/core/src/lib.rs` (`SpeculativeOutboxApi::generate_late_block_proof`)
- `cumulus/client/consensus/aura/src/collators/outbox_client.rs`
  (`OutboxQuery::generate_late_block_proof` + RPC impl)
- `cumulus/client/consensus/aura/src/collators/speculative_ingress.rs`
  (`fetch_ingress_for_block` returns `(SpeculativeIngress, Vec<LateBlockProof>)`)
- `cumulus/client/consensus/aura/src/collator.rs`
  (`Collator::collate` takes `late_block_proofs`)
- `cumulus/client/collator/src/service.rs`
  (`build_multi_block_collation_with_late_block_proofs` →
  `ParachainBlockData::new_v2(blocks, proof, late_block_proofs)`)

PoV-based proof verification: when the relay's committed
`ProvidesRoots[source]` differs from a batch's `provides_root`, the receiver
collator fetches a `LateBlockProof` from the sender at the block whose
`compute_provides_root()` equals the relay's current root. The proof rides
in `ParachainBlockData::V2.late_block_proofs` (PoV scaffolding, not block
body). The PVF decodes it during `validate_block`, verifies the two binary
Merkle proofs and the MMR extension, and transforms
`requires[source].expected_root` from old → new before returning the
`ValidationResultExtension::V4`. The relay's `requires_satisfied` check
then matches against the current root. Mismatched proofs cause commitments
hash mismatch and the candidate is rejected.

### 10.8 Step 8: Relay-Chain Runtime Enactment Rules

**Files:**
- new `polkadot/runtime/parachains/src/speculative_messaging.rs`
- `polkadot/runtime/parachains/src/inclusion/mod.rs`

Add `ProvidesRoots` storage. Keep `process_candidates()` for backing admission.
Extend the enactment path to check v4 `RequiresCommitment` against persisted
roots only. Add `UnsatisfiedRequires` error.

### 10.9 Step 9: Off-Chain Networking

**Files:** new node-side protocol module under `cumulus/client/...`

Add a provider/relayer process serving bounded recent history of both
`MessageBatch` data and `LateBlockProof` data. Add destination-side fetcher with
static `ParaId -> Vec<ProviderEndpoint>` configuration. Optionally add native
collator request/response later.

### 10.10 Step 10: POC Runtime and Tests

Target one contained parachain runtime (Penpal, Rococo parachain, or similar).

**Test milestones:**
1. sender runtime emits a stable cumulative `provides` root
2. receiver runtime accepts valid `SpeculativeIngress` and rejects invalid proofs/ordering/mixed-root cases
3. PVF returns matching v4 validation outputs (including transformed requires from late block proofs)
4. node-side candidate validation reconstructs the correct v4 commitments hash
5. relay-chain enactment accepts satisfied dependencies (batch root matches persisted ProvidesRoots, including late-block-proof cases) and rejects unsatisfied ones
6. collator networking can fetch, precheck, and inject a recent batch end-to-end
7. late block proof: receiver can consume messages from a source that has advanced past the root the receiver built against
8. resubmission: collator detects candidate rejection, fetches fresh data, rebuilds, and delivers the message on a subsequent attempt

---

## 10.11 POC Status Tracker

> **Pre-revision (as-built).** "✅ done" here means done in the **current POC**,
> which still uses Keccak256 and a two-level Merkle commitment. Migration onto
> `blake2_256` + the flat commitment + the `cumulus-primitives-spec-messaging`
> crate is tracked separately in the §3.6 integration checklist.

Legend: ✅ done · 🔶 partial · ❌ not started

| Step | Description | Status | Notes |
|------|-------------|--------|-------|
| 10.1 | Primitives & version gating | ✅ | `v9::speculative` types, `MMRExtensionProof.old_leaf_count`, `CandidateDescriptorV2::new_v4()` version-byte approach. V4 struct family removed. |
| 10.2 | Receiver runtime ingress path | ✅ | `pallet-speculative-inbox` complete: `IncomingState`, `ConsumedSourcesThisBlock`, `ingest_verified_messages`, `requires_commitments`. 11/11 tests pass. |
| 10.3 | Sender runtime outbox path | ✅ | `pallet-speculative-outbox` complete: peaks-only MMR, `HistoricalProvidesRoots`, `HistoricalSubtreeState`, `generate_late_block_proof`. |
| 10.4 | Collator-side inherent injection & commitment assembly | ✅ | Lookahead collator path complete: `speculative_ingress` fetched in `lookahead.rs` before block proposal, injected via `create_inherent_data`; after `build_collation` returns, `compute_provides_root` and `requires_commitments` are queried against the newly built block hash and patched directly onto the collation (`collation.provides`, `collation.requires`). **Root guard** (`speculative_ingress.rs`): batch is only included if the relay has committed *some* `provides_root` for the source; the receiver collator either picks the matching sender block or fetches a `LateBlockProof` to bridge from the batch's root to the relay's current root. **Fork suppression** (`lookahead.rs`): `speculative_built_heights: BTreeSet<BlockNumber>` tracks heights where a speculative collation was submitted; n_requires=0 collations at those heights are suppressed on subsequent relay-parent notifications. Slot-based collator has no speculative logic (not used for the POC). |
| 10.5 | PVF / Wasm validation ABI | ✅ | `ValidationResultExtension::V4` via `TrailingOption<ValidationResultExtension>` (not a separate `ValidationResultV4` struct — backward-compatible trailing field on existing `ValidationResult`). `apply_messaging_proofs`, `verify_mmr_extension` (connecting_nodes replay). `ParachainBlockData::V2` decoded in `validate_block`. |
| 10.6 | Node-side candidate validation | ✅ | v4 `CandidateCommitments` reconstruction from `ValidationResultExtension::V4` and hash check implemented in `candidate-validation/src/lib.rs` lines 1310–1337. |
| 10.7 | Late block proofs (PVF + collator) | ✅ | PVF-side proof verification complete (`apply_messaging_proofs`, `verify_mmr_extension`). Collator-side wiring complete: `OutboxQuery::generate_late_block_proof` (RPC impl), `fetch_ingress_for_block` returns `(SpeculativeIngress, Vec<LateBlockProof>)`, `Collator::collate` takes the LBP vec, `CollatorService::build_multi_block_collation_with_late_block_proofs` wraps into `ParachainBlockData::V2.late_block_proofs`. The receiver attaches an LBP whenever the relay's committed `ProvidesRoots[source]` differs from the batch's `provides_root`; the PVF then transforms `requires[source].expected_root` from the batch root to the relay's current root before returning the validation result. |
| 10.8 | Relay-chain enactment rules | ✅ | `ProvidesRoots` storage, `requires_satisfied`, `update_provides_root`, enactment-time check (lines 582–588, 956–964) and `UnsatisfiedRequires` error all wired in `inclusion/mod.rs`. |
| 10.9 | Off-chain networking | ✅ | `OutboxQuery` async trait with `RpcOutboxClient` (JSON-RPC WebSocket). `SpeculativeMessageSources` is generic-free. `fetch_ingress_for_block` is fully async. `--speculative-sender <PARA_ID>=<WS_URL>` CLI arg added to omni-node. At startup the lookahead collator async-connects to each configured sender and populates `SpeculativeMessageSources`; connection failures are logged and skipped gracefully. Slot-based collator has no speculative networking (not used for the POC). |
| 10.10 | POC runtime & test milestones | ✅ | All milestones complete. E2e tooling lives at `speculative_messaging_e2e/` in the repo root: `network-rococo.toml` (two Penpal instances — para 2000 sender, para 2001 receiver — on Rococo-local), `start-testnet.sh`, `send-xcm.js`, `observe.js`. Receiver started with `--speculative-sender 2000=ws://127.0.0.1:9955`. `send-xcm.js` uses `subscribeAllHeads` (not `subscribeNewHeads`) so speculative delivery in non-best fork blocks is detected. Live runs confirm end-to-end delivery with latency ~18–30 s (sender relay-inclusion + receiver slot alignment). |

---

## 11. What's NOT In This POC

- **Speculative (acknowledged) delivery mode**: requires Low-Latency v2's collator acknowledgement signatures, which are not yet implemented in the codebase. The receiver cannot optimistically build on an un-included sender block without a signed canonicality commitment from the sender's collators.
- **Super-chain (intra-block) delivery mode**: unlike speculative mode, super-chain does NOT require LLv2 (no cross-collator trust — one collator authors everything). It IS blocked by collator infrastructure that doesn't exist: collators today are tied to a single parachain; producing blocks for multiple parachains in one slot needs multi-parachain collator assignment, intra-block message dependency ordering (A's block before B's within the same slot), and atomic inclusion semantics on the relay chain. All three are design-only, not implemented.
- **Trust domains**: a concept from the high-level design (§8) where parachains declare which peers' collators they trust for speculative (acknowledged) delivery. Trust domains require three things that don't exist yet: LLv2 collator acknowledgement signatures, a `TrustedPeers: Vec<ParaId>` runtime configuration, and collator logic for trust-domain-aware acknowledgement rules. The POC uses inclusion-based delivery only, which relies purely on relay chain enforcement of `ProvidesRoots` — no trust assumptions between chains.
- **Low-Latency v2 integration**: LLv2 is the most invasive dependency in the speculative messaging design space — its core components touch consensus-critical code across the codebase. It requires: new `scheduling_parent` and `scheduling_session_index` fields in the candidate descriptor (decoupling scheduling from relay parent); backing group selection based on scheduling parent (relay chain runtime, security-critical); inclusion rules for candidates with relay parents up to ~14,400 blocks old; collator acknowledgement signatures (new primitives + gossip protocol); slashing rules for ACK'd-but-never-included blocks; and PVF header-chain proofs. The POC establishes the integration model (descriptor version gating, PVF validation ABI extension, relay-chain enactment-time rules) that LLv2 builds on, but does not reduce LLv2's own scope — it is a separate large project.
- **Relaxed or unordered delivery semantics**: Phase 1 requires contiguous per-source subtree advancement
- **Message pruning or MMR garbage collection**: leaves grow indefinitely
- **Economic incentives**: no fee mechanism for relayers/collators
- **Cycle prevention**: handled by "don't process messages from blocks that haven't been built yet"

---

## 12. Follow-Up Roadmap

### Delivery Bounds and Pruning
- Define what "eventual delivery" means operationally.
- Bound maximum message age and maximum catch-up per block.
- Define message retention windows and pruning triggers.

### Rate Limiting and DoS Protection
- Add per-channel message and byte limits.
- Enforce limits on outbox and inbox paths.

### Proof and Storage Bounds
- Define fallback behavior when late-block-proofs exceed PoV size limits.
- Confirm relay-chain storage remains bounded to latest-per-para data only.

### Trust Domains and Acknowledgements
- Define when speculative mode is allowed.
- Clarify unilateral trust, revocation, and fallback behavior.
- Integrate acknowledgements when Low-Latency v2 is available.

### Migration and Coexistence
- Define how HRMP and speculative messaging run in parallel.
- Clarify per-channel or per-parachain enablement.
- Add rollback and upgrade sequencing guidance.

### Production Hardening
- Formalize PoV / validation ABI extensions.
- Tighten proof size and storage growth guarantees.
- Expand adversarial testing and security review scope.
- **`SpeculativeOutboxApi` / `SpeculativeInboxApi` required for lookahead collation** —
  Both APIs are bounds on `StartLookaheadAuraConsensus` (and the omni-node lookahead
  path). They are NOT in `NodeRuntimeApi`, so Asset Hub, Bridge Hub, and other
  parachains that have not integrated the speculative pallets are unaffected as long
  as they do not use the lookahead collator with speculative sources configured. Any
  parachain that wants to use `--speculative-sender` must implement both APIs (stub
  impls returning `None`/empty are sufficient for non-speculative chains). The
  slot-based collator has no such bounds in the current POC and can be used without
  implementing these APIs. In production the lookahead path should use
  `ApiExt::has_api` checks to make speculative messaging opt-in at runtime rather
  than required at compile time.
- **`RpcOutboxClient.best_block_hash()` uses `block_in_place`** — the current
  implementation calls `chain_getHead` synchronously via `tokio::task::block_in_place`
  to satisfy the non-async `fn best_block_hash(&self) -> Hash` signature. This is
  acceptable for the POC (fast RPC read, infrequent call path) but should be
  replaced with a cached/subscribed best-hash approach for production.
- **HTTP provider as a future `OutboxQuery` implementation** — the `OutboxQuery`
  trait is the right abstraction point. An HTTP provider client that queries a
  running provider process (§7.3) would be a third `OutboxQuery` impl without
  touching any collator logic. Deferred to production hardening.
- **Outbox leaf granularity** — `XcmpMessageSource::take_outbound_messages`
  records each XCMP page (not individual XCM messages) as a single MMR leaf.
  Sender and receiver must hash the same unit; verify this is consistent before
  expanding to per-message proof generation.
- **`block_number_for_provides_root` linear scan** — `HistoricalProvidesRoots`
  is scanned linearly (O(N) in the retention window) to look up a block number
  by root hash. A reverse index `(H256 → BlockNumber)` would make this O(1).
- **LBP retention beyond 256-block window not handled.** Both
  `HistoricalProvidesRoots` and `HistoricalSubtreeState` retain only the last
  256 source blocks. If the receiver lags by more than the retention window,
  `generate_late_block_proof` returns `None` and the receiver collator skips
  the batch (§14.4 failure modes). For production: extend retention or
  introduce a checkpoint-based catch-up scheme.
- **Legacy parachain backward-compatibility break in candidate-validation.** The
  current POC extends `v9::CandidateCommitments` in place with `provides` and
  `requires` fields. Node-side candidate validation always hashes the 8-field
  struct (including `provides: None, requires: []` for non-speculative candidates).
  A legacy parachain collator (one that has not yet integrated the speculative
  pallets) hashes the old 6-field struct — the two extra `None`/empty SCALE bytes
  make the hashes differ, causing **all V2/V3 candidates from legacy parachains to
  fail the commitments hash check** on new relay chain nodes.
  The POC is unaffected because both Penpal instances run the upgraded runtime.
  **Fix plan (pre-production):** properly freeze `v9::CandidateCommitments`
  (remove `provides`/`requires`) and introduce a genuinely additive
  `CandidateCommitments` with those fields. Steps:
  1. Remove `provides`/`requires` from `v9::CandidateCommitments`; update
     `v10/mod.rs` to define a new struct (not a re-export) with all v9 fields
     plus the two new ones. Add `From<v9> for v10` (provides=None, requires=[]).
  2. Make `CommittedCandidateReceipt` generic over the commitments type, or
     introduce a `CommittedCandidateReceiptV4` — the generic approach is cleaner.
  3. Update `collation-generation`: V4 descriptor path produces v10 commitments;
     V2/V3 path stays with v9. Hash boundary is intentional — different versions,
     different encodings.
  4. Update `candidate-validation` and `backing`: thread the right commitments
     type through `BackgroundValidationOutputs` based on descriptor version.
  5. Update relay chain runtime: store v10 in `PendingAvailability` (convert
     v9→v10 on admission); read `provides`/`requires` from v10 at enactment.
  Legacy parachains without speculative pallets continue producing V2/V3
  candidates with unchanged v9 encoding — zero impact on them.
- **No HRMP fallback when speculative pathway is used.** The outbox pallet's
  `XcmpMessageSource::take_outbound_messages` returns `Vec::new()`, suppressing
  standard HRMP delivery. If the speculative pathway breaks (no receiver
  collator fetching, relay root mismatch, fork suppression), those messages
  are silently dropped — no fallback delivery mechanism exists. A sender
  running with the speculative-outbox pallet cannot deliver messages to
  receivers that lack the speculative-inbox pallet.
  **Fix plan (pre-production):** either (a) make the outbox return messages
  from `take_outbound_messages` in addition to recording them, or (b) add a
  runtime flag per destination to toggle between speculative-only and dual-path
  delivery. Option (b) is cleaner — dual-path means messages flow through both
  speculative (fast) and HRMP (reliable fallback); the receiver deduplicates.
- **Inherent `ingest_verified_messages` has no weight or size bounds.** The
  call is annotated `(0, DispatchClass::Mandatory)` with no explicit limit on
  `batches.len()` or per-batch `messages.len()`. While the collator-side cap
  (`max_messages_per_source = 32`) and `MAX_REQUIRES_PER_BLOCK = 32` provide
  practical bounding in the happy path, the unpriced weight means a buggy or
  malicious inherent could submit an oversized payload that exceeds block
  weight/proof-size limits at zero cost to the submitter.
  **Fix plan (pre-production):** add a `RefundWeight`-style annotation or
  explicit `batch_count * per_batch_weight + message_count * per_message_weight`
  so the block author pays proportionally to the data submitted.
- **Duplicated `Keccak256Merge` across crates — resolved by the primitives
  crate.** `Keccak256Merge` was defined identically in `speculative-inbox`,
  `speculative-outbox`, and `parachain-system/src/validate_block`. The shared
  `cumulus-primitives-spec-messaging` crate now provides the single domain-tagged
  `blake2_256` `SpecMerge` (an `mmr_lib::Merge`); the §3.6 integration checklist
  (steps 3–5) tracks pointing all three call sites at it. The receiver inbox
  carries no local MMR — it verifies per-batch inclusion proofs via
  `mmr_lib::MerkleProof::<Hash, SpecMerge>::verify`.
- **`HistoricalProvidesRoots` / `HistoricalSubtreeState` storage growth.**
  The 256-block retention window prunes per-block entries in `on_finalize`,
  but `HistoricalSubtreeState` stores `(root, peaks, leaf_count)` per block
  *per destination* — with many parachains this could accumulate significant
  on-chain state within the window. Confirm worst-case storage under full load.
  Consider reducing retention or pruning more aggressively.
- **`subtree_inclusion_proof` removed by the flat commitment.** The old
  `speculative-outbox` RPC rebuilt a sorted root list and a top-level Merkle
  proof on every call (O(D log D)). Under the flat commitment there is no
  top-level tree and no inclusion proof: `compute_provides()` returns the sorted
  `CommitmentSet` directly and the relay looks up entries by destination. The
  remaining optimization is just caching the `CommitmentSet` once per block in
  `on_finalize` rather than re-iterating `OutgoingMMRs`.
- **`ParachainHost` API version bumped 16→17 without migration guard.**
  Adding `provides_root` to the vstaging runtime API increments the
  `ParachainHost` version. Older nodes calling API v16 get an
  `ApiError::Version` — harmless for non-speculative use but worth noting
  in release notes. The `rococo`/`westend` genesis presets enable
  `SpeculativeMessaging` unconditionally; a runtime upgrade gating the feature
  behind a governance flag would be safer.
- **End-to-end integration test for the full pipeline.** The existing tests
  cover individual components (MMR, Merkle proofs, inclusion, inbox pallet
  integration), but there is no test exercising the full flow from outbox
  recording → off-chain batch fetch → inbox ingestion → relay inclusion
  with `requires_satisfied`. Adding a `#[test]` that simulates two parachains
  with the full speculative messaging stack would catch integration-level
  regressions early.

### Optional Future Directions
- Super-chain / intra-block messaging.
- Relaxed or unordered delivery semantics.
- Enhanced pruning and garbage collection strategies.

---

## 13. Related Documents

- [speculative-messaging-design.md](speculative-messaging-design.md) — Full high-level design including Late Block Proofs, trust domains, super chains, and LLv2 integration.
- [xcmp-mmd-minimal-poc.md](xcmp-mmd-minimal-poc.md) — Superseded earlier POC using BEEFY-anchored proofs. Retained for historical reference.

---

## 14. Appendix: Collator-Side Code Walkthrough

> **Pre-revision (as-built).** This appendix traces the **current POC code**,
> which still uses Keccak256, a two-level Merkle commitment, and the old
> `provides_root` runtime API. It documents what exists today, not the target.
> The §3.6 integration checklist tracks migrating it onto `blake2_256` + the flat
> commitment (see the revision note at the top of this doc).

End-to-end trace of how speculative messaging fields flow through the collator
pipeline on each slot, with file and line references.

### 14.1 Entry Point

```
cumulus/polkadot-omni-node/lib/src/nodes/aura.rs:948
  StartLookaheadAuraConsensus::start_consensus()
    → aura::run_with_export()          [cumulus/client/consensus/aura/src/collators/lookahead.rs]
```

`StartLookaheadAuraConsensus` is the omni-node's lookahead collator handle. It
carries `speculative_sources` (populated from `--speculative-sender` CLI args at
startup) down into the per-slot loop inside `run_with_export`.

### 14.2 Per-Slot Flow

**Step 1 — Fetch ingress** (`lookahead.rs:445–458`)

```rust
let speculative_ingress = if params.speculative_sources.sources.is_empty() {
    empty_speculative_ingress()
} else {
    fetch_ingress_for_block(...).await   // speculative_ingress.rs
};
```

`fetch_ingress_for_block` queries each configured sender chain via the
`OutboxQuery` trait (the POC ships `RpcOutboxClient`; future implementations
plug in without touching the collator), builds `MessageBatch`es, and returns
`(SpeculativeIngress, Vec<LateBlockProof>)`.

**Root guard.** After building each batch, `fetch_ingress_for_block` checks
whether the relay chain has already committed a matching `provides_root` for the
source parachain (`relay_provides_root == batch.provides_root`). If not, the batch
is silently dropped and the block is built without speculative ingress (`n_requires=0`).
This ensures the relay's `requires_satisfied` check passes at inclusion time,
eliminating the fork-contention stall that occurs when a premature `n_requires=1`
candidate competes with simpler `n_requires=0` forks. Tradeoff: the narrow
pre-enactment speculative window (~6–18 s before the relay commits the sender's
block) is lost; steady-state delivery latency is ~18–30 s (sender relay-inclusion
plus receiver slot alignment).

**Step 2 — Inject as inherent** (`lookahead.rs:468–485`)

```rust
let (parachain_inherent_data, other_inherent_data) =
    collator.create_inherent_data(..., Some(speculative_ingress)).await;
```

`create_inherent_data` puts the `SpeculativeIngress` into `InherentData` under
`INHERENT_IDENTIFIER`. During block construction, `ProvideInherent`
for `pallet-speculative-inbox` picks it up and creates the
`ingest_verified_messages` extrinsic in the block body.

**Step 3 — Build block and collation** (`lookahead.rs:512–521`)

```rust
collator.collate(..., (parachain_inherent_data, other_inherent_data), ...).await
```

Internally calls `build_block_and_import` (executes the block, running
`ingest_verified_messages` which verifies batches, updates `IncomingState`, and
writes `ConsumedSourcesThisBlock`), then `collator_service.build_collation` to
wrap the block into a `Collation` with its PoV.

**Step 4 — Patch speculative fields** (`lookahead.rs:536–551`)

```rust
let runtime_api = para_client.runtime_api();
let provides = runtime_api.compute_provides_root(new_block_hash).unwrap_or(None);
let requires = runtime_api.requires_commitments(new_block_hash).unwrap_or_default();
collation.provides = provides;
collation.requires = requires;
```

After block execution, these runtime API calls read the sender-side MMR root
(`OutgoingMMRs` → `compute_provides_root`) and the receiver-side consumed sources
(`ConsumedSourcesThisBlock` → `requires_commitments`) from the newly built block's
state. The values are patched directly onto the `Collation` struct.

**Fork suppression.** Immediately after patching, the collator checks whether
`collation.requires` is non-empty (i.e. `n_requires > 0`). If so, it inserts the
block number into a per-session `BTreeSet<BlockNumber>` called
`speculative_built_heights`. On any subsequent relay-parent notification, if the
collator would build a `n_requires=0` collation at a height already in the set, it
skips the `SubmitCollation` call and advances `parent_hash` without submitting.
The set is pruned to heights above the current `included_header.number()` on each
relay-parent iteration.

This prevents the relay from being offered a simpler non-speculative alternative
after a speculative one has already been submitted, closing the window where the
relay could back the wrong fork and cause a post-delivery stall.

**Step 5 — Commitments assembly** (`polkadot/node/collation-generation/src/lib.rs:620–647`)

```rust
let provides = collation.provides.map(|p| ProvidesCommitment { root: p.root });
let requires = collation.requires.into_iter()
    .map(|r| RequiresCommitment { source: r.source, expected_root: r.expected_root })
    .collect();

let commitments = CandidateCommitments {
    upward_messages, horizontal_messages, new_validation_code,
    head_data, processed_downward_messages, hrmp_watermark,
    provides,
    requires,
};
```

The collation-generation subsystem reads `collation.provides/requires`, maps them
into the primitives types, and assembles `CandidateCommitments`. The commitments
are hashed to produce `commitments_hash` in the `CandidateDescriptor`, which is
what backing validators check against after running the PVF.

### 14.3 Relay Chain Side Walkthrough

All relay-chain speculative messaging logic lives in
`polkadot/runtime/parachains/src/inclusion/mod.rs`.

**Storage** (line 376)

```rust
pub(crate) type ProvidesRoots<T: Config> =
    StorageMap<_, Twox64Concat, polkadot_primitives::Id, polkadot_primitives::Hash>;
```

One hash per parachain — the latest enacted `provides` root. Overwritten on each
successful enactment; no history kept.

**Stage 1 — Backing / pending-availability admission** (`process_candidates`)

`process_candidates` accepts backed candidates and moves them into
`PendingAvailability`. The full `CandidateCommitments` (including `provides` and
`requires`) are stored there unchanged. No `requires` satisfaction check happens
here — the relay chain does not gate backing on speculative dependencies.

**Stage 2 — Availability check and enactment gate** (lines 590–603)

Once a candidate accumulates enough availability votes, the relay chain decides
whether to enact it:

```rust
if can_enact &&
    candidate.descriptor.version() == CandidateDescriptorVersion::V4 &&
    !Self::requires_satisfied(&candidate.commitments.requires)
{
    // Drop this candidate and all its descendants.
    drop_from_index = Some(candidate_index);
    can_enact = false;
}
```

`requires_satisfied` is a pure hash comparison (line 919):

```rust
pub(crate) fn requires_satisfied(requires: &[RequiresCommitment]) -> bool {
    requires.iter().all(|r| Self::provides_root(&r.source) == Some(r.expected_root))
}
```

If unsatisfied, the candidate is dropped immediately rather than stalling the
core — letting the collator retry on the next slot. The check is only applied to
V4 candidates; V1/V2/V3 candidates skip it entirely.

**Stage 3 — Enactment** (`enact_candidate`, lines 991–1003)

```rust
if !Self::requires_satisfied(&commitments.requires) {
    defensive!("requirements no longer satisfied at enactment");
} else if let Some(ref p) = commitments.provides {
    Self::update_provides_root(receipt.descriptor.para_id(), p.root);
}
```

After all standard effects (UMP, HRMP, head update), the relay chain:
1. Re-checks `requires` as a defensive assertion (should always pass since stage 2 already verified it)
2. If the candidate has a `provides` commitment, writes `ProvidesRoots[para_id] = root`

This persisted root is what future receiver candidates match against in their
`RequiresCommitment.expected_root`.

**What the relay chain does NOT do**

- No MMR or Merkle proof verification — all cryptographic work is in the PVF
- No message payload storage — payloads live in the parachain's block body
- No history — only the latest `ProvidesRoots` entry per parachain is kept
- No in-block ordering — a providing and consuming candidate in the same relay
  block: the provider must be enacted first; the consumer succeeds in relay
  block N+1

### 14.4 Late Block Proof Collator Path

When the receiver collator builds a block whose batches reference an older
`provides_root` than the relay's current `ProvidesRoots[source]`, it must
attach a `LateBlockProof` so the PVF can transform
`requires[source].expected_root` from the batch root to the relay root before
the relay's `requires_satisfied` check at enactment.

This path is fully wired. End-to-end:

**1. Sender exposes the proof generator.**
`SpeculativeOutboxApi::generate_late_block_proof(dest, old_provides_root) ->
Option<LateBlockProof>` is implemented by `pallet-speculative-outbox`
(`speculative-outbox/src/lib.rs`). It looks up the historical block that
produced `old_provides_root` (via `HistoricalProvidesRoots`/`HistoricalSubtreeState`),
recomputes the current state, and assembles old/new subtree Merkle proofs
plus an optional `MMRExtensionProof`.

**2. Collator-side trait method.** `OutboxQuery::generate_late_block_proof`
(`cumulus/client/consensus/aura/src/collators/outbox_client.rs`):

```rust
async fn generate_late_block_proof(
    &self,
    at: Hash,
    dest: ParaId,
    old_provides_root: Hash,
) -> Option<LateBlockProof>;
```

The `RpcOutboxClient` impl maps to
`state_call("SpeculativeOutboxApi_generate_late_block_proof", at, (dest, old_provides_root).encode())`.

**3. Receiver fetch logic.** `fetch_ingress_for_block`
(`speculative_ingress.rs`) returns `(SpeculativeIngress, Vec<LateBlockProof>)`.
For each source S:

- Read `R_relay = relay_client.provides_root(S, relay_parent)`.
- If `R_relay` is `None` → root guard skips the source (sender not relay-enacted).
- Pick `fetch_at` on the sender by `sender.block_hash_for_provides_root(R_relay)`.
- Build the `MessageBatch` at `fetch_at`. Its `provides_root` is `R_batch`.
- If `R_batch == R_relay` → push the batch, no LBP needed.
- If `R_batch != R_relay` → fetch
  `sender.generate_late_block_proof(lbp_at, destination, R_batch)` where
  `lbp_at` is the sender block whose root equals `R_relay`. Push both batch
  and LBP.

**4. Threading into the PoV.** `Collator::collate`
(`cumulus/client/consensus/aura/src/collator.rs`) takes a
`late_block_proofs: Vec<LateBlockProof>` argument and forwards it to
`CollatorService::build_multi_block_collation_with_late_block_proofs`, which
constructs `ParachainBlockData::new_v2(blocks, compact_proof, late_block_proofs)`
and SCALE-encodes that into the PoV. The receiver runtime block body is
unchanged — LBPs ride only in the PoV scaffolding.

**5. PVF transforms requires.** During `validate_block` (PVF), the runtime
decodes `ParachainBlockData::V2`, executes the block, then calls
`apply_messaging_proofs(self_para_id, &mut extension, late_block_proofs)`
(`parachain-system/src/validate_block/implementation.rs`). For each LBP that
matches a `(source, old_provides_root)` entry in `extension.requires`, the
PVF verifies the two binary Merkle proofs (old and new subtree), the MMR
extension if `old_subtree_root != new_subtree_root`, and rewrites
`req.expected_root = proof.new_provides_root`.

**6. Relay enactment.** The candidate's `requires[source].expected_root` now
equals `R_relay`, so `Inclusion::requires_satisfied` (which compares against
`ProvidesRoots[source]`) accepts the candidate.

**Failure modes.**

- `block_hash_for_provides_root` returns `None` (sender pruned history beyond
  256-block retention): collator falls back to `sender_best`, but the LBP's
  `new_provides_root` may not equal `R_relay` → `apply_messaging_proofs`
  transforms to a stale root → still `UnsatisfiedRequires`. The collator
  retries on the next slot.
- `generate_late_block_proof` returns `None` (RPC failure or sender pruned
  the old root): `fetch_ingress_for_block` logs a warning and skips the
  batch. Same retry behavior.
- Race between LBP fetch and enactment (relay advances past `R_relay`):
  candidate is rejected at enactment; collator retries with fresh data on
  the next slot.

**No changes** to PVF, relay chain, or `collation-generation` were required
to wire this — only `OutboxQuery`, `fetch_ingress_for_block`,
`Collator::collate`, and `CollatorService::build_multi_block_collation_*`.

### 14.5 Summary Diagram

```
StartLookaheadAuraConsensus::start_consensus()  [aura.rs:948]
  └─ aura::run_with_export()  [lookahead.rs]
       └─ per slot:
            ├─ fetch_ingress_for_block()  [speculative_ingress.rs]
            │    └─ OutboxQuery (RpcOutboxClient)
            │         → MessageBatch[]  +  LateBlockProof[]
            │         →  (SpeculativeIngress, Vec<LateBlockProof>)
            │
            ├─ collator.create_inherent_data(Some(speculative_ingress))
            │    └─ InherentData[INHERENT_IDENTIFIER] = ingress
            │
            ├─ collator.collate(inherent_data, late_block_proofs)  [collator.rs]
            │    ├─ build_block_and_import()
            │    │    └─ block execution:
            │    │         ingest_verified_messages()  ← verifies subtree proof
            │    │         and combined MMR proof, updates IncomingState,
            │    │         writes ConsumedSourcesThisBlock
            │    └─ collator_service.build_multi_block_collation_with_late_block_proofs()
            │         → ParachainBlockData::V2 { blocks, proof, late_block_proofs }
            │         → Collation (provides/requires still empty)
            │
            ├─ runtime_api.compute_provides_root(new_block_hash)
            │    → collation.provides = Some(ProvidesCommitment { root })
            ├─ runtime_api.requires_commitments(new_block_hash)
            │    → collation.requires = Vec<RequiresCommitment>
            │      (PVF will later transform expected_root via
            │       apply_messaging_proofs(late_block_proofs))
            │
            └─ collation-generation subsystem  [collation-generation/src/lib.rs:620]
                 → CandidateCommitments { ..., provides, requires }
                 → commitments_hash in CandidateDescriptor
                 → submitted to relay chain for backing
```

---

## 15. Appendix: Execute-First, Match-Later — The Core Mental Model

### 15.1 How This Differs From HRMP

In HRMP the relay chain is in the **critical path** of message delivery:

```
Sender enacted on relay  →  relay stores payload in HRMP queue
                         →  relay delivers to receiver at next inclusion
                         →  receiver runtime reads from downward/HRMP queue
```

The relay mediates every step. The receiver cannot act until the relay has
already confirmed the sender and routed the payload.

Speculative messaging inverts this:

```
Receiver executes XCM speculatively (local, off-chain fetch)
  →  block built, backed, made available
  →  relay checks dependency at enactment time
  →  pass → state changes canonical  /  fail → block dropped, retry
```

The relay chain is no longer in the critical path of *execution* — it is only
in the critical path of *settlement*.

### 15.2 What "Speculative" Means Precisely

When the receiver collator builds a block it:

1. Fetches the sender's `MessageBatch` directly over RPC (`RpcOutboxClient`)
2. Injects it as a `SpeculativeIngress` inherent
3. Executes the block — `ingest_verified_messages` verifies the subtree proof
   locally, dispatches the XCM payload through `XcmpMessageHandler`, updates
   balances and other state, and writes `ConsumedSourcesThisBlock`
4. Reads back `requires_commitments()` from its own post-execution state and
   patches `collation.requires`

All of step 3 happens **before the relay chain has seen anything**. The receiver
bets that the sender's block will be relay-committed by the time the receiver's
own block reaches enactment. That is the speculation.

The relay chain's `requires_satisfied` check at enactment time is the settlement:

- **Bet wins** (sender enacted before or at receiver enactment): block is enacted,
  XCM effects become canonical.
- **Bet loses** (sender not yet enacted): block is dropped, state changes
  discarded, collator retries next slot with a fresh fetch.

### 15.3 Failure Is Safe

Because the entire receiver block is discarded atomically on a failed
`requires_satisfied` check, there is no partial execution risk. The XCM payload
is either fully applied (canonical) or fully rolled back (dropped), never half-way.

The receiver chain itself never stalls — a dropped speculative block is simply
not enacted; the collator builds a new block next slot, either with updated
speculative ingress or without it (`n_requires=0`) if the RPC is unavailable.
Degraded mode is normal block production with no speculative delivery.

### 15.4 Why This Enables Lower Latency

HRMP delivery latency is:

```
sender tx → sender relay-included → relay routes → receiver relay-included
≈ (1-2 relay blocks for sender) + (1-2 relay blocks for delivery)
≈ 12–24 s
```

Speculative delivery latency is:

```
sender tx → sender relay-included → receiver builds with batch (+ optional LBP)
        → receiver relay-included
≈ (1-2 relay blocks for sender enactment) + (receiver slot alignment)
≈ 18–30 s
```

The relay's message-routing step is eliminated. The receiver acts as soon as
the sender's `ProvidesRoots[source]` is populated on the relay, using a
`LateBlockProof` to bridge any gap between the batch's root and the relay's
current root (§14.4). HRMP would have required two relay blocks for routing
*on top of* the sender enactment.

---

## 16. Appendix: Where CollationGeneration Runs

### 16.1 The Subsystem Is Collator-Side

`polkadot/node/collation-generation/src/lib.rs` is the `CollationGeneration`
subsystem. Despite living under `polkadot/node/`, it runs inside the **collator
node process**, not on validator nodes. It is wired into the collator's overseer,
not the validator's.

### 16.2 What It Does

After the lookahead collator builds a block and patches `collation.provides` /
`collation.requires` (§14.2 steps 4–5), it sends a
`CollationGenerationMessage::SubmitCollation` to the overseer. The
`CollationGeneration` subsystem picks this up and:

1. **Maps speculative fields into `CandidateCommitments`**

```rust
let provides = collation.provides.map(|p| ProvidesCommitment { root: p.root });
let requires = collation.requires.into_iter()
    .map(|r| RequiresCommitment { source: r.source, expected_root: r.expected_root })
    .collect();

let commitments = CandidateCommitments { ..., provides, requires };
```

Without this step the speculative fields set by the lookahead collator would be
silently dropped and never reach the candidate receipt.

2. **Selects the right descriptor version**

```rust
let speculative_enabled = FeatureIndex::SpeculativeMessaging.is_set(&node_features);
let use_v4 = speculative_enabled && has_speculative;

let descriptor = if use_v4 {
    CandidateDescriptorV2::new_v4(...)   // carries speculative commitments
} else if scheduling_parent.is_some() && v3_enabled {
    CandidateDescriptorV2::new_v3(...)
} else {
    CandidateDescriptorV2::new(...)      // V2 legacy
};
```

V4 is gated on both the runtime feature flag (`FeatureIndex::SpeculativeMessaging`
queried from the relay chain) and whether the collation actually has speculative
content (`has_speculative = provides.is_some() || !requires.is_empty()`). This
means speculative fields are only used when the relay chain runtime supports them,
providing a clean upgrade path.

3. **Builds and submits the `CommittedCandidateReceipt`**

The commitments are hashed to produce `commitments_hash` in the descriptor.
The finished receipt and PoV are sent to backing validators.

### 16.3 Validators Are Downstream

Validators receive the finished receipt and PoV from the collator. They run the
PVF (`validate_block`) to independently recompute commitments and verify the hash
matches the receipt — but they do not construct the receipt themselves. All
descriptor version selection and speculative field mapping happens on the collator
side in this subsystem.

```
Collator node
  lookahead.rs          — builds block, patches collation.provides/requires
  collation-generation  — maps into CandidateCommitments, selects V4 descriptor,
                          builds CommittedCandidateReceipt, submits to validators

Validator node
  candidate-validation  — runs PVF, recomputes commitments, checks hash
  inclusion/mod.rs      — enactment-time requires_satisfied check
```


