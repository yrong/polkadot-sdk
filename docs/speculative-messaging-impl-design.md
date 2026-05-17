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
   `LateBlockProof`s in `ParachainBlockDataV4` and encodes as the PoV content.
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
   decodes `ParachainBlockDataV4` from the PoV bytes (getting both block data and
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

New types go into a **new `v10` primitives module**. The existing `v9` types are
frozen. New speculative-messaging candidates use `v10` types, while legacy
candidates continue to use the existing `v9` path.

```
polkadot/primitives/src/v10/mod.rs  ← NEW FILE
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

Concretely, the intended behavior:

- legacy candidates keep their existing commitments layout and validation/reconstruction path
- v4 candidates use the extended commitments layout with speculative messaging fields
- relay-chain inclusion only enforces requires/provides matching for v4+ candidates
- node-side candidate validation reconstructs commitments according to the candidate descriptor version

This means the upgrade is additive: pre-v4 parachains remain valid, v4 parachains
opt into new semantics, and both formats coexist during migration. Every component
that touches commitments is version-aware:

- **Collator** (§5.3): branches on `api_version` to include speculative fields for
  v4 parachains, skips them for legacy.
- **Relay chain backing** (§4.2): `process_candidates` accepts both formats
  unchanged into `PendingAvailability`.
- **Relay chain enactment** (§4.2): only enforces `requires`/`provides` matching
  for `descriptor.version() >= V4` candidates.
- **Node-side validation** (§6.1): reconstructs `v9::CandidateCommitments` for V1–V3,
  `v10::CandidateCommitments` for V4.
- **Relay chain runtime API** (`check_validation_outputs`): accepts the extended
  type from the start, ignoring optional speculative fields for pre-speculative
  candidates.

```rust
// In v10/mod.rs:
// Note: the POC does NOT introduce a new CandidateDescriptorV4 struct. Instead,
// CandidateDescriptorV2::new_v4() sets version byte 2, making version() return
// CandidateDescriptorVersion::V4. The struct layout is unchanged.

pub struct CandidateCommitments<N = BlockNumber> {
    pub upward_messages: UpwardMessages,
    pub horizontal_messages: HorizontalMessages,  // HRMP (legacy, coexists in Phase 1)
    pub new_validation_code: Option<ValidationCode>,
    pub head_data: HeadData,
    pub processed_downward_messages: u32,
    pub hrmp_watermark: N,

    // ── New speculative messaging fields ──
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

## 3. Primitives (polkadot-primitives v10)

### 3.1 Commitment Types

```rust
/// A commitment that a parachain provides a set of outbound messages.
/// The root is the top-level Merkle root over all per-destination MMR roots.
#[derive(Clone, Encode, Decode, PartialEq, RuntimeDebug, TypeInfo)]
pub struct ProvidesCommitment {
    /// Top-level Merkle root over all per-destination MMR roots.
    pub root: Hash,
}

/// A commitment that a parachain requires messages from a source parachain.
#[derive(Clone, Encode, Decode, PartialEq, RuntimeDebug, TypeInfo)]
pub struct RequiresCommitment {
    /// The source parachain whose provides root we expect.
    pub source: ParaId,
    /// The provides root we built against (the source chain's top-level root at the
    /// block from which we received messages).
    pub expected_root: Hash,
}
```

This split is intentional: subtree roots remain internal runtime state used for
message-batch verification, while `RequiresCommitment.expected_root` always
refers to the sender's top-level `ProvidesCommitment.root`, which is the value
matched by the relay chain.

Two invariants are implicit and should be treated as part of the Phase 1 design:

1. **Canonicalization of `requires`** — sort entries by `source: ParaId`
   ascending, allow at most one entry per source, reject duplicates before
   hashing. Semantically equivalent dependency sets must produce identical
   `CandidateCommitments` hashes.

2. **Exact top-level root construction** — gather `(destination_para_id,
   subtree_root)` pairs from the sender's per-destination outbox state, sort by
   `destination_para_id`, compute each leaf as `keccak256(SCALE(destination_para_id,
   subtree_root))`, and compute the Merkle root over that ordered leaf list. All
   proof generation, proof verification, and relay-visible commitment matching
   must use this exact same keyed-leaf encoding.

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
    /// The top-level provides root for this block
    pub provides_root: Hash,
    /// The per-destination MMR root for the receiver
    pub subtree_root: Hash,
    /// Merkle proof that subtree_root is in provides_root.
    /// Length: O(log D) where D = number of destinations.
    pub subtree_inclusion_proof: Vec<Hash>,
    /// Total number of destinations in the top-level Merkle tree.
    /// Required by `binary_merkle_tree::verify_proof` as an explicit parameter.
    pub number_of_destinations: u32,
    /// The 0-based index of the receiver's leaf in the top-level Merkle tree
    /// (leaves sorted by destination ParaId).
    /// Required by `binary_merkle_tree::verify_proof` as an explicit parameter.
    pub leaf_index: u32,
    /// The messages with their positions in the sender's subtree MMR.
    pub messages: Vec<OutgoingMessage>,
}

#[derive(Clone, Encode, Decode, Debug)]
pub struct OutgoingMessage {
    /// Zero-based position in the source's per-destination MMR.
    pub position: u64,
    /// Raw XCM message bytes (what gets passed to `handle_xcmp_messages`).
    pub payload: Vec<u8>,
}
```

For the minimal POC, this shape is sufficient. It contains everything the
receiver needs to verify that the destination-specific subtree is included in the
sender's top-level `provides_root`, verify per-source ordered continuity of
messages against local receiver state, reconstruct the receiver's local subtree
and check it matches `subtree_root`, and dispatch the verified payloads through
the existing XCMP batch handler.

Invariants:

1. **Canonical subtree proof leaf** — `subtree_inclusion_proof` must always prove
   inclusion of `keccak256(SCALE(destination_para_id, subtree_root))` into
   `provides_root`. The destination parachain is not carried explicitly in
   `MessageBatch` because the receiver already knows "this batch is for me," but
   both sides must use the same keyed leaf format.

2. **Canonical message ordering** — `messages` must be ordered by ascending
   `position` with no duplicates. During verification, the receiver expects them
   to advance continuously from `last_processed + 1`.

3. **Batch-to-root consistency** — `provides_root` commits to `subtree_root`,
   which commits to the ordered message sequence. The receiver checks both links.

4. **Practical bounds** — `subtree_inclusion_proof`, `messages`, and each
   `payload` should have explicit bounds in a production implementation. The POC
   pseudocode can leave them as `Vec`, but the implementation should define
   concrete maxima.

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
// Registration: Defined in primitives or a dedicated pallet.
pub const SPECULATIVE_INGRESS_IDENTIFIER: InherentIdentifier = *b"specingr";

// client-side before proposal
let mut inherent_data = other_inherent_providers.create_inherent_data().await?;
inherent_data.put_data(SPECULATIVE_INGRESS_IDENTIFIER, &ingress)?;

// runtime-side during block construction
impl<T: Config> ProvideInherent for Pallet<T> {
    const INHERENT_IDENTIFIER: InherentIdentifier = SPECULATIVE_INGRESS_IDENTIFIER;

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

For empty blocks (no outbound messages, no inbound messages):
- `provides: None`
- `requires: vec![]`

### 3.5 Late Block Proof Types

When a receiver block is built against an older source `provides_root` than
what's now current on the relay chain, the receiver collator includes a
`LateBlockProof` in the PoV. The collator prechecks the proof and uses the
transformed root in the candidate commitments. The PVF independently verifies
the proof and transforms the `RequiresCommitment` during `validate_block`, before
the relay chain sees it.

```rust
/// Included in the receiver candidate's PoV when the block was built against
/// an older source root than what's persisted in ProvidesRoots.
#[derive(Clone, Encode, Decode, Debug)]
pub struct LateBlockProof {
    /// The source parachain this proof covers.
    pub source: ParaId,

    /// The total number of destinations in the source's provides root.
    /// Required by `binary_merkle_tree::verify_proof`.
    pub number_of_destinations: u32,
    /// The 0-based index of the receiver's subtree in the source's top-level tree.
    /// Required by `binary_merkle_tree::verify_proof`.
    pub leaf_index: u32,

    /// The provides root the receiver block was built against (the old root
    /// from the batch). This is the root that would appear in
    /// RequiresCommitment.expected_root without the proof.
    pub old_provides_root: Hash,
    /// The subtree root the receiver built against (from the old source block).
    pub old_subtree_root: Hash,
    /// Merkle proof that old_subtree_root was in old_provides_root.
    pub old_subtree_proof: Vec<Hash>,

    /// The current provides root of the source (what's now in ProvidesRoots).
    pub new_provides_root: Hash,
    /// The subtree root under the new provides root.
    pub new_subtree_root: Hash,
    /// Merkle proof that new_subtree_root is in new_provides_root.
    pub new_subtree_proof: Vec<Hash>,

    /// If the source produced additional messages to this receiver since the
    /// block was built, this proof shows the old subtree is a valid prefix of
    /// the new subtree.
    pub subtree_extension: Option<MMRExtensionProof>,
}

/// Proves that an MMR root R_old is an ancestor of R_new, i.e. the MMR was
/// only appended to, not mutated.
#[derive(Clone, Encode, Decode, Debug)]
pub struct MMRExtensionProof {
    /// The peaks of the old MMR.
    pub old_peaks: Vec<Hash>,
    /// The peaks of the new (larger) MMR.
    pub new_peaks: Vec<Hash>,
    /// Nodes connecting old peaks to new peaks to prove prefix relationship.
    pub connecting_nodes: Vec<Hash>,
}
```

The canonical leaf format for top-level proofs matches section 3.1:
`keccak256(SCALE(destination_para_id, subtree_root))`. Subtree extension proofs
are per-destination MMR proofs — they follow the standard MMR append-only
verification semantics of `sp-mmr-primitives`.

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

```rust
/// Latest provides root per parachain.
/// Updated each time a v4 candidate with a provides commitment is included.
/// Only the most recent root is stored — old roots are overwritten.
#[pallet::storage]
pub type ProvidesRoots<T: Config> = StorageMap<_, Twox64Concat, ParaId, Hash>;

impl<T: Config> Pallet<T> {
    /// Read the latest provides root for a parachain.
    pub fn provides_root(para_id: &ParaId) -> Option<Hash> {
        ProvidesRoots::<T>::get(para_id)
    }

    /// Update the provides root after a candidate is included.
    pub fn update_provides_root(para_id: ParaId, root: Hash) {
        ProvidesRoots::<T>::insert(para_id, root);
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
    if !requires_satisfied(&commitments.requires) {
        defensive!("requirements no longer satisfied at enactment");
    }
    if let Some(ref p) = commitments.provides {
        update_provides_root(receipt.descriptor.para_id(), p.root);
    }
}
```

The relay chain is not asked to verify message proofs again. It only needs to
inspect the already-validated `provides` / `requires` fields, check dependency
satisfaction, and persist the newest provides root. This is a relay-runtime
inclusion rule change, not a new protocol stage.

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
it. From the relay chain's perspective, the rule is always "the expected_root
must match the latest persisted `ProvidesRoots[source]`" — the PVF handles the
transformation.

Note that this problem is asymmetric: LateBlockProofs are only needed when the
source chain outpaces the destination (i.e., the source produces more blocks, or
the destination's candidate is delayed in the backing pipeline). If the
destination produces blocks faster than the source, the source root remains
stable across multiple destination blocks, and each can match against the
unchanged `ProvidesRoots[source]` without a proof. Faster destination production
is not a problem; slower destination *inclusion* is.

**What causes `ProvidesRoots[source]` to change.** Because the provides root is
the top-level Merkle root over all per-destination subtree roots (see §5.1),
it changes whenever the source produces outbound messages to **any** destination,
not just this receiver. If the source sends messages to other parachains, the
root advances even though this receiver has no new messages. In that case the
receiver collator simply skips the source — no new batches to fetch, no proof
needed. LateBlockProofs are only needed when there *are* new messages for this
receiver *and* the root has advanced between the batch being built and the
receiver's candidate reaching enactment.

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
- **No history.** `ProvidesRoots` stores one hash per parachain, overwritten each
  time a candidate with a new provides root is enacted. There is no per-block
  root history, no MMR of roots, no retention of old values. The relay chain only
  needs the latest root for dependency matching.
- **No new protocol stage.** The relay chain still backs candidates, admits them
  to pending availability, and enacts them. Speculative messaging adds one
  inclusion-time check: `RequiresCommitment.expected_root` must match the latest
  persisted `ProvidesRoots[source]`. That check is a
  hash comparison, not a cryptographic verification.

In short: all cryptographic work lives in the PVF; the relay chain only adds
hash-equality checks on already-validated commitment fields.

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
`OutgoingMessage.position` refers to that per-destination counter,
`ProvidesCommitment.root` is derived from the set of current subtree roots, and a
single sender-wide counter does **not** define the proof/position space used by
receivers.

**MMR implementation approach.** The hierarchical accumulator structure uses two
different constructions:

- **Per-destination subtrees** are MMRs that grow over time. The codebase ships
  `sp-mmr-primitives` (at `substrate/primitives/merkle-mountain-range/`) with
  append, prove, and peek operations. Per-destination subtrees can be implemented
  as instances of `sp_mmr_primitives::MMR` stored in `OutgoingMMRs` and
  `IncomingState`.
- **The top level** is rebuilt every block from the current set of
  `(destination_para_id, subtree_root)` pairs. Since it never needs append-only
  proofs connecting historical roots, a plain binary Merkle tree (not an MMR) is
  sufficient. The canonical construction: sort leaves by `destination_para_id`,
  compute each leaf as `keccak256(SCALE(key))`, and build a standard binary
  Merkle tree.

The top-level tree uses **Keccak256** rather than the Substrate-default Blake2.
Two reasons: (a) the keyed-leaf pattern `keccak256(SCALE(para_id, root))`
prevents second-preimage attacks where an attacker could interpret a leaf hash as
an internal node hash, a known concern with unbalanced or non-padded Merkle
trees; (b) Keccak256 is the EVM-native hash, which simplifies interop with
EVM-side light-client or bridge verifiers that may need to check subtree
inclusion against a top-level provides root in the future.

**Computing the provides root** — called by the collator after block execution to
populate `CandidateCommitments.provides`. Phase 1 uses **cumulative latest-root
semantics**: the root commits to the sender's full current speculative outbox
state after executing this block, not merely "the delta produced by this block."

```rust
pub fn compute_provides_root() -> Option<ProvidesCommitment> {
    let mut roots: Vec<(ParaId, H256)> = OutgoingMMRs::<T>::iter()
        .filter(|(_, state)| state.leaf_count > 0)
        .map(|(dest, state)| {
            let root = bag_peaks(&state.peaks).unwrap_or_default();
            (dest, root)
        })
        .collect();

    if roots.is_empty() {
        return None;
    }

    roots.sort_by_key(|(id, _)| *id);
    let leaves: Vec<Vec<u8>> = roots.iter()
        .map(|(dest, root)| (dest, root).encode())
        .collect();
    Some(ProvidesCommitment {
        root: binary_merkle_tree::merkle_root::<Keccak256, _>(leaves),
    })
}
```

### 5.2 Incoming Message State (Receiver Side)

```rust
/// Per-source tracking.
#[pallet::storage]
pub type IncomingState<T: Config> = StorageMap<
    _, Twox64Concat, ParaId, SourceState,
>;

#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct SourceState {
    /// Last processed message position in the source's subtree MMR.
    pub last_processed: u64,
    /// The source's top-level provides root for the latest batch we accepted.
    /// Used in the `MultipleRootsPerSourceInOneBlock` check.
    pub last_seen_provides_root: H256,
    /// The source's subtree root we last accepted.
    /// Used for diagnostics and LateBlockProof verification (the PVF compares
    /// `proof.old_subtree_root` against this value).
    pub last_seen_subtree_root: H256,
    /// Leaf count of the receiver's local subtree MMR.
    pub mmr_size: u64,
    /// Peaks of the receiver's local subtree MMR — O(log n) hashes sufficient
    /// to reconstruct the root without storing all internal nodes.
    pub mmr_peaks: Vec<H256>,
}

/// Per-block sources actually consumed during THIS block.
/// Cleared in `on_initialize`, populated by `ingest_verified_messages`,
/// then read by a runtime API after block execution to populate
/// `CandidateCommitments.requires`.
#[pallet::storage]
pub type ConsumedSourcesThisBlock<T: Config> = StorageValue<
    _,
    Vec<(ParaId, H256)>, // (source, expected top-level provides root)
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
    // 1. Verify subtree_inclusion_proof
    let leaf = (LOCAL_PARA_ID, batch.subtree_root).encode();
    let leaf_hash = sp_io::hashing::keccak_256(&leaf);
    verify_merkle_proof(batch.provides_root, &batch.subtree_inclusion_proof, leaf_hash)
        .map_err(|_| VerificationError::InvalidSubtreeProof)?;

    // 2. Verify message continuity against collator-local state
    let mut local_state = snapshot.per_source
        .get(&batch.source)
        .cloned()
        .unwrap_or_default();

    for msg in &batch.messages {
        ensure!(
            msg.position == local_state.last_processed + 1,
            VerificationError::NonConsecutiveMessage,
        );
        let msg_hash = sp_io::hashing::keccak_256(&msg.payload);
        local_state.local_subtree.insert_leaf(msg_hash);
        local_state.last_processed = msg.position;
    }

    // 3. Verify computed root matches batch
    ensure!(
        local_state.local_subtree.root == batch.subtree_root,
        VerificationError::SubtreeRootMismatch,
    );

    // 4. Persist updated collator-local snapshot
    local_state.last_seen_provides_root = batch.provides_root;
    local_state.last_seen_subtree_root = batch.subtree_root;
    snapshot.per_source.insert(batch.source, local_state);

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

        let mut consumed = Vec::new();

        for batch in ingress.batches {
            let leaf = (T::SelfParaId::get(), batch.subtree_root).encode();
            let leaf_hash = sp_io::hashing::keccak_256(&leaf);
            verify_merkle_proof(batch.provides_root, &batch.subtree_inclusion_proof, leaf_hash)
                .map_err(|_| Error::<T>::InvalidSubtreeProof)?;

            let mut state = IncomingState::<T>::get(&batch.source).unwrap_or_default();
            for msg in &batch.messages {
                // Guard against position-0 edge case: when mmr_size == 0 the
                // first-ever message must be at position 0, not last_processed + 1.
                let expected_position = if state.mmr_size == 0 { 0 } else { state.last_processed + 1 };
                ensure!(msg.position == expected_position, Error::<T>::NonConsecutiveMessage);
                let msg_hash = sp_io::hashing::keccak_256(&msg.payload);
                state.local_subtree.insert_leaf(msg_hash);
                state.last_processed = msg.position;
            }

            ensure!(state.local_subtree.root == batch.subtree_root, Error::<T>::SubtreeRootMismatch);

            // Phase 1 invariant: one distinct top-level provides root per source per block
            if state.last_processed > 0 {
                ensure!(
                    state.last_seen_provides_root == batch.provides_root ||
                        !consumed.iter().any(|(source, _)| source == &batch.source),
                    Error::<T>::MultipleRootsPerSourceInOneBlock,
                );
            }

            state.last_seen_provides_root = batch.provides_root;
            state.last_seen_subtree_root = batch.subtree_root;
            IncomingState::<T>::insert(batch.source, state);
            consumed.push((batch.source, batch.provides_root));

            // Re-batch and dispatch through the standard XCMP handler
            let encoded_batch = encode_xcmp_batch(
                batch.messages.iter().map(|msg| msg.payload.as_slice())
            );
            let max_weight =
                <ReservedXcmpWeightOverride<T>>::get().unwrap_or_else(T::ReservedXcmpWeight::get);
            T::XcmpMessageHandler::handle_xcmp_messages(
                core::iter::once((
                    batch.source,
                    batch.source_relay_parent_number,
                    encoded_batch.as_slice(),
                )),
                max_weight,
            );
        }

        ConsumedSourcesThisBlock::<T>::put(consumed);
        Ok(())
    }
}
```

**Encoding for the XCMP handler.** The existing `XcmpMessageHandler::handle_xcmp_messages`
interface (defined in `polkadot/parachain/src/primitives.rs`) takes an iterator
of `(ParaId, RelayChainBlockNumber, &[u8])` where each `&[u8]` is an XCMP
*page* — a byte slice prefixed with an `XcmpMessageFormat` tag followed by
concatenated message data. The `encode_xcmp_batch` helper produces this page
format:

```rust
fn encode_xcmp_batch<'a>(payloads: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut page = XcmpMessageFormat::ConcatenatedVersionedXcm.encode();
    for payload in payloads {
        page.extend_from_slice(payload);
    }
    page
}
```

The format variant must match what the receiver's `XcmpMessageHandler`
implementation knows how to decode. For the POC, using
`ConcatenatedVersionedXcm` throughout is the simplest consistent choice. When
configuring the test runtime (e.g., Penpal/Rococo), **verify that the
destination's XCMP handler actually decodes this exact variant**. A mismatch in
the format tag (e.g. if the receiver expects `ConcatenatedOpaqueVersionedXcm`)
will cause the handler to reject the page, leading to silent message drops.

### 5.3 Producing Commitments

After block execution, the collator reads the provides/requires from runtime
storage and populates `CandidateCommitments`. Phase 1 enforces at most one
`RequiresCommitment` per source parachain per block.

**Codebase integration.** In the current codebase,
`cumulus/client/collator/src/service.rs` line 238 calls `fetch_collation_info`
to retrieve `CollationInfo` (upward_messages, horizontal_messages, head_data,
etc.) and assembles `CandidateCommitments` from it at line ~294. For speculative
messaging, the collator makes two additional runtime API calls right after
`fetch_collation_info` and adds the results to the commitments struct:

```rust
// In cumulus/client/collator/src/service.rs, after fetch_collation_info:
let commitments = if api_version >= SPECULATIVE_API_VERSION {
    // v4+ parachain: include speculative fields
    CandidateCommitments {
        // ... existing fields from collation_info ...
        provides: self.runtime_api.compute_provides_root(block_hash)?,
        requires: self.runtime_api.requires_commitments(block_hash)?,
    }
} else {
    // Legacy parachain: unchanged v9 path, no speculative fields
    CandidateCommitments {
        // ... existing fields only ...
    }
};
```

The collator already branches on `api_version` for PoV encoding format in the
existing code (line ~267). The same pattern gates speculative fields — a
speculative-capable runtime exports `compute_provides_root` and
`requires_commitments` at the known API version; a non-speculative runtime
doesn't. No new `CollationInfo` fields or pipeline changes needed.

```rust
pub fn requires_commitments() -> Vec<RequiresCommitment> {
    let mut consumed = ConsumedSourcesThisBlock::<T>::get();
    consumed.sort_by_key(|(source, _)| *source);
    consumed.dedup_by_key(|(source, _)| *source);

    consumed.into_iter().map(|(source, provides_root)| RequiresCommitment {
        source,
        expected_root: provides_root,
    }).collect()
}

If late block proofs were prechecked (§6.2), the collator overrides the
transformed root for each source with a proof before assembling commitments:

```rust
for proof in &self.prechecked_late_block_proofs {
    if let Some(req) = requires.iter_mut().find(|r| r.source == proof.source) {
        req.expected_root = proof.new_provides_root;
    }
}
requires.sort_by_key(|r| r.source);
```

**POC status:** The above override is not yet implemented in the collator. In the current
POC, `late_block_proofs` is always `Vec::new()` and no pre-flight LBP fetch happens
(see §10.4 tracker and §12). If the source root advances between batch fetch and
enactment, the candidate is rejected with `UnsatisfiedRequires` and the collator retries.
The PVF-side `apply_messaging_proofs` is complete and correct — the gap is purely on the
collator pre-fetch side.

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
    /// V4: speculative messaging provides root and requires commitments.
    V4 {
        provides_root: Option<Hash>,           // top-level MMR root (sender side)
        requires: Vec<(ParaId, Hash)>,         // (source, expected_root) pairs (receiver side)
    },
}

pub struct ValidationResult {
    pub head_data: HeadData,
    pub new_validation_code: Option<ValidationCode>,
    pub upward_messages: UpwardMessages,
    pub horizontal_messages: HorizontalMessages,
    pub processed_downward_messages: u32,
    pub hrmp_watermark: RelayChainBlockNumber,
    /// Speculative messaging extension (v10+). Must be the LAST field.
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
4. In `polkadot/node/core/candidate-validation`: decode `speculative` field; extract `provides_root`/`requires` from `ValidationResultExtension::V4`; reconstruct `v10::CandidateCommitments` for v4 candidates.
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
        let commitments = v10::CandidateCommitments {
            head_data, upward_messages, horizontal_messages,
            new_validation_code, processed_downward_messages, hrmp_watermark,
            provides, requires,
        };
        ensure!(commitments.hash() == candidate_receipt.commitments_hash, ...);
    }
}
```

The corresponding implementation work:

1. add `v10::CandidateCommitments` and speculative types in `polkadot/primitives`
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
2. Wraps both block data and proofs in `ParachainBlockDataV4` and encodes
   the full struct as the PoV content.

**`ParachainBlockDataV4` wrapper type.** Instead of appending raw proof bytes after
the SCALE-encoded block data and parsing them with a manual cursor, the POC defines
a formal wrapper struct:

```rust
/// Extended block data for v4 speculative-messaging candidates.
#[derive(Encode, Decode)]
pub struct ParachainBlockDataV4 {
    pub inner: ParachainBlockData,
    pub late_block_proofs: Vec<LateBlockProof>,
}
```

The wire format is a single SCALE-encoded `ParachainBlockDataV4` — no manual
length-prefixed sections, no cursor arithmetic. The collator constructs it directly:

```rust
// In the collator's proposal path:
let block_data = build_block(...)?;  // existing PoV content
let pov_v4 = ParachainBlockDataV4 {
    inner: block_data,
    late_block_proofs,
};
let pov_bytes = pov_v4.encode();
```

**PVF decode.** The `validate_block` entry point decodes `ParachainBlockDataV4`
from the PoV bytes in one SCALE decode call. If the decode fails (wrong format,
truncated data, etc.), the candidate is invalid — same error model as any other
SCALE decode:

```rust
fn validate_block(params: ValidationParams) -> Result<ValidationResultV4, ValidationError> {
    // Single decode: block data + late block proofs in one call.
    let pov_v4 = ParachainBlockDataV4::decode(&mut &params.pov.block_data[..])
        .map_err(|_| ValidationError::InvalidBlockData)?;

    // 1. Execute the block with the inner block data
    let mut result = execute_block(&pov_v4.inner)?;

    // 2. Verify each late block proof and transform requires
    let mut transformed_requires = Vec::new();
    for proof in &pov_v4.late_block_proofs {
        let transformed = verify_and_transform(&result.requires, proof)?;
        transformed_requires.push(transformed);
    }
    // Keep non-transformed requires for sources without proofs
    for req in &result.requires {
        if !pov_v4.late_block_proofs.iter().any(|p| p.source == req.source) {
            transformed_requires.push(req.clone());
        }
    }

    result.requires = transformed_requires;
    Ok(result)
}
```

No SCALE cursor tricks, no manual offset tracking, no `parse_late_block_proofs`
function. The `ParachainBlockDataV4::decode` call either succeeds with both block
data and proofs, or fails cleanly.

**Version gating.** The collator and PVF must agree on the wire format. The
descriptor version (`candidate.descriptor.version()`) distinguishes the two cases:

- `V4` candidates: the PoV content is a SCALE-encoded `ParachainBlockDataV4`
- Pre-`V4` candidates: the PoV content is a plain `ParachainBlockData` (unchanged)

The collator branches on the candidate version when constructing the PoV:

```rust
if candidate_version >= V4 {
    let pov_v4 = ParachainBlockDataV4 { inner: block_data, late_block_proofs };
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
        let pov = ParachainBlockDataV4::decode(&mut &params.pov.block_data[..])
            .map_err(|_| ValidationError::InvalidBlockData)?;
        let result = execute_block(&pov.inner)?;
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
   and the order of manual decode calls. `ParachainBlockDataV4` gives the compiler
   a single struct to check, and SCALE derive handles the encoding/decoding.
3. **Cleaner version gating.** With the wrapper, the version distinction is "use
   this type or that type" rather than "decode this struct, then manually parse
   trailing bytes of unknown format."

No PVF host changes are needed — the PoV is already passed to the PVF as opaque
bytes via `params.pov.block_data`. The only difference is what type we decode from
those bytes.

The relay chain never sees the proofs and never verifies them. The entire
pipeline is: collator wraps proofs in `ParachainBlockDataV4` → PVF decodes and
verifies proofs in one SCALE decode → transforms requires → node-side validation
reconstructs commitments from the transformed result → relay chain matches
`expected_root` against `ProvidesRoots`. See §4.4 for what the relay chain does
*not* do, and §6.1 for commitments reconstruction.

**Runtime performs no late-block-proof verification.** All four steps of
`verify_and_transform` (old subtree Merkle proof, new subtree Merkle proof, MMR
extension proof, root transformation) happen exclusively inside the PVF's
`apply_messaging_proofs`. The relay-chain runtime (`ingest_verified_messages`)
never sees `LateBlockProof` data and performs no verification of it. Adding a
`proof.old_subtree_root == state.last_seen_subtree_root` equality check to the
runtime would be a possible future defense-in-depth improvement but is not
required for correctness — the PVF is the consensus-critical path.

```rust
fn verify_and_transform(
    block_requires: &[RequiresCommitment],
    proof: &LateBlockProof,
) -> Result<RequiresCommitment, ValidationError> {
    // 1. Verify old subtree was in the old provides root
    let old_leaf = (proof.source, proof.old_subtree_root).encode();
    let old_leaf_hash = keccak_256(&old_leaf);
    verify_merkle_proof(
        proof.old_provides_root,
        &proof.old_subtree_proof,
        old_leaf_hash,
    )?;

    // 2. Verify new subtree is in the current root
    let new_leaf = (proof.source, proof.new_subtree_root).encode();
    let new_leaf_hash = keccak_256(&new_leaf);
    verify_merkle_proof(
        proof.new_provides_root,
        &proof.new_subtree_proof,
        new_leaf_hash,
    )?;

    // 3. Subtrees must be identical or old must be a valid prefix
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

    // 4. Return transformed commitment — references the current root
    Ok(RequiresCommitment {
        source: proof.source,
        expected_root: proof.new_provides_root,
    })
}
```

**How the collator pre-transforms commitments.** The collator's precheck
produces the same transformed root. When building commitments (§5.3), the
collator uses the transformed root directly — `ConsumedSourcesThisBlock` still
stores the original root from batch processing, but the collator overrides it
with the proof-verified root when constructing `CandidateCommitments`. The PVF
confirms this override independently.

**What the relay chain sees.** No change from section 4.2. The relay chain always
matches `RequiresCommitment.expected_root` against `ProvidesRoots[source]`. The
transformation happens before commitments are finalized, so the relay chain never
knows whether a proof was needed.

**Proof size.** For a sender with D destinations and m messages to this receiver:
the two top-level Merkle proofs are O(log D) each (~14 hashes for 100
destinations), and the subtree extension is O(log m) (~10 hashes for 1000
messages). Total: well under 2 KB in typical cases. The PoV size budget should
reserve a small allowance for these proofs (e.g., 50 KB).

**Serving extension proofs.** The provider serves `LateBlockProof` data via the
same HTTP endpoint (section 7.3), returning proofs alongside or instead of
batches when the source's provides_root has advanced beyond the receiver's
last-seen root.

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

`OutboxQuery` is an `async_trait` that abstracts over two concrete implementations:

- **`DirectOutboxClient<Block, Client>`** — holds an `Arc<Client>` and calls
  `SpeculativeOutboxApi` runtime APIs directly, in-process. Suitable for zombienet
  setups where both sender and receiver parachains share a single process tree.
  Constructed via `DirectOutboxClient::new(arc_client)`, which returns
  `Arc<dyn OutboxQuery>`.

- **`RpcOutboxClient`** — connects to a remote node via JSON-RPC WebSocket
  (`jsonrpsee` ws-client). All outbox queries are translated to `state_call` RPC
  requests, e.g. `state_call("SpeculativeOutboxApi_compute_provides_root", ...)`.
  Supports two separate penpal binaries (sender and receiver running as independent
  OS processes). Constructed via `RpcOutboxClient::connect(url).await`.

This abstraction lives in:
- `cumulus/client/consensus/aura/src/collators/outbox_client.rs` — `OutboxQuery`
  trait, `DirectOutboxClient`, `RpcOutboxClient`, `build_message_batch_from_query`.
- `cumulus/client/consensus/aura/src/collators/speculative_ingress.rs` —
  `SpeculativeMessageSources` (now generic-free) and `fetch_ingress_for_block()`
  (fully async): queries relay chain for the current provides root, queries each
  configured sender via `OutboxQuery`, assembles `MessageBatch`es.
- `cumulus/pallets/speculative-inbox/src/client.rs` — `build_message_batch()`
  lower-level batch construction helpers (receiver reads IncomingState from its
  own runtime API).

The design doc originally described an HTTP relayer/provider model. That model is
deferred to production hardening (§12). The `OutboxQuery` trait is the correct
abstraction layer for both: a future HTTP provider client would be a third
`OutboxQuery` implementation without touching any of the collator logic.

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
    fn provides_root() -> Option<Hash>;
    fn destination_state(dest: ParaId) -> Option<(Hash, u64)>;
    /// Read payload bytes from on-chain storage for a destination starting at
    /// `from_position`. Returns up to `max_messages` entries.
    fn outbound_messages(dest: ParaId, from_position: u64, max_messages: u32) -> Vec<(u64, Vec<u8>)>;
    fn subtree_inclusion_proof(dest: ParaId, subtree_root: Hash) -> Option<Vec<Hash>>;
    /// Return an MMR extension proof proving that `old_subtree_root` at
    /// `old_subtree_size` is a valid prefix of the current subtree for
    /// this destination.
    fn mmr_extension_proof(
        dest: ParaId,
        old_subtree_root: Hash,
        old_subtree_size: u64,
    ) -> Option<MMRExtensionProof>;
}
```

**Payload bytes are read from on-chain storage.** The outbox pallet stores full
payload bytes in `OutgoingMessages` (see §5.1). These runtime APIs can be
queried by any RPC client that knows the source chain's endpoint.

**The provider as a convenience layer.** A separate provider process monitors the
sender chain, queries these runtime APIs, and caches the results as pre-assembled
`MessageBatch` structs. For each destination that received messages in a source
block, the provider reads `destination_state(dest)` for `(subtree_root,
leaf_count)`, reads `subtree_inclusion_proof(dest, subtree_root)` for the Merkle
proof, reads `outbound_messages(dest, last_known_position, max)` for payload
bytes, reads `provides_root()`, and assembles the `MessageBatch`.

The provider retains batches in a bounded in-memory cache keyed by
`(destination_para_id, provides_root)` with a retention window of the last N
finalized source blocks (e.g., N = 64) or last T minutes (e.g., T = 10). The
cache is purely in-memory for the POC — the source chain's runtime state is the
canonical store.

This caching layer is not required — the receiver collator could query the
sender's runtime APIs directly via RPC (§7.4). The provider amortizes work
across multiple receivers, provides a simpler HTTP interface (§7.3), and avoids
requiring every receiver collator to maintain an RPC connection to every source
chain. For the POC, one or more known providers per source is the simplest
operational model.

### 7.3 Transport: HTTP API

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
iterates over configured source parachains, reads the local
`IncomingState[source].last_seen_provides_root`, queries each known provider with
`since_provides_root`, and collects all returned batches. Timeouts (e.g., 2
seconds per provider) prevent hanging.

**Precheck.** Each fetched batch goes through the collator-local precheck
described in section 5.2: verify subtree inclusion proof, verify message
continuity, reconstruct local subtree.

The collator then compares the batch's `provides_root` against the current
`ProvidesRoots[source]` by querying its relay chain node via RPC (the
collator runs alongside a relay chain node and knows the storage key for the
`speculative_messaging` pallet's `ProvidesRoots` map). This RPC read is a
**best-effort precheck**, not a consensus path:

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
injected into `InherentData` under `SPECULATIVE_INGRESS_IDENTIFIER`. Prechecked
`LateBlockProof` data and the block data are wrapped in `ParachainBlockDataV4`
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
- `polkadot/primitives/src/v10/mod.rs`
- `polkadot/primitives/src/lib.rs`
- `polkadot/primitives/test-helpers/src/lib.rs`

Add `ProvidesCommitment`, `RequiresCommitment`, `MessageBatch`, `OutgoingMessage`,
`SpeculativeIngress`, and v10 `CandidateCommitments`. Extend descriptor-version
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

Decode the extended validation result for v4 candidates. Reconstruct v10
`CandidateCommitments` from returned outputs. Keep pre-v4 candidates on the
legacy path. Continue hash-checking against the candidate receipt.

### 10.7 Step 7: Late Block Proofs (PVF + Provider)

**Files:**
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`
- `polkadot/parachain/src/primitives.rs` (extend `ValidationResult`)
- `polkadot/parachain/src/wasm_api.rs` (PoV parsing)
- provider/relayer process (same as step 9)

Add `LateBlockProof` and `MMRExtensionProof` types to `v10` primitives. Implement
PoV-based proof verification: collator fetches and prechecks proofs, uses
transformed root in candidate commitments, wraps proofs and block data in
`ParachainBlockDataV4`. PVF decodes `ParachainBlockDataV4` from the PoV during
`validate_block`, verifies proofs, and transforms requires. Collator precheck
and PVF verification use the same logic; mismatches cause commitments hash
mismatch (candidate rejected).

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

Legend: ✅ done · 🔶 partial · ❌ not started

| Step | Description | Status | Notes |
|------|-------------|--------|-------|
| 10.1 | Primitives & version gating | ✅ | `v10` types, `MMRExtensionProof.old_leaf_count`, `CandidateDescriptorV2::new_v4()` version-byte approach. V4 struct family removed. |
| 10.2 | Receiver runtime ingress path | ✅ | `pallet-speculative-inbox` complete: `IncomingState`, `ConsumedSourcesThisBlock`, `ingest_verified_messages`, `requires_commitments`. 11/11 tests pass. |
| 10.3 | Sender runtime outbox path | ✅ | `pallet-speculative-outbox` complete: peaks-only MMR, `HistoricalProvidesRoots`, `HistoricalSubtreeState`, `generate_late_block_proof`. |
| 10.4 | Collator-side inherent injection & commitment assembly | 🔶 | Slot-based path complete: `provides`/`requires` read from runtime via `SpeculativeInboxApi` after block execution in `build_collation_for_core`, flowed through `CollatorMessage` to `build_multi_block_collation` in `service.rs`. `ParachainBlockDataV4` wrapping works. Remaining gap: single-block path (`build_collation` in basic/lookahead collators) still passes `None`/empty. Collator-side LBP pre-fetch deferred (see §12). |
| 10.5 | PVF / Wasm validation ABI | ✅ | `ValidationResultExtension::V4` via `TrailingOption<ValidationResultExtension>` (not a separate `ValidationResultV4` struct — backward-compatible trailing field on existing `ValidationResult`). `apply_messaging_proofs`, `verify_mmr_extension` (connecting_nodes replay). `ParachainBlockDataV4` decoded in `validate_block`. |
| 10.6 | Node-side candidate validation | ✅ | v4 `CandidateCommitments` reconstruction from `ValidationResultExtension::V4` and hash check implemented in `candidate-validation/src/lib.rs` lines 1310–1337. |
| 10.7 | Late block proofs (PVF side) | ✅ | PVF-side proof verification complete. Collator-side pre-fetch not implemented (see §10.4). |
| 10.8 | Relay-chain enactment rules | ✅ | `ProvidesRoots` storage, `requires_satisfied`, `update_provides_root`, enactment-time check (lines 582–588, 956–964) and `UnsatisfiedRequires` error all wired in `inclusion/mod.rs`. |
| 10.9 | Off-chain networking | ✅ | `OutboxQuery` async trait with `RpcOutboxClient` (JSON-RPC WebSocket). `SpeculativeMessageSources` is generic-free. `fetch_ingress_for_block` is fully async. `--speculative-sender <PARA_ID>=<WS_URL>` CLI arg added to omni-node. At startup the node async-connects to each configured sender and populates `SpeculativeMessageSources`; connection failures are logged and skipped gracefully. Slot-based ingress injection path fully wired. Note: basic/lookahead collator paths inject ingress but still hardcode `provides=None, requires=[]` for commitment assembly (see §10.4). |
| 10.10 | POC runtime & test milestones | 🔶 | Milestones 1–5 ✅. Zombienet config added: `cumulus/zombienet/examples/speculative_messaging_poc.toml` — two Penpal instances (para 2000 sender, para 2001 receiver) on Rococo-local, receiver started with `--speculative-sender 2000=ws://127.0.0.1:9955`. Milestones 6–8 (live network verification) pending actual run. |

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
- **`SpeculativeOutboxApi` / `SpeculativeInboxApi` required for slot-based collation** —
  Both APIs are bounds on `StartSlotBasedAuraConsensus` (and `new_aura_node_spec` when
  called with the slot-based path). They are NOT in `NodeRuntimeApi`, so Asset Hub,
  Bridge Hub, and other parachains using the lookahead collator are unaffected. Any
  parachain that wants to use `--authoring slot-based` must implement both APIs (stub
  impls returning `None`/empty are sufficient for non-speculative chains). In production
  the slot-based path should use `ApiExt::has_api` checks to make speculative messaging
  opt-in at runtime rather than required at compile time.
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
- **Collator-side Late Block Proof collection not yet implemented.** The POC
  omits the pre-flight LBP fetch in the collator (`cumulus/client/consensus/aura`).
  The code was stubbed but had unresolvable compile issues:
  `CollatorServiceInterface` does not expose the para chain client (needed to call
  `requires_commitments` after block execution), `generate_late_block_proof` trait
  bounds could not be added to generic collator `run` functions without breaking all
  non-speculative runtimes, and `relay_client.provides_root` argument order was
  wrong. Without collator-side LBP collection, the candidate is submitted with the
  old `requires` root; if the source root has advanced the relay chain rejects with
  `UnsatisfiedRequires` and the collator retries (§7.4 resubmission loop). The
  consensus-critical path — PVF-side `apply_messaging_proofs` in
  `validate_block/implementation.rs` — is fully intact. Restoring the collator-side
  optimization requires either: (a) exposing a para client accessor on
  `CollatorServiceInterface`, or (b) reading provides/requires before calling
  `build_collation` via a new field on `ParachainCandidate`.

### Optional Future Directions
- Super-chain / intra-block messaging.
- Relaxed or unordered delivery semantics.
- Enhanced pruning and garbage collection strategies.

---

## 13. Related Documents

- [speculative-messaging-design.md](speculative-messaging-design.md) — Full high-level design including Late Block Proofs, trust domains, super chains, and LLv2 integration.
- [xcmp-mmd-minimal-poc.md](xcmp-mmd-minimal-poc.md) — Superseded earlier POC using BEEFY-anchored proofs. Retained for historical reference.
