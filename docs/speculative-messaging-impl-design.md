# Speculative Messaging — Minimal POC Implementation Design

Based on [speculative-messaging-design.md](speculative-messaging-design.md).

This document is the **source of truth** for the minimal speculative-messaging
POC on top of the current codebase.

It covers the "happy path" without Late Block Proofs. Assumes: sender and
receiver blocks are both included in the same or adjacent relay chain blocks,
so provides roots are available at matching time.

**Phase 1 scope**: Inclusion-based messaging. This is "HRMP with off-chain message
passing" — removes message storage from relay chain state, but latency remains
~6–12s (1–2 relay blocks for inclusion). Low-latency requires Phase 2 (Late Block
Proofs) and Phase 3 (acknowledgements from Low-Latency v2).

This should also be understood as the **first implementation slice of the
broader offchain-XCMP replacement direction**, not a separate competing design.
Speculative messaging is the more general commitment-driven model; the Phase 1
POC implements its conservative inclusion-based path first.

This minimal POC therefore assumes a relatively timely destination block
production / inclusion path. For destinations that are **core-on-demand** or
otherwise produce blocks only sporadically, the happy-path assumption breaks
down more often: by the time the destination candidate is included, the source
chain's current `provides` root may already have advanced beyond the old root
the destination built against. In practice, this makes **Late Block Proofs**
the main follow-up feature required for robust operation on core-on-demand
chains, even though they are intentionally excluded from the minimal POC.

One important scope boundary follows from that: **guaranteed eventual delivery**
is a hard requirement for the full design, but the minimal Phase 1 POC only
demonstrates the happy-path inclusion-based mechanism. Full eventual-delivery
behavior depends on the follow-up work captured in
`speculative-messaging-follow-up-roadmap.md`, especially:

- late block proofs for lagging destinations
- retention / pruning rules
- bounded catch-up behavior
- fallback and resubmission policy

The document is organized in this order:

1. section 1 gives the end-to-end workflow and recommended implementation order
2. sections 2-3 define the new primitives and deterministic ingress model
3. sections 4-7 map each protocol step onto the current codebase
4. sections 8-11 cover coexistence, rollout, implementation checklist, and
   explicit out-of-scope items

---

## 1. Minimal POC Workflow

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

### 1.1 End-to-End Flow on the Current Architecture

The minimal POC fits into the current parachain flow like this:

1. **Candidate production on the sender**
   - The collator executes the parachain block normally.
   - Any outbound speculative XCM messages are appended to the sender's
     per-destination subtree/MMR state.
   - At the end of execution, the runtime can derive the sender's
     `ProvidesCommitment.root`.

2. **Off-chain batch serving and fetch**
   - After observing the sender candidate, source-side nodes retain a bounded
     recent history of speculative batch/proof data.
   - Destination collators obtain the corresponding `MessageBatch` off-chain
     from a provider.
   - For the initial POC, that provider can be a **separate relayer / indexer /
     helper process**, which is usually the simplest way to validate the
     end-to-end path without first building custom collator-to-collator P2P.
   - In a fuller deployment, the same data may also be served directly by
     source-side peers through a native collator request/response protocol.
   - This networking step is only a data-fetch path; it is not consensus by
     itself.

3. **Candidate production on the receiver**
   - The receiver collator chooses which fetched batches to include.
   - It embeds them into the block body as `SpeculativeIngress`, using the same
     inherent-style pattern already used for parachain-system inputs.
   - During block execution, `ingest_verified_messages` re-verifies the proofs,
     updates durable incoming state, records which source roots were actually
     consumed in this block, and dispatches the messages through the existing
     XCMP batch handler.

4. **Backing / PVF execution**
   - Backing validators execute the wasm PVF over the receiver candidate exactly
     as they do today.
   - Because `SpeculativeIngress` is part of `block_data`, validators replay the
     same imported batches deterministically.
   - The extended validation result returns both the legacy outputs and the new
     speculative `provides` / `requires` outputs.

5. **Candidate commitments reconstruction**
   - Node-side candidate validation reconstructs `CandidateCommitments` from the
     PVF outputs, including speculative messaging fields, and checks the
     commitments hash against the candidate receipt.

6. **Relay-chain backing and enactment**
   - `process_candidates()` still handles backing / pending-availability
     admission.
   - The actual speculative dependency check happens in the path that enacts
     pending candidates in the current relay block.
   - Each receiver-side `RequiresCommitment` is checked against either:
     the same-block enacted-provides set, or the latest persisted provides root
     for that source parachain.
   - If the candidate is actually enacted, the relay chain updates the
     persisted latest provides root for that parachain.

This means the POC does **not** invent a second execution pipeline. It reuses
the existing parachain lifecycle:

- collator fetches data off-chain,
- block body carries the consensus input,
- runtime executes it,
- PVF replays it,
- relay-chain inclusion checks the new commitment dependencies.

### 1.2 Recommended Implementation Order

If we implement this as a minimal POC on the current codebase, the practical
order should be:

1. **Primitives and version gating**
   - Add `v10` speculative types, `CandidateCommitments` extension, and v4
     descriptor/version handling.
2. **Parachain sender runtime**
   - Add speculative outbox tracking and the runtime API that derives the
     sender's cumulative `provides` root from executed block state.
3. **Parachain receiver runtime**
   - Add `SpeculativeIngress`, inherent wiring, deterministic runtime
     re-verification, `IncomingState`, and `get_requires_commitments()`.
4. **PVF / validation ABI / node-side commitments reconstruction**
   - Extend the validation result for v4 candidates and teach candidate
     validation to reconstruct v10 commitments.
5. **Relay-chain enactment rules**
   - Add persisted `ProvidesRoots` and enactment-time matching of `requires`
     against same-block enacted roots plus persisted latest roots.
6. **Off-chain networking**
   - Add a provider fetch path and bounded recent batch/proof history needed to
     serve recent requests.
   - For the initial POC, prefer a separate relayer/provider process.
   - Treat native collator request/response as an optional later transport
     optimization.
7. **Rollout and feature gating**
   - Enable the path only for v4 parachains, keep HRMP coexistence, and test
     the happy-path POC before any late-block-proof work.

---

## 2. Commitments Versioning Strategy

New types go into a **new `v10` primitives module**. The existing `v9` types are
frozen. New speculative-messaging candidates use `v10` types, while legacy
candidates continue to use the existing `v9` path. In other words, the
codebase becomes **version-aware**, not "blindly switched over" to a single new
commitments type.

```
polkadot/primitives/src/v10/mod.rs  ← NEW FILE
```

A `CandidateDescriptor` version bump to **v4** signals that the parachain supports
speculative messaging. Parachains still using v3 (or v2) descriptors skip the new
validation — the relay chain only enforces requires/provides matching for v4+
candidates. This provides backward compatibility and a clear upgrade path.

One practical implementation nuance is important here: in the current tree, the
main primitives still revolve around `CandidateDescriptorV2`,
`CandidateReceiptV2`, `CommittedCandidateReceiptV2`, and
`CandidateDescriptorVersion::{V1, V2, Unknown}`. So "v4" in this document
should be read as **the next concrete speculative-capable descriptor/receipt
version introduced in this codebase**, not as "there is already a real v4 type
available today". In practice, implementing this POC means updating the
descriptor/version/receipt plumbing consistently across:

- candidate descriptor version tagging
- candidate receipt / committed candidate receipt types
- node-side candidate validation
- relay-chain inclusion / pending-availability storage

The important design point is the **version-gated coexistence model**, not the
literal version numeral.

Concretely, the intended behavior is:

- legacy candidates keep their existing commitments layout and existing
  validation / reconstruction path
- v4 candidates use the extended commitments layout with speculative messaging
  fields
- relay-chain inclusion only enforces requires/provides matching for v4+
  candidates
- node-side candidate validation reconstructs commitments according to the
  candidate descriptor version

This means the upgrade is additive:

- pre-v4 parachains remain valid without speculative messaging
- v4 parachains opt into the new commitment semantics
- both formats can coexist during migration

```rust
// In v10/mod.rs:
pub struct CandidateDescriptorV4<N = BlockNumber> {
    // All descriptor fields needed by the existing candidate pipeline remain.
    // The exact carried-over set should match the real descriptor layout used by
    // the current candidate receipt path.
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
- `requires` should be **bounded** at the type or protocol level for production
  code; the POC may start with `Vec` in pseudocode, but the implementation
  should define a concrete maximum

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

Two additional invariants are implicit in these commitment types and should be
treated as part of the Phase 1 design:

1. **Canonicalization of `requires`**

   The `Vec<RequiresCommitment>` carried inside `CandidateCommitments` must have
   a canonical form before hashing:

   - sort entries by `source: ParaId` ascending
   - allow at most one `RequiresCommitment` per source parachain
   - reject duplicates before commitments hashing / inclusion

   This ensures semantically equivalent dependency sets cannot produce different
   `CandidateCommitments` hashes due only to ordering differences.

2. **Exact top-level root construction**

   `ProvidesCommitment.root` must always refer to one canonical top-level Merkle
   construction. For Phase 1, that construction is:

   - gather `(destination_para_id, subtree_root)` pairs from the sender's
     per-destination outbox state
   - sort those pairs by `destination_para_id`
   - compute each top-level leaf as:
     `leaf_hash = keccak256(SCALE(destination_para_id, subtree_root))`
   - compute the Merkle root over that ordered leaf list

   All proof generation, proof verification, and relay-visible commitment
   matching must use this exact same keyed-leaf encoding. `ProvidesCommitment`
   therefore never means "an arbitrary root"; it always means this specific
   canonical top-level root.

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
    /// Format: the opaque payload produced by `XcmpMessageHandler`.
    pub payload: Vec<u8>,
}
```

For the minimal POC, this shape is sufficient. It contains everything the
receiver needs to:

- verify that the destination-specific subtree is included in the sender's
  top-level `provides_root`
- verify per-source ordered continuity of messages against local receiver state
- reconstruct the receiver's local subtree and check it matches `subtree_root`
- dispatch the verified payloads through the existing XCMP batch handler

That said, several invariants should be considered part of the data model:

1. **Canonical subtree proof leaf**

   `subtree_inclusion_proof` must always prove inclusion of the keyed leaf:

   `leaf_hash = keccak256(SCALE(destination_para_id, subtree_root))`

   into `provides_root`.

   The destination parachain is not carried explicitly in `MessageBatch`
   because, in Phase 1, the receiver already knows "this batch is for me". But
   the sender-side proof construction and receiver-side verification must both
   use the same keyed leaf format based on the receiver's own `ParaId`.

2. **Canonical message ordering**

   `messages` must be ordered by ascending `position`, with no duplicates.
   During verification, the receiver expects them to advance continuously from
   `last_processed + 1`.

   So valid Phase 1 batches satisfy:

   - strictly increasing `position`
   - no missing positions inside the batch
   - the first position matches the receiver's expected next message for that
     source

3. **Batch-to-root consistency**

   `subtree_root` is not just metadata. It is the expected result of replaying
   the sender's per-destination message sequence up to the final message in this
   batch, starting from the receiver's previously accepted state for that source.

   In other words:

   - `provides_root` commits to `subtree_root`
   - `subtree_root` commits to the ordered message sequence
   - the receiver checks both links

4. **Minimal-vs-future fields**

   For Phase 1, the required fields are effectively:

   - `source`
   - `source_relay_parent_number`
   - `provides_root`
   - `subtree_root`
   - `subtree_inclusion_proof`
   - `messages`

   `source_block` is still a reasonable POC field because it gives the
   off-chain transport a concrete provenance handle and will be useful for later
   late-block-proof style extensions, but the Phase 1 relay-visible dependency
   logic fundamentally keys off `provides_root`, not `source_block`.

5. **Practical bounds**

   As with other protocol containers, `subtree_inclusion_proof`, `messages`, and
   each `payload` should have explicit bounds in a production implementation.
   The POC pseudocode can leave them as `Vec`, but the implementation should
   define concrete maxima for transport and runtime safety.

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

For Phase 1, `SpeculativeIngress.batches` should also follow simple canonical
selection rules:

- batches are grouped logically per `source`
- for a given `source`, they appear oldest-to-newest
- duplicate or overlapping batches for the same source in a single block should
  be rejected by collator precheck and by runtime re-verification

This keeps the runtime replay rules simple and avoids ambiguous within-block
ordering for one source's imported messages.

Phase 1 uses a single inherent-like dispatch, for example:

```rust
SpeculativeInbox::ingest_verified_messages { ingress: SpeculativeIngress }
```

This is the **practical implementation path on top of the current codebase**.
It should follow the same shape as Cumulus' existing inbound-message handling:

- The collator gathers external data before block construction
- `ProvideInherent` turns that data into an inherent call placed in the block
- The runtime re-verifies the data during block execution
- Validators replay the same inherent during `validate_block`

In other words, speculative messaging should behave more like
`ParachainSystem::set_validation_data` plus inbound HRMP/XCMP handling than like
an off-chain cache mutating pallet storage directly.

Concretely, the intended pattern is:

1. A node-local component fetches speculative batches from peers
2. An inherent-data provider places the accepted batches into `InherentData`
3. `ProvideInherent::create_inherent` constructs
   `SpeculativeInbox::ingest_verified_messages`
4. The runtime verifies proofs, updates `IncomingState`, records
   `ConsumedSourcesThisBlock`, and dispatches the embedded XCM payloads
5. `validate_block` replays the same call deterministically

This mirrors the existing Cumulus flow where collator-supplied inbound message
data is carried into the block and then executed in the runtime, rather than
being trusted as an off-chain side effect.

This is achievable in the current codebase because Cumulus already has the
necessary client-side and runtime-side hooks:

- the collator creates `InherentData` before proposing a block
- pallets can implement `ProvideInherent` to turn that data into an inherent
  call placed in the block body
- the block proposer includes those inherents during normal block construction
- `validate_block` replays the resulting block body deterministically

So the receiver-side speculative path does not require inventing a new block
input mechanism. It plugs into the same inherent pipeline already used for
`ParachainSystem::set_validation_data`.

The collator is responsible for:
1. Fetching batches from peers off-chain
2. Verifying them locally before block construction
3. Encoding the accepted batches into `SpeculativeIngress`
4. Placing the ingress call into the block body

Validators do not trust the collator's off-chain fetch. They only trust the
batch data that is present in the block body and re-verify it during block
execution.

For Phase 1, imported speculative messages are therefore defined as
**block-body inputs**, not as an implicit off-chain side channel and not as a
separate POV-only object. Practically, this means:

- The collator-selected `SpeculativeIngress` is encoded in an inherent-style call
  inside the block body
- That block body is already part of `block_data` during parachain validation
- Validators recover the same ingress payload by decoding and executing the block

So while the ingress data is naturally present in the candidate's PoV because
the block body is part of `block_data`, the canonical consensus input for Phase
1 is the inherent extrinsic carried by the block itself.

**Current-codebase embedding**

On the client side, the minimal integration path is:

1. Add a node-local speculative ingress collector that runs before proposal and
   returns the batches the collator wants to include for this block.
2. Extend the collator's `CreateInherentDataProviders` closure/implementation so
   it inserts a new inherent payload for speculative ingress into
   `sp_inherents::InherentData`.
3. Add a new runtime pallet such as `pallet-speculative-inbox` implementing
   `ProvideInherent`, with its own `INHERENT_IDENTIFIER`.
4. In `ProvideInherent::create_inherent`, decode the ingress payload from
   `InherentData` and construct:

```rust
SpeculativeInbox::ingest_verified_messages { ingress }
```

5. Let the existing proposer path include that inherent alongside the normal
   parachain-system inherent and other block inherents.

The relevant existing hook points in the current codebase are:

- `cumulus/client/consensus/aura/src/collator.rs`
  - the collator already creates `(ParachainInherentData, InherentData)` before
    proposal in `create_inherent_data_with_rp_offset(...)`
  - this is the natural place to extend the "other inherents" payload with
    `SpeculativeIngress`
- `substrate/primitives/inherents/src/client_side.rs`
  - `CreateInherentDataProviders` is the client-side abstraction that should be
    extended or wrapped so the speculative ingress collector can inject its data
- `cumulus/pallets/parachain-system/src/lib.rs`
  - this provides a concrete example of a pallet implementing `ProvideInherent`
    and turning `InherentData` into a block-body inherent call
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`
  - this is the replay path where validators execute the block body during PVF
    validation; speculative inbox execution follows the same deterministic path

So an implementer should think of the wiring as:

1. node-side fetcher/collector produces `SpeculativeIngress`
2. collator-side inherent-data creation inserts it into `InherentData`
3. runtime-side `ProvideInherent` decodes it into
   `SpeculativeInbox::ingest_verified_messages`
4. proposer includes that call in the block body
5. `validate_block` replays the same block body on validators

Conceptually, this fits the existing Cumulus proposer pipeline:

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

This means the receiver collator's "choice of which batches to include" lives in
the client-side inherent-data creation step, while the consensus-critical
execution still happens entirely inside the runtime call generated by
`ProvideInherent`.

One practical implementation detail is worth making explicit: if
`ingest_verified_messages` depends on fresh parachain-system state written by
`set_validation_data`, then the speculative inbox pallet should be ordered in the
runtime such that its inherent executes **after** `ParachainSystem`'s inherent.
That is fully compatible with the current inherent model, but the pallet order
should be chosen deliberately.

### 3.4 Message Payload Format

`OutgoingMessage.payload` contains raw XCM bytes — the same blob that
the receiver wants to deliver. During ingress execution, the runtime re-batches
the verified messages into the aggregate XCMP wire format expected by the
configured `T::XcmpMessageHandler::handle_xcmp_messages` interface. No new
message-execution trait is introduced for Phase 1; speculative ingress adapts to
the existing XCMP batch handler shape.

More precisely, `OutgoingMessage.payload` is the opaque outbound message payload
carried by the sender-side XCMP/XCM path and later delivered to the receiver's
`T::XcmpMessageHandler`. It is **consumed** by `XcmpMessageHandler`; it is not
"produced" by that interface.

When a block imports more than one speculative batch, the receiver runtime may
need to regroup messages by `(source, source_relay_parent_number)` before
calling `handle_xcmp_messages(...)`, so the dispatch shape still matches the
existing batch-oriented XCMP handler contract.

For empty blocks (no outbound messages, no inbound messages):
- `provides: None`
- `requires: vec![]`

---

## 4. Relay Chain Runtime Changes

### 4.1 New Module: `speculative_messaging.rs`

```
polkadot/runtime/parachains/src/speculative_messaging.rs  ← NEW FILE
```

```rust
use frame_support::pallet_prelude::*;
use polkadot_primitives::v10::{Hash, Id as ParaId};

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

### 4.2 Validation in `inclusion/mod.rs`

The relay-chain integration must distinguish **backing/pending-availability**
from **actual inclusion/enactment**.

In the current architecture:

- `inclusion::process_candidates()` handles newly backed candidates and moves
  them into `PendingAvailability`
- candidates may remain pending for some time before they are actually enacted
- `inclusion::enact_candidate()` is the inclusion-time path that applies the
  candidate's relay-visible messaging effects

For speculative messaging, this means:

- persisted `ProvidesRoots` must be updated only when a candidate is actually
  enacted/included
- requires/provides dependency satisfaction must be defined against roots that
  are actually included in relay-visible state, not merely newly backed in the
  same block

So the minimal POC should use a two-stage relay-chain treatment:

1. `process_candidates()` remains the place where v4 candidates are admitted
   into pending availability alongside their `provides` / `requires` fields.
2. The actual requires/provides satisfaction check for same-block dependencies
   is performed in the path that determines which pending candidates are enacted
   in the current relay block, immediately before `enact_candidate()` is called.

For the minimal POC, this exact-root matching is considered sufficient under a
happy-path timing assumption: source and destination chains produce at roughly
the same pace, and the destination is not delayed for many source-root
advances. Under those conditions, matching against either (a) a same-block
provide or (b) the latest persisted provide is enough to demonstrate the basic
protocol. Robust handling of delayed destinations, especially core-on-demand
chains, is deferred to the later Late Block Proofs phase.

```rust
// Stage 1: backing / pending-availability admission
pub(crate) fn process_candidates<GV>(
    allowed_relay_parents: &AllowedRelayParentsTracker<T::Hash, BlockNumberFor<T>>,
    candidates: &BTreeMap<ParaId, Vec<(BackedCandidate<T::Hash>, CoreIndex)>>,
    group_validators: GV,
    ...
) -> Result<..., Error> {
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
    // Same-block provides are tracked as a SET per source para, not a single root.
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

This step is achievable in the current codebase because the relay-chain runtime
already has the right structural hooks:

- `paras_inherent` sanitizes and groups backed candidates
- `inclusion::process_candidates()` is the place where candidate receipts are
  checked and moved into pending availability
- the enactment path is where pending candidates are actually included in the
  current relay block
- `inclusion::enact_candidate()` is the place where inclusion-time messaging
  effects are applied

Speculative messaging fits this model well because the relay chain is not asked
to verify message proofs again here. It only needs to:

1. inspect the already-validated `provides` / `requires` fields carried in the
   candidate commitments
2. check dependency satisfaction against same-block enacted roots and persisted
   roots
3. persist the newest provides root for future blocks

So this is a relay-runtime inclusion rule change, not a new protocol stage.

**Current-codebase embedding**

For the POC, the practical implementation path is:

1. Add a small new relay-runtime module, e.g.
   `polkadot/runtime/parachains/src/speculative_messaging.rs`, holding:
   - `ProvidesRoots<ParaId, Hash>`
   - helpers to read/update the latest root for a parachain
2. Register that module in `polkadot/runtime/parachains/src/lib.rs`.
3. Keep `inclusion::process_candidates()` as the backing / pending-availability
   admission path for v4 candidates.
4. Extend the code path that enacts pending candidates in the current relay
   block so it maintains a temporary in-memory:
   `enacted_provides_in_block: BTreeMap<ParaId, BTreeSet<Hash>>`
5. Immediately before enacting a v4 candidate, validate each
   `RequiresCommitment` against:
   - path 1: `enacted_provides_in_block`
   - path 2: `SpeculativeMessaging::provides_root(source)`
6. Reject/drop the candidate from enactment if any requirement is unsatisfied.
7. When the candidate is actually enacted/included, update
   `SpeculativeMessaging::update_provides_root(para_id, root)`.

Conceptually:

```rust
// Enactment-time same-block state for the CURRENT relay block
let mut enacted_provides_in_block: BTreeMap<ParaId, BTreeSet<Hash>> = BTreeMap::new();

for candidate in pending_candidates_being_enacted_now {
    if candidate.is_v4() {
        for req in candidate.commitments.requires() {
            let satisfied_same_block = enacted_provides_in_block
                .get(&req.source)
                .map_or(false, |roots| roots.contains(&req.expected_root));
            let satisfied_persisted =
                SpeculativeMessaging::<T>::provides_root(&req.source) == Some(req.expected_root);
            ensure!(satisfied_same_block || satisfied_persisted, Error::<T>::UnsatisfiedRequires);
        }
    }

    Self::enact_candidate(...);

    if candidate.is_v4() {
        if let Some(provides) = candidate.commitments.provides() {
            enacted_provides_in_block
                .entry(candidate.para_id())
                .or_default()
                .insert(provides.root);
            SpeculativeMessaging::<T>::update_provides_root(candidate.para_id(), provides.root);
        }
    }
}
```

This aligns well with the current relay-chain architecture because:

- `process_candidates()` already handles the backing/pending-availability stage
- `enact_candidate()` already handles inclusion-time relay-visible effects
- the same-block tracking set is purely in-memory for the current enactment pass
- persisted latest-root state is tiny and naturally belongs in a dedicated relay
  runtime storage map
- the update point can stay coupled to successful inclusion, just like other
  inclusion-time state updates already handled by `inclusion`

For the minimal POC, the "latest persisted root OR same-block root" rule is a
reasonable fit for the current architecture. More robust delayed-chain handling
can be layered later with Late Block Proofs without changing where this
inclusion check lives.

### 4.3 New Error

```rust
/// A requires commitment could not be matched to any provides.
UnsatisfiedRequires,
```

---

## 5. Parachain Runtime Changes

### 5.1 Outgoing Message MMR (Sender Side)

New pallet or utility module in the parachain runtime. Pattern: **wrap the
runtime's configured `OutboundXcmpMessageSource`** (which is typically `XcmpQueue`) by
implementing the `XcmpMessageSource` trait such that each outbound message is both
recorded in the speculative outbox and forwarded to the inner source. The wrapping
type then replaces `XcmpQueue` as the `type OutboundXcmpMessageSource` in the
parachain runtime's `ParachainSystem` config. This is the same interception-point
pattern that parachain-system already uses to drain outbound HRMP messages in
`on_finalize` (see `cumulus/pallets/parachain-system/src/lib.rs` line ~409).

This sender-side flow is **not** a separate off-chain-only pipeline. The
collator does build the block off-chain, but the speculative outbox state must
still be produced by **normal runtime block execution** so validators can replay
the same state transition during `validate_block`.

The intended execution model is:

1. Runtime execution emits outbound sibling-parachain XCM through the existing
   `SendXcm`/`XcmpQueue` path.
2. The speculative outbox wrapper intercepts those outbound payloads during that
   same runtime execution and appends them into per-destination MMR state.
3. After block execution finishes, the collator reads the resulting
   `provides_root` from runtime state via runtime API, just like Cumulus already
   reads other collation outputs after block execution.

This keeps the design aligned with the current codebase shape:

- outbound messages are produced by runtime execution, not by a collator-local
  cache
- parachain-system still drains outbound XCMP messages from the runtime's
  `OutboundXcmpMessageSource`
- the collator gathers post-execution outputs via runtime API
- validators replay the full block, including speculative outbox updates, inside
  the wasm PVF

For a minimal POC, the least invasive embedding is a new
`pallet-speculative-outbox` (or equivalent utility module) that wraps the
runtime's outbound XCMP sender rather than replacing the existing outbound
pipeline. In practice, it should:

- hook into the runtime path that currently sends sibling-parachain XCM through
  `XcmpQueue`
- hash each outbound payload and append it to `OutgoingMMRs[destination]`
- preserve the normal XCMP delivery path so HRMP/XCMP output behavior remains
  intact
- expose `compute_provides_root()` as a runtime API for the collator after
  execution
- optionally expose serving helpers for off-chain batch distribution to receiver
  collators

In short: the collator orchestrates block production off-chain, but the sender's
speculative outbox contents and `ProvidesCommitment.root` must be a deterministic
result of runtime execution.

```rust
/// Per-destination MMRs for outgoing messages.
#[pallet::storage]
pub type OutgoingMMRs<T: Config> = StorageMap<
    _, Twox64Concat, ParaId, MMRState,
>;

#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct MMRState {
    /// Leaf count for THIS destination's subtree MMR.
    /// This is the position space used by `OutgoingMessage.position`.
    pub leaf_count: u64,
    pub root: H256,
    /// Nodes stored for proof generation (peaks + internal nodes).
    pub nodes: BTreeMap<u64, H256>,
}

/// Optional per-destination historical cache for proof serving.
/// This is only needed if the implementation wants to reconstruct recent
/// subtree proofs from runtime-managed state rather than an off-chain/provider
/// index. The Phase 1 protocol does not require this to exist on-chain.
#[pallet::storage]
pub type DestinationLeafCountByBlock<T: Config> = StorageDoubleMap<
    _,
    Twox64Concat, ParaId,
    Twox64Concat, BlockNumberFor<T>,
    u64,
>;

/// Optional sender-wide append counter across all destinations.
/// This can be useful for metrics or node-local indexing, but it is not part of
/// the Phase 1 receiver verification model and is not required to compute
/// `ProvidesCommitment.root`.
#[pallet::storage]
pub type TotalLeafCount<T: Config> = StorageValue<_, u64, ValueQuery>;
```

The important distinction is:

- `OutgoingMMRs[destination].leaf_count` is the authoritative leaf count for
  that destination's subtree MMR
- `OutgoingMessage.position` refers to that per-destination counter
- `ProvidesCommitment.root` is derived from the set of current subtree roots
- a single sender-wide `TotalLeafCount` does **not** define the proof/position
  space used by receivers

For Phase 1 correctness, only the per-destination subtree state is required on
the consensus path. A sender-wide counter may still be kept as an implementation
detail, but it should not be read as the primary leaf-count model for this
hierarchical design.

**Block lifecycle hook** (`on_finalize`, optional cache path):

```rust
fn on_finalize(_n: BlockNumberFor<T>) {
    let now = frame_system::Pallet::<T>::block_number();
    for (destination, state) in OutgoingMMRs::<T>::iter() {
        DestinationLeafCountByBlock::<T>::insert(destination, now, state.leaf_count);
    }
}
```

This is one valid caching strategy, but it is not required for correctness.
`compute_provides_root()` can still be derived deterministically from executed
block state and read by the collator after execution via runtime API.

For the initial POC, the simpler and more realistic approach is usually to keep
recent `MessageBatch` / subtree-proof material in a node-local relayer/provider
index, rather than storing historical subtree snapshots in runtime storage.
That provider-side retained history is already assumed elsewhere in this design.

**Computing the provides root** — called by the collator after block execution to
populate `CandidateCommitments.provides`:

For Phase 1, `CandidateCommitments.provides` uses **cumulative latest-root
semantics**. That is, it commits to the sender's full current speculative
outbox state after executing this block, not merely "the delta produced by this
block".

This means:

- if this block emits new speculative outbound messages, the root advances
- if this block emits no new speculative outbound messages but the sender
  already has speculative outbox state, the same latest root may be re-emitted
- relay-chain matching treats `provides` as "the latest root this candidate
  exposes after execution", not "a block-local diff commitment"

That cumulative-root interpretation is the one used consistently by the Phase 1
requires/provides matching model.

The top-level Merkle tree is defined over **keyed leaves**, not raw subtree
roots. Each leaf is the SCALE encoding of `(destination_para_id, subtree_root)`,
with leaves sorted by `destination_para_id` before Merkle root computation.
Proof generation and proof verification must use this exact same leaf format.

**MMR implementation approach.** The hierarchical accumulator structure uses two
different constructions, each suited to its role:

- **Per-destination subtrees** are MMRs that grow over time as messages are
  appended. The codebase already ships `sp-mmr-primitives` (at
  `substrate/primitives/merkle-mountain-range/`) with append, prove, and peek
  operations. Per-destination subtrees can be implemented as instances of
  `sp_mmr_primitives::MMR` stored in `OutgoingMMRs` and `IncomingState`.
- **The top level** is rebuilt every block from the current set of
  `(destination_para_id, subtree_root)` pairs. Since it never needs
  append-only proofs connecting historical roots, a plain binary Merkle tree
  (not an MMR) is sufficient. The canonical construction: sort leaves by
  `destination_para_id`, compute each leaf as `keccak256(SCALE(key))`, and
  build a standard binary Merkle tree.

The top-level tree uses **Keccak256** rather than the Substrate-default Blake2.
Two reasons: (a) the keyed-leaf pattern `keccak256(SCALE(para_id, root))`
prevents second-preimage attacks where an attacker could interpret a leaf hash
as an internal node hash, which is a known concern with unbalanced or non-padded
Merkle trees; (b) Keccak256 is the EVM-native hash, which simplifies
interop with EVM-side light-client or bridge verifiers that may need to check
subtree inclusion against a top-level provides root in the future.

```rust
pub fn compute_provides_root() -> Option<ProvidesCommitment> {
    let mut roots: Vec<(ParaId, H256)> = OutgoingMMRs::<T>::iter()
        .map(|(dest, state)| (dest, state.root))
        .collect();

    if roots.is_empty() {
        return None;  // no speculative outbox state exists yet
    }

    roots.sort_by_key(|(id, _)| *id);
    // Canonical top-level leaf format:
    // leaf_hash = keccak256(SCALE(destination_para_id, subtree_root))
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
/// This is ephemeral "produced this block" state, analogous to other per-block
/// collation outputs such as upward or outbound HRMP messages. It is not durable
/// protocol state like `IncomingState`.
#[pallet::storage]
pub type ConsumedSourcesThisBlock<T: Config> = StorageValue<
    _,
    Vec<(ParaId, H256)>, // (source, expected top-level provides root)
    ValueQuery,
>;
```

**Message batch verification** has two phases:

1. **Collator-local precheck** before block building. This uses a collator-local
   cache of the receiver's latest finalized `IncomingState` snapshot and does
   not mutate runtime storage.
2. **Runtime verification** inside `ingest_verified_messages`, which replays the
   same checks against on-chain state and updates pallet storage deterministically.

The collator-local precheck is only an optimization for selecting batches. It is
not consensus-critical state.

Both phases are required because they serve different purposes:

- **Collator precheck** is for selection and efficiency. It helps the collator
  avoid proposing obviously invalid, stale, or non-consecutive batches and lets
  it choose which valid batches to include when block space is limited.
- **Runtime re-verification** is for consensus and safety. Validators do not
  trust the collator's off-chain fetch or precheck result; they only trust the
  `SpeculativeIngress` embedded in the block body and must be able to replay the
  same proof, ordering, and subtree-root checks deterministically during block
  execution.

Without collator precheck, the design would still be safe but wasteful. Without
runtime re-verification, it would be efficient but not consensus-safe.

```rust
/// Collator-local cache. Not part of runtime storage.
struct LocalIncomingSnapshot {
    per_source: BTreeMap<ParaId, SourceState>,
}
```

**Collator-local precheck** — runs in the collator's off-chain logic BEFORE block
building:

```rust
/// Collator off-chain: verify an incoming MessageBatch from a source chain.
/// Returns `Ok(())` if this batch can be proposed for inclusion.
pub fn precheck_message_batch(
    snapshot: &mut LocalIncomingSnapshot,
    batch: &MessageBatch,
) -> Result<(), VerificationError> {
    // 1. Verify subtree_inclusion_proof:
    //    leaf = SCALE(destination_para_id, subtree_root)
    //    prove leaf is in provides_root (top-level Merkle tree)
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

**On-chain ingress execution** — this is the consensus-critical path:

```rust
fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
    ConsumedSourcesThisBlock::<T>::kill();
    Weight::zero()
}

#[pallet::call]
impl<T: Config> Pallet<T> {
    /// Inherent-like call inserted by the collator.
    /// Re-verifies all batches against runtime storage, updates IncomingState,
    /// records which source roots this block actually depended on, and dispatches
    /// the embedded XCM payloads.
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

            // Phase 1 invariant: a single receiver block may only consume one
            // distinct top-level provides root per source parachain. This keeps
            // `requires` at one canonical entry per source.
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

            // Re-batch the verified messages into the existing XCMP wire format
            // and dispatch them through the standard batch handler.
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

Phase 1 keeps **per-source ordered consumption**: batches must advance the
receiver's local subtree continuously for each source. This matches the main
speculative messaging design and keeps the runtime replay rules simple.

**Encoding for the XCMP handler.** The existing `XcmpMessageHandler::handle_xcmp_messages`
interface (defined in `polkadot/parachain/src/primitives.rs`) takes an iterator
of `(ParaId, RelayChainBlockNumber, &[u8])` where each `&[u8]` is an XCMP
*page* — a byte slice prefixed with an `XcmpMessageFormat` tag followed by
concatenated message data. The `encode_xcmp_batch` helper must therefore produce
this page format from the verified speculative payloads:

```rust
/// Encode verified speculative message payloads into the XCMP page format
/// expected by `XcmpMessageHandler::handle_xcmp_messages`.
///
/// The sender's outbox stores payloads in the same encoding used by the
/// configured `XcmpMessageFormat` variant; here we assume
/// `ConcatenatedVersionedXcm` (the default for sibling-parachain XCM).
fn encode_xcmp_batch<'a>(payloads: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut page = XcmpMessageFormat::ConcatenatedVersionedXcm.encode();
    for payload in payloads {
        page.extend_from_slice(payload);
    }
    page
}
```

If the sender stores payloads in a different format (e.g.
`ConcatenatedEncodedBlob`), use the matching variant. The key constraint is that
the format variant must match what the receiver's `XcmpMessageHandler`
implementation knows how to decode. For the POC, using
`ConcatenatedVersionedXcm` throughout is the simplest consistent choice.

For the initial POC, this is the right level of integration with the current
architecture:

- collator-side selection happens before proposal
- ingress enters the block through the existing inherent mechanism
- runtime execution re-verifies and applies it deterministically
- validators replay exactly the same call through the existing `validate_block`
  pipeline

No new block-body transport or custom PVF input path is needed for this step.

If the runtime detects two different top-level `provides_root` values for the
same `source` within one receiver block, it should reject the ingress as
invalid. The pseudocode above refers to this as
`Error::<T>::MultipleRootsPerSourceInOneBlock`; the exact error name is up to
the implementation, but the invariant should be explicit.

### 5.3 Producing Commitments

After block execution, the collator reads the provides/requires from runtime
storage and populates `CandidateCommitments`. These values are computed from the
runtime state that resulted from executing the block, including the
`ingest_verified_messages` call embedded in the block body.

For Phase 1, `requires` also uses a simple canonicalization rule:

- at most one `RequiresCommitment` per source parachain per block

This is enforced by the receiver-side ingestion rules above: a single block may
not consume two distinct top-level `provides_root` values from the same source.
That allows `requires` to remain a canonical one-entry-per-source set.

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
        // provides: computed from OutgoingMMRs
        let provides = self.runtime_api
            .compute_provides_root(block.hash())
            .await?;

        // requires: one entry per source actually consumed in THIS block
        let requires = self.runtime_api
            .get_requires_commitments(block.hash())
            .await?;

        v10::CandidateCommitments {
            upward_messages: block.upward_messages(),
            horizontal_messages: block.horizontal_messages(),  // HRMP coexists
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
validation ABI returns a `ValidationResult` containing only the legacy fields
(`head_data`, `upward_messages`, `horizontal_messages`, etc.). Since
speculative messaging adds relay-visible commitment outputs, the practical POC
path is to extend the validation result returned by the wasm PVF so that it also
contains:

- `provides: Option<ProvidesCommitment>`
- `requires: Vec<RequiresCommitment>`

Because speculative ingress is embedded in the block body via
`SpeculativeIngress`, validators replay exactly the same message batches the
collator chose. No hidden off-chain input is needed.

For implementation clarity, the minimal POC should choose **one upgraded wasm
result shape** on the ABI boundary for upgraded runtimes, rather than implying
multiple binary return schemas depending on candidate version. In other words:

- the wasm entrypoint returns one upgraded validation-result struct
- speculative-capable candidates populate `provides` / `requires`
- non-speculative candidates on the upgraded runtime return:
  - `provides: None`
  - `requires: vec![]`

The **semantic** interpretation remains version-gated by the candidate
descriptor/receipt version, but the wasm return shape itself should be one
consistent upgraded schema for the POC.

This step is achievable, but unlike the networking and inherent-injection steps,
it does require a **real cross-layer interface change**. In the current codebase:

- the wasm PVF returns `polkadot_parachain::primitives::ValidationResult`
- `cumulus_pallet_parachain_system::validate_block` constructs that result after
  executing the block
- the wasm result is serialized through `polkadot/parachain/src/wasm_api.rs`
- node-side candidate validation reconstructs `CandidateCommitments` from the
  returned validation outputs and checks the commitments hash

So the embedding for Phase 1 is not "add one more runtime API". The correct
embedding is:

1. extend the wasm validation result type for v4 speculative candidates
2. teach parachain-system's `validate_block` path to populate the new
   `provides` / `requires` fields from post-execution runtime state
3. update the wasm result serialization layer to return the extended structure
4. update node-side candidate validation to decode that extended result,
   reconstruct the extended `CandidateCommitments`, and hash-check it against
   the candidate receipt

That still fits the current architecture well: validators already execute the
same block deterministically and already reconstruct commitments from PVF
outputs today. Speculative messaging just adds two more commitment outputs to
that existing path.

The key point is that the wasm PVF does **not** read candidate commitments as an
input. Instead, it executes the block, derives the full validation outputs, and
returns them. Then the node-side candidate-validation pipeline reconstructs
`CandidateCommitments` from those returned outputs and checks the commitments
hash exactly as it does today for the legacy fields.

The important nuance is that the speculative outputs should be treated as
additional fields assembled by the `validate_block` implementation from the
post-execution runtime state. They are not meant to imply a separate "runtime
API call from inside the PVF" after execution. This matches the current legacy
flow more closely: `validate_block` executes the block, reads the resulting
state/output fields, and returns one consolidated validation result.

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

/// In the PVF (wasm), called during candidate validation:
fn validate_block(params: ValidationParams) -> Result<ValidationResultV4, ValidationError> {
    // 1. Execute the block and collect the standard validation outputs plus the
    //    speculative outputs produced by block execution.
    let result = execute_block_and_collect_outputs(&params)?;

    // 2. Return the full validation outputs
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

**Current-codebase embedding**

For the POC, the concrete implementation path is:

1. In `polkadot/parachain/src/primitives.rs`, introduce an extended validation
   result shape for v4 speculative candidates.
2. In `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`,
   after block execution finishes, read the speculative outputs produced by the
   runtime (`provides`, `requires`) and include them in the returned validation
   result.
3. In `polkadot/parachain/src/wasm_api.rs`, return that extended result from the
   wasm entrypoint exactly like the legacy validation result is returned today.
4. In `polkadot/node/core/candidate-validation`, decode the extended result and
   reconstruct `v10::CandidateCommitments` instead of the legacy commitments for
   v4 candidates.
5. Keep older descriptor versions on the legacy path so pre-v4 parachains remain
   unaffected.
6. Update any relay-chain runtime-API entrypoints that still accept the legacy
   unversioned `CandidateCommitments`, especially
   `ParachainHost::check_validation_outputs` in
   `polkadot/primitives/src/runtime_api.rs` and the corresponding
   `check_validation_outputs_for_runtime_api(...)` path in
   `polkadot/runtime/parachains/src/inclusion/mod.rs`, so they remain coherent
   with the new speculative commitments layout.

Conceptually, the node-side validation logic remains the same:

```rust
let pvf_result = execute_pvf(candidate, pov)?;
let reconstructed_commitments = CandidateCommitments::from_validation_result(pvf_result);
ensure!(
    reconstructed_commitments.hash() == candidate.commitments_hash,
    CommitmentsHashMismatch,
);
```

The only difference is that for v4 speculative candidates,
`from_validation_result(...)` now includes `provides` and `requires` in the
reconstructed commitments.

On the node side, candidate validation constructs `v10::CandidateCommitments`
from the returned `ValidationResultV4` and compares its hash to the candidate
receipt's commitments hash. That is the same place in the pipeline where the
legacy candidate commitments are already reconstructed today.

So the answer for Phase 1 is:

- yes, backing/PVF execution is achievable with the current architecture
- yes, validators can replay `SpeculativeIngress` deterministically because it is
  already inside `block_data`
- but this step does require a targeted validation-ABI / candidate-validation
  upgrade, not just runtime logic alone

### 6.1 Candidate Commitments Reconstruction

This step is also achievable in the current codebase. In fact, the current
candidate-validation pipeline already follows exactly this model for legacy
fields:

1. execute the PVF
2. read the returned validation outputs
3. reconstruct `CandidateCommitments`
4. hash them
5. compare the hash to `candidate_receipt.commitments_hash`

For speculative messaging, the practical change is to extend that existing
reconstruction path for v4 candidates so it also includes:

- `provides: Option<ProvidesCommitment>`
- `requires: Vec<RequiresCommitment>`

This is not a new subsystem. It is a version-gated extension of the existing
node-side commitments reconstruction logic.

**Current-codebase embedding**

Today, node-side candidate validation reconstructs commitments inline after PVF
execution. For the POC, update that logic so it branches on candidate descriptor
version:

```rust
match candidate_receipt.descriptor.version() {
    V1 | V2 | V3 => {
        let commitments = v9::CandidateCommitments {
            head_data,
            upward_messages,
            horizontal_messages,
            new_validation_code,
            processed_downward_messages,
            hrmp_watermark,
        };
        ensure!(commitments.hash() == candidate_receipt.commitments_hash, ...);
    }
    V4 => {
        let commitments = v10::CandidateCommitments {
            head_data,
            upward_messages,
            horizontal_messages,
            new_validation_code,
            processed_downward_messages,
            hrmp_watermark,
            provides,
            requires,
        };
        ensure!(commitments.hash() == candidate_receipt.commitments_hash, ...);
    }
}
```

The corresponding implementation work is:

1. add `v10::CandidateCommitments` and the new speculative types in
   `polkadot/primitives`
2. extend candidate receipt / descriptor version handling so v4 candidates use
   the new commitments layout
3. update `polkadot/node/core/candidate-validation` to reconstruct the correct
   commitments type for each descriptor version
4. keep all pre-v4 candidates on the unchanged legacy reconstruction path

That makes this step a clean fit for the current architecture:

- PVF execution still returns validation outputs
- node-side validation still reconstructs commitments locally
- hash checking still happens in the same place
- only the commitment schema changes for the new candidate version

---

## 7. Off-Chain Networking

The destination collator needs an off-chain path to fetch `MessageBatch` data
before block construction. For the **initial POC**, this document assumes a
**separate relayer/provider process** rather than native collator-to-collator
P2P.

That transport path is **not consensus-critical**. Consensus still depends only
on `SpeculativeIngress` being embedded in the block body and re-verified during
execution.

The relayer/provider is responsible for:

- watching source blocks
- retaining a bounded recent window of batch/proof material
- serving that data to destination collators before proposal

The detailed relayer/provider transport design lives in:

- [speculative-messaging-networking-design.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-networking-design.md)

---

## 8. HRMP Coexistence

Phase 1 runs **alongside HRMP**. Both paths produce/consume messages. The receiver
deduplicates: if the same message arrives via both HRMP and speculative messaging,
the second dispatch attempt is ignored (replay protection by `mmr_leaf_index` or
message hash).

**Collator block building order**:
1. Fetch pending messages via HRMP (from relay parent, as before)
2. Fetch pending messages via speculative messaging (off-chain)
3. Locally precheck speculative batches and encode them into `SpeculativeIngress`
4. Both sets of messages are executed in the same block
5. Both HRMP watermark and provides/requires are emitted in `CandidateCommitments`

The `horizontal_messages` field in `CandidateCommitments` continues to carry HRMP
messages. Speculative messaging messages are NOT carried in `horizontal_messages`
— they are carried in the block body's `SpeculativeIngress` call, while
`requires` only commits to the source `provides` roots that were actually used.

**Weight accounting.** Both HRMP (`handle_xcmp_messages` called from
`ParachainSystem::set_validation_data`) and speculative ingress call
`handle_xcmp_messages`, each consuming from the same
`ReservedXcmpWeight`/`ReservedXcmpWeightOverride` budget. The simplest POC
approach: set the total reserved XCMP weight high enough to cover both paths in
the worst case, and let each call consume what it needs. The two calls are
independent — speculative ingress does not share a weight meter with the HRMP
path. For a more precise approach later, the weight budget can be split
explicitly between the two sources, but this is not required for the POC.

---

## 9. Feature Gating & Upgrade Path

### 9.1 Per-Parachain Enablement

A parachain signals speculative messaging support by upgrading to a
`CandidateDescriptor` v4. The relay chain only enforces requires/provides for v4
candidates. v3 (and v2) candidates skip the new validation entirely.

The upgrade order:
1. Parachain runtime upgrades to maintain speculative inbox/outbox state and
   expose runtime APIs for computing `provides` / `requires` from executed block state
2. Collator nodes upgrade to support v4 descriptors and the new protocol
3. Relay chain runtime upgrades to recognize v4 descriptors and perform
   commitment matching
4. Once all three are deployed, messages begin flowing through the new path

### 9.2 Per-Channel Gating (Optional)

For finer control, a parachain runtime config can list which source chains to use
speculative messaging with:

```rust
parameter_types! {
    pub SpeculativeMessagingSources: Vec<ParaId> = vec![
        ParaId(1000),  // use speculative messaging for messages from para 1000
        // ParaId(2000),  // commented out: still use HRMP for para 2000
    ];
}
```

Sources not in this list continue to receive messages via HRMP only.

---

## 10. Codebase Implementation Plan

For the minimal POC, the most practical way to build this on the current
codebase is to implement it in the following order.

### 10.1 Step 1: Primitives and Version Gating

**Primary files/modules**

- `polkadot/primitives/src/v10/mod.rs`
- `polkadot/primitives/src/lib.rs`
- `polkadot/primitives/test-helpers/src/lib.rs`
- any call sites still constructing or pattern-matching `v9::CandidateCommitments`

**Implementation work**

1. Add a new `v10` module containing:
   - `ProvidesCommitment`
   - `RequiresCommitment`
   - `MessageBatch`
   - `OutgoingMessage`
   - `SpeculativeIngress` if you want it shared at the primitives layer
   - v10 `CandidateCommitments`
2. Re-export the new types in `polkadot/primitives/src/lib.rs` only if the team
   wants an unversioned convenience path; otherwise keep them version-qualified.
3. Extend descriptor-version handling so speculative candidates can be
   identified as v4 while pre-v4 candidates continue to use the legacy path.
4. Update helper/test constructors in
   `polkadot/primitives/test-helpers/src/lib.rs` so v4 test candidates can be
   built explicitly.

**Why first**

Everything else depends on the commitment schema and version gates being
defined first.

### 10.2 Step 2: Receiver Runtime Ingress Path

This is the best first end-to-end vertical slice because it establishes the
deterministic block-body input model.

**Primary files/modules**

- new parachain pallet, e.g. `cumulus/pallets/speculative-inbox/`
- `cumulus/pallets/parachain-system/src/lib.rs`
- target parachain runtime(s), e.g.
  `cumulus/parachains/runtimes/testing/penpal/src/lib.rs`
  or another chosen POC runtime
- `substrate/frame/support` / `sp-inherents` are reference points only; no core
  framework change should be needed for Phase 1

**Implementation work**

1. Add a new pallet implementing:
   - `IncomingState`
   - `ConsumedSourcesThisBlock`
   - `ingest_verified_messages`
   - `ProvideInherent`
2. Re-verify:
   - subtree inclusion against the destination-keyed top-level leaf
   - per-source message continuity
   - subtree-root reconstruction
   - the Phase 1 invariant that one block cannot consume two distinct
     top-level `provides_root` values from the same source
3. Dispatch verified payloads through the existing
   `T::XcmpMessageHandler::handle_xcmp_messages(...)` batch interface.
4. Register the pallet in the chosen parachain runtime and ensure its inherent
   executes after `ParachainSystem::set_validation_data` if it depends on fresh
   parachain-system state.
5. Expose a runtime API such as `get_requires_commitments()`.

**Concrete runtime hook**

- `cumulus/pallets/parachain-system/src/lib.rs`
  already demonstrates the `ProvideInherent` pattern and the existing
  `OutboundXcmpMessageSource` / validation flow that this should integrate with.

### 10.3 Step 3: Sender Runtime Outbox Path

**Primary files/modules**

- new parachain pallet or wrapper, e.g. `cumulus/pallets/speculative-outbox/`
- `cumulus/pallets/parachain-system/src/lib.rs`
- chosen POC runtime(s) where
  `type OutboundXcmpMessageSource = XcmpQueue;`
  already exists, such as:
  - `cumulus/parachains/runtimes/testing/penpal/src/lib.rs`
  - `cumulus/parachains/runtimes/assets/asset-hub-westend/src/lib.rs`
  - other concrete parachain runtimes depending on the POC target

**Implementation work**

1. Wrap or sit alongside the existing outbound XCMP path so speculative outbox
   tracking happens during normal runtime execution.
2. Maintain per-destination outgoing subtree/MMR state.
3. Implement canonical top-level root construction over
   `(destination_para_id, subtree_root)` keyed leaves.
4. Expose a post-execution runtime API such as `compute_provides_root()`.
5. If the POC needs source-side serving support directly from runtime state,
   expose helper APIs for recent subtree/proof data, but keep historical serving
   primarily in the node layer.

**Concrete runtime hook**

- In parachain runtimes, the current `type OutboundXcmpMessageSource = XcmpQueue;`
  definition is the natural integration point for the sender-side wrapper.

### 10.4 Step 4: Collator-Side Inherent Injection and Commitment Assembly

**Primary files/modules**

- `cumulus/client/consensus/aura/src/collator.rs`
- `cumulus/client/consensus/aura/src/collators/basic.rs`
- `cumulus/client/consensus/aura/src/collators/lookahead.rs`
- `substrate/primitives/inherents/src/client_side.rs`
- whichever node/service crate owns the chosen POC collator wiring

**Implementation work**

1. Add a node-local speculative fetch/precheck component that runs before block
   proposal.
2. Extend the inherent-data creation path so `SpeculativeIngress` is inserted
   into `InherentData`.
3. After block execution, read runtime-produced `provides` and `requires`
   outputs and construct the v4 commitments used in the candidate receipt.

**Concrete runtime hook**

- `cumulus/client/consensus/aura/src/collator.rs`
  `create_inherent_data_with_rp_offset(...)`
  already assembles `(ParachainInherentData, InherentData)` and is the most
  direct place to inject the new ingress payload.

### 10.5 Step 5: PVF / Wasm Validation ABI

**Primary files/modules**

- `polkadot/parachain/src/primitives.rs`
- `polkadot/parachain/src/wasm_api.rs`
- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`
- runtime entrypoints using
  `cumulus_pallet_parachain_system::register_validate_block! { ... }`

**Implementation work**

1. Extend the wasm validation result shape for v4 speculative candidates.
2. In `validate_block`, assemble speculative outputs from post-execution runtime
   state, alongside the legacy validation outputs.
3. Ensure wasm result serialization returns the extended shape for v4 while
   preserving the legacy path for older candidates.

**Concrete runtime hook**

- `cumulus/pallets/parachain-system/src/validate_block/implementation.rs`
  is where block execution happens and the current legacy validation outputs are
  collected.

### 10.6 Step 6: Node-Side Candidate Validation

**Primary files/modules**

- `polkadot/node/core/candidate-validation/src/lib.rs`
- any adjacent node primitives/types that decode the PVF result

**Implementation work**

1. Decode the extended validation result for v4 candidates.
2. Reconstruct v10 `CandidateCommitments` from the returned outputs.
3. Keep pre-v4 candidates on the unchanged legacy commitments reconstruction
   path.
4. Continue to compare the reconstructed commitments hash with the candidate
   receipt exactly as today.

**Concrete runtime hook**

- `polkadot/node/core/candidate-validation/src/lib.rs`
  currently imports legacy `CandidateCommitments` /
  `CandidateDescriptorV2` / `CandidateReceiptV2`; this is the main place that
  needs to become version-aware for speculative candidates.

### 10.7 Step 7: Relay-Chain Runtime Enactment Rules

**Primary files/modules**

- new relay-runtime module:
  `polkadot/runtime/parachains/src/speculative_messaging.rs`
- `polkadot/runtime/parachains/src/lib.rs`
- `polkadot/runtime/parachains/src/inclusion/mod.rs`
- `polkadot/runtime/parachains/src/paras/mod.rs` or equivalent error location

**Implementation work**

1. Add persisted `ProvidesRoots`.
2. Keep `process_candidates()` as the backing / pending-availability admission
   path.
3. Extend the path that enacts pending candidates in the current relay block so
   it:
   - tracks same-block enacted provides as
     `BTreeMap<ParaId, BTreeSet<Hash>>`
   - checks each v4 `RequiresCommitment` against:
     - same-block enacted provides
     - latest persisted provides root
4. Update persisted `ProvidesRoots` only after actual enactment.
5. Add `UnsatisfiedRequires`.

**Concrete runtime hook**

- `polkadot/runtime/parachains/src/inclusion/mod.rs`
  already stores `CandidatePendingAvailability` and applies
  `enact_candidate(...)`, which is the correct place to split backing from
  enactment for speculative dependency matching.

### 10.8 Step 8: Off-Chain Networking and Source-Side History

**Primary files/modules**

- new node-side protocol module, likely under a `cumulus/client/...` crate or a
  POC-specific collator service crate
- `cumulus/client/bootnodes/src/config.rs`
- `cumulus/client/bootnodes/src/task.rs`
- `cumulus/client/bootnodes/src/discovery.rs`
- service/bootstrap wiring such as:
  - `cumulus/client/relay-chain-inprocess-interface/src/lib.rs`
  - `cumulus/client/relay-chain-minimal-node/src/lib.rs`
  - `cumulus/polkadot-omni-node/lib/src/common/spec.rs`

**Implementation work**

1. For the initial POC, add a provider/relayer process that serves a bounded
   recent history of:
   - `provides_root`
   - destination subtree root
   - subtree inclusion proof
   - ordered messages and positions
2. Add a destination-side fetcher that queries known providers before proposal.
3. Start with static/configured `ParaId -> Vec<ProviderId>` discovery for the
   POC.
4. Optionally add a native collator request/response protocol later, for
   example `/polkadot/speculative-messaging/1`, as an additional provider path.

**Concrete runtime hook**

- `cumulus/client/bootnodes`
  remains the clearest existing example for a later native request/response
  transport if we decide to add direct collator serving after the relayer-first
  POC.

### 10.9 Step 9: Choose a POC Runtime and Wire Tests Around It

For the first implementation, it is better to target one contained parachain
runtime rather than trying to wire every production runtime immediately.

**Good POC targets**

- `cumulus/parachains/runtimes/testing/penpal/src/lib.rs`
- `cumulus/parachains/runtimes/testing/rococo-parachain/src/lib.rs`
- another small testing runtime already using `XcmpQueue` and
  `register_validate_block!`

**Suggested test milestones**

1. sender runtime emits a stable cumulative `provides` root
2. receiver runtime accepts valid `SpeculativeIngress` and rejects invalid
   proofs / ordering / mixed-root-per-source cases
3. PVF returns matching v4 validation outputs
4. node-side candidate validation reconstructs the correct v4 commitments hash
5. relay-chain enactment accepts satisfied dependencies and rejects unsatisfied
   ones
6. collator networking can fetch, precheck, and inject a recent batch end-to-end

### 10.10 Scope Discipline for the Minimal POC

Keep the first implementation deliberately narrow:

- one or two testing runtimes first
- static peer configuration first
- no late-block proofs
- no pruning/GC
- no trust-domain logic
- no LLv2-dependent features

That keeps the first build focused on proving the deterministic end-to-end path:
runtime execution -> v4 commitments -> PVF replay -> relay-chain enactment ->
off-chain fetch/inject loop.

### 10.11 Implementation Decisions (Settled)

The core Phase 1 design is settled. The two upgrade-shape choices identified
below have been resolved as follows.

1. **Descriptor / receipt versioning shape — new concrete version family.**

   The current codebase centers on `CandidateDescriptorV2` /
   `CandidateReceiptV2` / `CommittedCandidateReceiptV2`. The chosen approach is
   to introduce a new concrete descriptor/receipt version family for speculative
   candidates rather than evolving the existing V2 struct. The V2 struct uses a
   reserved-byte pattern for backward-compatible version detection; overloading
   those reserved bytes with speculative fields would risk subtle
   backward-compatibility bugs where a non-speculative node parsing a
   speculative candidate could silently misinterpret fields.

   Concretely:
   - Introduce a new descriptor/receipt version (the next integer after V2 — the
     document uses "v4" as a placeholder, but the actual numeral follows
     whatever the next version is in the codebase).
   - `v10` primitives module carries the extended `CandidateCommitments` with
     `provides` and `requires` fields.
   - Legacy candidates continue to use the unchanged `v9` path.
   - This must be applied consistently across node-side candidate validation,
     relay-chain pending-availability storage, candidate hashing, and test
     helpers.

2. **Validation-result / runtime-API type transition — one upgraded wasm schema.**

   The chosen approach is one upgraded wasm validation-result schema for
   upgraded runtimes:
   - The wasm entrypoint returns one extended `ValidationResult` containing all
     legacy fields plus `provides: Option<ProvidesCommitment>` and
     `requires: Vec<RequiresCommitment>`.
   - Non-speculative candidates on upgraded runtimes return `provides: None` and
     `requires: vec![]`.
   - Version-gating happens on the node side: candidate validation branches on
     the descriptor version to know whether to expect populated speculative
     fields.
   - The relay-chain runtime API (`ParachainHost::check_validation_outputs` in
     `polkadot/primitives/src/runtime_api.rs` and
     `check_validation_outputs_for_runtime_api(...)` in
     `polkadot/runtime/parachains/src/inclusion/mod.rs`) must be evolved to
     accept the extended commitments layout. The simplest path: make it accept
     the extended type from the start, with optional speculative fields that
     are ignored for pre-speculative candidates.

This approach — one new speculative-capable descriptor/receipt version family
plus one upgraded wasm validation-result schema with version-gated
reconstruction — keeps the implementation model aligned with the rest of this
document and avoids supporting multiple partially overlapping speculative
schemas at once.

---

## 11. What's NOT In This POC

- **Late Block Proofs**: requires MMR extension proofs, PVF transformation
- **Trust domains**: all collators trust each other; no cross-domain fallback
- **Super chains**: no intra-block bidirectional messaging
- **Low-Latency v2 integration**: no acknowledgement signatures, no scheduling parent
- **Relaxed or unordered delivery semantics**: Phase 1 requires contiguous per-source
  subtree advancement; alternative delivery models are deferred
- **Message pruning or MMR garbage collection**: leaves grow indefinitely
- **Economic incentives**: no fee mechanism for relayers/collators
- **Cycle prevention**: handled by the simple rule "don't process messages from blocks
  that haven't been built yet" (a block can only depend on blocks that already exist)

See [speculative-messaging-follow-up-roadmap.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-follow-up-roadmap.md) for the post-POC production track: late block proofs, pruning, rate limits, trust domains, and migration hardening.
