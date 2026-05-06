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
backing pipeline, not just core-on-demand chains. **Guaranteed eventual delivery**
is a hard requirement for the full design but is deferred past Phase 1 (see
section 12).

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
                                                a) same-block enacted provides
                                                b) latest persisted provides root
                                              - Update ProvidesRoots only after actual enactment
```

### 1.2 Detailed Walkthrough

**Step 1 — Sender block execution.** The source parachain collator builds a block
normally. During runtime execution, outbound sibling-parachain XCM is produced
through the existing path. A speculative outbox wrapper records the payloads into
per-destination MMR/subtree state, and the sender's cumulative top-level
`provides` root becomes derivable from the resulting runtime state. See section 5.1.

**Step 2 — Source-side batch/proof retention.** After the sender block exists, a
relayer/provider process retains a bounded recent history of: the sender
`provides_root`, destination subtree roots, subtree inclusion proofs, and ordered
messages with positions. See section 7.2.

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
time, the relay chain checks every `RequiresCommitment` against same-block
enacted provides or the latest persisted provides root, then updates
`ProvidesRoots[source]` on success. See section 4.2.

The detailed implementation order, including specific files and modules for each step, is in section 10.

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

**Settled decision — new concrete version family.** Introduce a new
descriptor/receipt version rather than evolving the existing V2 struct.
Overloading the V2 reserved bytes with speculative fields risks
backward-compatibility bugs where a non-speculative node parsing a speculative
candidate silently misinterprets fields. Legacy candidates continue on the
unchanged v9 path; v4 candidates use v10 types.

Concretely, the intended behavior:

- legacy candidates keep their existing commitments layout and validation/reconstruction path
- v4 candidates use the extended commitments layout with speculative messaging fields
- relay-chain inclusion only enforces requires/provides matching for v4+ candidates
- node-side candidate validation reconstructs commitments according to the candidate descriptor version

This means the upgrade is additive: pre-v4 parachains remain valid, v4 parachains
opt into new semantics, and both formats coexist during migration.

```rust
// In v10/mod.rs:
pub struct CandidateDescriptorV4<N = BlockNumber> {
    pub para_id: ParaId,
    pub relay_parent: Hash,
    // Phase 1 speculative messaging does not require LLv2 fields. If the
    // implementation wants to stay strictly decoupled from LLv2, these can be
    // omitted from the initial V4. If the team intentionally wants one shared
    // descriptor upgrade path, they can be included as optional fields:
    pub scheduling_parent: Option<Hash>,
    pub scheduling_session_index: Option<SessionIndex>,
    pub collator: CollatorId,
    pub persisted_validation_data_hash: Hash,
    pub pov_hash: Hash,
    pub erasure_root: Hash,
    pub para_head: Hash,
    pub validation_code_hash: ValidationCodeHash,
    pub signature: CollatorSignature,
    pub core_index: CoreIndex,
    pub session_index: SessionIndex,
}

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
    /// Late block proofs for sources whose provides_root has advanced
    /// since the receiver block was built. At most one proof per source.
    pub late_block_proofs: Vec<LateBlockProof>,
}
```

For Phase 1, `SpeculativeIngress.batches` follows simple canonical selection
rules: batches are grouped logically per `source`, for a given source they appear
oldest-to-newest, and duplicate or overlapping batches for the same source in a
single block should be rejected by both collator precheck and runtime
re-verification. `late_block_proofs` must have at most one entry per source.

Phase 1 uses a single inherent-like dispatch, following the same pattern as
`ParachainSystem::set_validation_data`: a node-local component fetches batches
off-chain, `ProvideInherent` turns them into a block-body call, the runtime
re-verifies deterministically, and `validate_block` replays the same call.

```rust
SpeculativeInbox::ingest_verified_messages { ingress: SpeculativeIngress }
```

The wiring:

```rust
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
`set_validation_data`, the speculative inbox pallet should be ordered in the
runtime such that its inherent executes **after** `ParachainSystem`'s inherent.

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
what's now current on the relay chain, the receiver runtime verifies a proof
demonstrating that the messages it consumed are still valid under the current
root. The proof is carried in the block body via `SpeculativeIngress`, alongside
the message batches, and verified during `ingest_verified_messages`. The
verified current root replaces the old root in `ConsumedSourcesThisBlock`, so
the relay chain sees the correct `RequiresCommitment`.

```rust
/// Carried in `SpeculativeIngress.late_block_proofs` when the receiver block
/// was built against an older source root than what's persisted in ProvidesRoots.
#[derive(Clone, Encode, Decode, Debug)]
pub struct LateBlockProof {
    /// The source parachain this proof covers.
    pub source: ParaId,

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

### 4.1 New Module: `speculative_messaging.rs`

```
polkadot/runtime/parachains/src/speculative_messaging.rs  ← NEW FILE
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
- requires/provides dependency satisfaction must be defined against roots that are actually included in relay-visible state

So the minimal POC uses a two-stage relay-chain treatment:

1. `process_candidates()` remains the place where v4 candidates are admitted into pending availability alongside their `provides` / `requires` fields.
2. The actual requires/provides satisfaction check is performed in the path that determines which pending candidates are enacted in the current relay block.

```rust
// Stage 1: backing / pending-availability admission
pub(crate) fn process_candidates<GV>(...) -> Result<..., Error> {
    for (para_id, backed_list) in candidates.iter() {
        for (candidate, core_index) in backed_list {
            // ... existing candidate checks ...
            // Store the v4 commitments unchanged in PendingAvailability.
            // No same-block requires satisfaction decision is finalized here.
        }
    }
}

// Stage 2: inclusion / enactment in the current relay block
fn enact_pending_candidates_for_current_block(...) {
    let mut enacted_provides_in_block: BTreeMap<ParaId, BTreeSet<Hash>> = BTreeMap::new();

    for candidate in candidates_being_enacted_now {
        if candidate.descriptor.version() >= V4 {
            for req in &candidate.commitments.requires {
                let satisfied_same_block = enacted_provides_in_block
                    .get(&req.source)
                    .map_or(false, |roots| roots.contains(&req.expected_root));

                let satisfied_persisted = SpeculativeMessaging::<T>::provides_root(&req.source)
                    .map_or(false, |root| root == req.expected_root);

                ensure!(satisfied_same_block || satisfied_persisted, Error::<T>::UnsatisfiedRequires);
            }
        }

        Self::enact_candidate(...);

        if candidate.descriptor.version() >= V4 {
            if let Some(ref p) = candidate.commitments.provides {
                enacted_provides_in_block
                    .entry(candidate.para_id())
                    .or_default()
                    .insert(p.root);

                SpeculativeMessaging::<T>::update_provides_root(candidate.para_id(), p.root);
            }
        }
    }
}
```

The relay chain is not asked to verify message proofs again. It only needs to
inspect the already-validated `provides` / `requires` fields, check dependency
satisfaction, and persist the newest provides root. This is a relay-runtime
inclusion rule change, not a new protocol stage.

For the minimal POC, the "latest persisted root OR same-block root" rule is
sufficient under the happy-path timing assumption. Robust handling of delayed
destinations is deferred to the Late Block Proofs phase.

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
  inclusion-time check: `RequiresCommitment.expected_root` must match either a
  same-block enacted provides root or the latest persisted root. That check is a
  hash comparison, not a cryptographic verification.

In short: all cryptographic work lives in the PVF; the relay chain only adds
hash-equality checks on already-validated commitment fields.

---

## 5. Parachain Runtime Changes

### 5.1 Outgoing Message MMR (Sender Side)

Pattern: **wrap the runtime's configured `OutboundXcmpMessageSource`** (typically
`XcmpQueue`) by implementing the `XcmpMessageSource` trait such that each
outbound message is both recorded in the speculative outbox and forwarded to the
inner source. The wrapping type then replaces `XcmpQueue` as the `type
OutboundXcmpMessageSource` in the parachain runtime's `ParachainSystem` config.
This is the same interception-point pattern that parachain-system already uses to
drain outbound HRMP messages in `on_finalize` (see
`cumulus/pallets/parachain-system/src/lib.rs` line ~409).

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
    /// Leaf count for THIS destination's subtree MMR.
    pub leaf_count: u64,
    pub root: H256,
    /// Nodes stored for proof generation (peaks + internal nodes).
    pub nodes: BTreeMap<u64, H256>,
}
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
        .map(|(dest, state)| (dest, state.root))
        .collect();

    if roots.is_empty() {
        return None;  // no speculative outbox state exists yet
    }

    roots.sort_by_key(|(id, _)| *id);
    let leaves: Vec<H256> = roots.into_iter().map(|(dest, root)| {
        H256::from(sp_io::hashing::keccak_256(&(dest, root).encode()))
    }).collect();
    Some(ProvidesCommitment { root: compute_merkle_root(&leaves) })
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
    pub last_seen_provides_root: H256,
    /// The source's subtree root we last synced to.
    pub last_seen_subtree_root: H256,
    /// Local copy of the subtree MMR (only messages sent to us).
    pub local_subtree: MMRState,
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
                ensure!(msg.position == state.last_processed + 1, Error::<T>::NonConsecutiveMessage);
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
`ConcatenatedVersionedXcm` throughout is the simplest consistent choice.

### 5.3 Producing Commitments

After block execution, the collator reads the provides/requires from runtime
storage and populates `CandidateCommitments`. Phase 1 enforces at most one
`RequiresCommitment` per source parachain per block.

```rust
pub fn get_requires_commitments() -> Vec<RequiresCommitment> {
    let mut consumed = ConsumedSourcesThisBlock::<T>::get();
    consumed.sort_by_key(|(source, _)| *source);
    consumed.dedup_by_key(|(source, _)| *source);

    consumed.into_iter().map(|(source, provides_root)| RequiresCommitment {
        source,
        expected_root: provides_root,
    }).collect()
}

impl Collator {
    async fn build_block_commitments(&self, block: &Block) -> v10::CandidateCommitments {
        let provides = self.runtime_api
            .compute_provides_root(block.hash())
            .await?;

        let requires = self.runtime_api
            .get_requires_commitments(block.hash())
            .await?;

        v10::CandidateCommitments {
            upward_messages: block.upward_messages(),
            horizontal_messages: block.horizontal_messages(),
            new_validation_code: block.new_code(),
            head_data: block.head_data(),
            processed_downward_messages: block.processed_dmp(),
            hrmp_watermark: block.hrmp_watermark(),
            provides,
            requires,
        }
    }
}
```

---

## 6. PVF Validation Entry Point

Phase 1 requires a **small validation ABI extension**. The current parachain
validation ABI returns a `ValidationResult` containing only legacy fields.
Speculative messaging adds:

- `provides: Option<ProvidesCommitment>`
- `requires: Vec<RequiresCommitment>`

The wasm entrypoint returns **one upgraded validation-result struct**.
Non-speculative candidates on upgraded runtimes return `provides: None` and
`requires: vec![]`. Version-gating happens on the node side — candidate
validation branches on descriptor version to know whether to expect populated
speculative fields. The relay-chain runtime API (`check_validation_outputs`)
must evolve to accept the extended type from the start, ignoring optional
speculative fields for pre-speculative candidates.

**Current-codebase embedding:**

1. In `polkadot/parachain/src/primitives.rs`, introduce an extended validation result shape.
2. In `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`, after block execution, read speculative outputs (`provides`, `requires`) from runtime state and include them in the returned validation result.
3. In `polkadot/parachain/src/wasm_api.rs`, return that extended result from the wasm entrypoint.
4. In `polkadot/node/core/candidate-validation`, decode the extended result and reconstruct `v10::CandidateCommitments` for v4 candidates.
5. Keep older descriptor versions on the legacy path.
6. Update relay-chain runtime-API entrypoints that still accept the legacy unversioned `CandidateCommitments` (`ParachainHost::check_validation_outputs` and `check_validation_outputs_for_runtime_api(...)`).

```rust
/// Extended wasm validation result for v4 speculative-messaging candidates.
pub struct ValidationResultV4 {
    pub head_data: HeadData,
    pub new_validation_code: Option<ValidationCode>,
    pub upward_messages: UpwardMessages,
    pub horizontal_messages: HorizontalMessages,
    pub processed_downward_messages: u32,
    pub hrmp_watermark: RelayChainBlockNumber,
    pub provides: Option<ProvidesCommitment>,
    pub requires: Vec<RequiresCommitment>,
}

fn validate_block(params: ValidationParams) -> Result<ValidationResultV4, ValidationError> {
    let result = execute_block_and_collect_outputs(&params)?;
    Ok(ValidationResultV4 {
        head_data: result.head_data,
        new_validation_code: result.new_validation_code,
        upward_messages: result.upward_messages,
        horizontal_messages: result.horizontal_messages,
        processed_downward_messages: result.processed_downward_messages,
        hrmp_watermark: result.hrmp_watermark,
        provides: result.provides,
        requires: result.requires,
    })
}
```

The wasm PVF does **not** read candidate commitments as an input. It executes the
block, derives the full validation outputs, and returns them. The node-side
candidate-validation pipeline then reconstructs `CandidateCommitments` from those
returned outputs and checks the commitments hash.

### 6.1 Candidate Commitments Reconstruction

Node-side candidate validation already reconstructs commitments inline after PVF
execution. For the POC, update that logic to branch on candidate descriptor
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

### 6.2 Late Block Proofs (Inherent Approach)

When a receiver block was built against an older source root than what's now in
`ProvidesRoots`, the receiver collator includes a `LateBlockProof` in
`SpeculativeIngress.late_block_proofs` alongside the message batches. The proof
is verified during `ingest_verified_messages` — the same runtime call that
processes batches. The verified current root replaces the old root in
`ConsumedSourcesThisBlock`, so `get_requires_commitments()` returns the correct
`RequiresCommitment` referencing the current root. No PVF changes are needed:
`validate_block` deterministically replays `ingest_verified_messages`, so
validators independently verify the same proofs and produce the same transformed
commitments.

**Design choice — inherent vs PoV.** The high-level design specifies a PoV-based
approach where the PVF verifies proofs and transforms commitments in a separate
pre-block stage (matching LLv2's scheduling-parent pattern). The POC uses the
inherent approach instead: the runtime verifies proofs during normal block
execution. This avoids building a new PVF input channel solely for late block
proofs, at the cost of a small deviation from the source design. The verification
logic is identical; moving to the PoV approach later is a mechanical refactor
once LLv2's separate-PVF-inputs infrastructure is in place.

**When this triggers.** The collator detects the mismatch before block proposal:
it reads `ProvidesRoots[source]` from the relay parent's state and compares it to
the `provides_root` of the fetched batch. If they differ, the collator fetches a
`LateBlockProof` from the source/provider and includes it in
`SpeculativeIngress.late_block_proofs`.

**Verification.** During `ingest_verified_messages`, the runtime first verifies
all late block proofs and builds a map of `source -> new_provides_root`. Then
when processing batches, it uses the transformed root instead of the batch's
original `provides_root`:

```rust
pub fn ingest_verified_messages(
    origin: OriginFor<T>,
    ingress: SpeculativeIngress,
) -> DispatchResult {
    ensure_none(origin)?;

    // ── Pre-pass: verify late block proofs ──
    let mut transformed_roots: BTreeMap<ParaId, Hash> = BTreeMap::new();
    for proof in &ingress.late_block_proofs {
        // 1. Verify old subtree was in the old provides root
        let old_leaf = (T::SelfParaId::get(), proof.old_subtree_root).encode();
        let old_leaf_hash = sp_io::hashing::keccak_256(&old_leaf);
        verify_merkle_proof(
            proof.old_provides_root,
            &proof.old_subtree_proof,
            old_leaf_hash,
        ).map_err(|_| Error::<T>::InvalidLateBlockProof)?;

        // 2. Verify new subtree is in the current root
        let new_leaf = (T::SelfParaId::get(), proof.new_subtree_root).encode();
        let new_leaf_hash = sp_io::hashing::keccak_256(&new_leaf);
        verify_merkle_proof(
            proof.new_provides_root,
            &proof.new_subtree_proof,
            new_leaf_hash,
        ).map_err(|_| Error::<T>::InvalidLateBlockProof)?;

        // 3. Subtrees must be identical or old must be a valid prefix
        if proof.old_subtree_root != proof.new_subtree_root {
            let ext = proof.subtree_extension
                .as_ref()
                .ok_or(Error::<T>::SubtreeChangedWithoutProof)?;
            verify_mmr_extension(proof.old_subtree_root, proof.new_subtree_root, ext)
                .map_err(|_| Error::<T>::InvalidLateBlockProof)?;
        }

        // 4. Record the transformed root for this source
        ensure!(
            !transformed_roots.contains_key(&proof.source),
            Error::<T>::DuplicateLateBlockProof,
        );
        transformed_roots.insert(proof.source, proof.new_provides_root);
    }

    // ── Main pass: process batches (section 5.2) ──
    let mut consumed = Vec::new();
    for batch in ingress.batches {
        // ... subtree proof, message continuity, subtree root checks ...

        // Use the transformed root if a late block proof was verified for this source
        let effective_root = transformed_roots
            .get(&batch.source)
            .unwrap_or(&batch.provides_root);

        // Phase 1 invariant: one distinct top-level provides root per source per block
        // (checked against effective_root, not batch.provides_root)
        //
        // ... MultipleRootsPerSourceInOneBlock check, dispatch ...

        consumed.push((batch.source, *effective_root));
    }

    ConsumedSourcesThisBlock::<T>::put(consumed);
    Ok(())
}
```

The `LateBlockProof` carries `old_provides_root` so the pre-pass is
self-contained — it does not need to peek at batches to resolve the old root.

**What the relay chain sees.** No change. `get_requires_commitments()` returns
the effective (transformed) root, and the relay chain matches it against
`ProvidesRoots` exactly as in section 4.2. The relay chain never knows whether
a proof was needed.

**Proof size.** For a sender with D destinations and m messages to this receiver:
the two top-level Merkle proofs are O(log D) each (~14 hashes for 100
destinations), and the subtree extension is O(log m) (~10 hashes for 1000
messages). Total: well under 2 KB in typical cases.

**Serving extension proofs.** The provider serves `LateBlockProof` data via the
same HTTP endpoint (section 7.3), returning proofs alongside or instead of
batches when the cursor root differs from the current root.

---

## 7. Off-Chain Networking

### 7.1 Model

The POC uses a **relayer/provider** model rather than native collator-to-collator P2P:

- One or more provider processes watch source chain blocks and serve `MessageBatch` data to destination collators.
- Destination collators fetch batches from known providers before block proposal, precheck them locally, and encode accepted batches into `SpeculativeIngress`.
- The transport is a data-fetch path, not a consensus path. Consensus depends only on `SpeculativeIngress` being embedded in the block body and re-verified deterministically during PVF execution.

The relay-chain interaction is **pull-based**: destination collators ask a
provider for batches they want to import. If no provider answers, the destination
simply skips speculative ingress for that source in this block and can fall back
to HRMP.

### 7.2 Sender-Side: Batch Construction and Retention

The sender runtime exposes APIs that the provider queries after block
finalization. These are not consensus-critical but must return correct data for
the receiver to accept the resulting batches.

```rust
#[runtime_api]
pub trait SpeculativeOutboxApi {
    fn provides_root() -> Option<Hash>;
    fn destination_state(dest: ParaId) -> Option<(Hash, u64)>;
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

For each destination that received messages in a source block, the provider:
reads `destination_state(dest)` for `(subtree_root, leaf_count)`, reads
`subtree_inclusion_proof(dest, subtree_root)` for the Merkle proof, reads
`outbound_messages(dest, last_known_position, max)` for ordered messages, reads
`provides_root()`, and assembles the `MessageBatch`.

The provider retains batches in a bounded in-memory cache keyed by
`(destination_para_id, provides_root)` with a retention window of the last N
finalized source blocks (e.g., N = 64) or last T minutes (e.g., T = 10). The
cache is purely in-memory for the POC — the source chain's runtime state is the
canonical store.

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
continuity, reconstruct local subtree. Batches that fail precheck are discarded.

**Selection.** Batches are ordered by source priority (configurable) then by age
(oldest first). The collator selects greedily until block weight or size limits
are met. At most one distinct `provides_root` per source per block.

**Injection.** Selected batches are encoded into `SpeculativeIngress` and
injected into `InherentData` under `SPECULATIVE_INGRESS_IDENTIFIER`.

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
falls behind by more than the retention window cannot fetch the missing batches.
The receiver's precheck rejects batches where `source_relay_parent_number` is too
far behind the current relay parent. Robust catch-up requires Late Block Proofs.

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
`mmr_leaf_index` or message hash).

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
the worst case, and let each call consume what it needs. The two calls are
independent.

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
`get_requires_commitments()` runtime API.

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

### 10.7 Step 7: Late Block Proofs (Runtime + Provider)

**Files:**
- `cumulus/pallets/speculative-inbox/src/lib.rs` (or equivalent)
- provider/relayer process (same as step 9)

Add `LateBlockProof` and `MMRExtensionProof` types to `v10` primitives. Implement
late block proof verification as a pre-pass in `ingest_verified_messages`: verify
old subtree was in the expected root, verify new subtree is in the current root,
verify subtree extension (if any), store the transformed root for use during
batch processing. Because verification runs during normal block execution, no PVF
changes are needed — `validate_block` deterministically replays the same path.

### 10.8 Step 8: Relay-Chain Runtime Enactment Rules

**Files:**
- new `polkadot/runtime/parachains/src/speculative_messaging.rs`
- `polkadot/runtime/parachains/src/inclusion/mod.rs`

Add `ProvidesRoots` storage. Keep `process_candidates()` for backing admission.
Extend the enactment path to track same-block enacted provides and check v4
`RequiresCommitment` against same-block + persisted roots. Add
`UnsatisfiedRequires` error.

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
5. relay-chain enactment accepts satisfied dependencies (both same-root and late-block-proof cases) and rejects unsatisfied ones
6. collator networking can fetch, precheck, and inject a recent batch end-to-end
7. late block proof: receiver can consume messages from a source that has advanced past the root the receiver built against

---

## 11. What's NOT In This POC

- **Speculative (acknowledged) delivery mode**: requires Low-Latency v2's collator acknowledgement signatures, which are not yet implemented in the codebase. The receiver cannot optimistically build on an un-included sender block without a signed canonicality commitment from the sender's collators.
- **Super-chain (intra-block) delivery mode**: requires a shared collator set infrastructure where one collator produces blocks for multiple parachains atomically in the same slot. This infrastructure does not exist yet in the codebase.
- **Trust domains**: all collators trust each other; no cross-domain fallback
- **Low-Latency v2 integration**: no acknowledgement signatures, no scheduling parent
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

### Optional Future Directions
- Super-chain / intra-block messaging.
- Relaxed or unordered delivery semantics.
- Enhanced pruning and garbage collection strategies.

---

## 13. Related Documents

- [speculative-messaging-design.md](speculative-messaging-design.md) — Full high-level design including Late Block Proofs, trust domains, super chains, and LLv2 integration.
- [xcmp-mmd-minimal-poc.md](xcmp-mmd-minimal-poc.md) — Superseded earlier POC using BEEFY-anchored proofs. Retained for historical reference.
