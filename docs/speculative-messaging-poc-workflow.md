# Speculative Messaging — Minimal POC Workflow Walkthrough

This document is a **companion guide** to
[speculative-messaging-impl-design.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-impl-design.md).

It does not replace the implementation design. Instead, it explains the
minimal speculative-messaging POC as a practical step-by-step workflow and
highlights why each step is understandable and achievable on the current
codebase.

## 1. Scope

This walkthrough covers the same **Phase 1 / minimal POC** boundary as the main
implementation doc:

- inclusion-based speculative messaging only
- deterministic ingress through the block body
- no late-block proofs yet
- no acknowledgement-based Low-Latency v2 integration yet
- no full eventual-delivery guarantees yet

So this is the "happy path" implementation:

- sender and receiver remain reasonably close in block production / inclusion
- the receiver can still match the sender root it built against
- off-chain transport only fetches data; consensus remains fully deterministic

For the full design, especially on lagging or core-on-demand destinations,
follow-up work is still required. See:

- [speculative-messaging-follow-up-roadmap.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-follow-up-roadmap.md)

## 2. The Core Idea

The POC keeps one critical rule:

> nothing consensus-critical happens only off-chain

Off-chain logic may fetch, cache, and precheck batches, but validators never
trust that by itself. The actual consensus path is:

1. the sender runtime executes and produces a `provides` root
2. the receiver collator fetches candidate ingress data from a relayer/provider
3. the receiver embeds that ingress into the block body
4. the receiver runtime re-verifies and executes it
5. the PVF replays the same block deterministically
6. the relay chain checks `requires` against `provides` at enactment

That is what makes this design practical on the current architecture: it reuses
the existing parachain lifecycle instead of inventing a second execution path.

## 3. End-to-End Workflow

### Step 1: Sender block execution

On the source parachain, the collator builds a block normally.

During runtime execution:

- outbound sibling-parachain XCM is produced through the existing path
- a speculative outbox wrapper records the payloads into per-destination MMR or
  subtree state
- the sender's cumulative top-level `provides` root becomes derivable from the
  resulting runtime state

Why this is achievable:

- this is ordinary deterministic runtime logic
- it does not require a new protocol stage
- it fits the same sender-side interception pattern already used by related
  outbound message helpers

Practical result:

- after execution, the sender has a root that can later become
  `ProvidesCommitment.root`

### Step 2: Source-side batch/proof retention

After the sender block exists, some source-side component needs to retain a
bounded recent history of data that receivers can later fetch:

- the sender `provides_root`
- the destination subtree root
- the subtree inclusion proof
- the ordered messages and positions

For the minimal POC, the simplest approach is a separate relayer/provider
process.

Why this is achievable:

- this is node-side indexing and serving, not consensus logic
- it can be implemented without changing relay-chain or runtime semantics
- it is easier to prototype than native collator-to-collator networking

Practical result:

- destination collators have somewhere to fetch valid `MessageBatch` data from

### Step 3: Receiver collator fetches and prechecks

Before proposing its own block, the destination collator fetches recent batches
from a provider.

It then performs a **local precheck**:

- verify the subtree inclusion proof
- verify message positions are consecutive
- verify local subtree continuity against its local snapshot

This precheck is only for efficiency and block selection.

Why this is achievable:

- collators already gather non-consensus inputs before proposal
- the precheck can run entirely outside runtime storage
- it helps avoid proposing obviously bad or stale batches

Practical result:

- the receiver collator chooses which speculative batches are worth embedding

### Step 4: Receiver embeds `SpeculativeIngress`

The receiver collator converts the accepted batches into:

- `SpeculativeIngress`

That ingress is inserted into `InherentData`, and the runtime constructs an
inherent-style call such as:

```rust
SpeculativeInbox::ingest_verified_messages { ingress }
```

This call is placed into the block body.

Why this is achievable:

- Cumulus already has the inherent-data pipeline
- parachain-system already uses this general execution shape
- we are reusing the existing "collator gathers input, runtime executes it"
  pattern

Practical result:

- speculative ingress becomes a normal deterministic block input

### Step 5: Receiver runtime re-verifies and dispatches

When the receiver block executes, the runtime re-verifies each embedded batch
against real on-chain state.

For each batch, it:

- re-verifies the subtree proof
- re-verifies message ordering and continuity
- updates durable incoming state
- records which source root was actually consumed in this block
- dispatches the payloads through the existing batch XCMP handler

Why this is achievable:

- this is a standard pallet/inherent design
- it uses runtime storage and dispatch exactly like other deterministic runtime
  behavior
- it adapts to the existing XCMP batch handler rather than inventing a fake
  single-message API

Practical result:

- imported speculative messages become part of normal runtime execution
- validators can later replay exactly the same behavior

### Step 6: Collator assembles `provides` and `requires`

After execution, the collator reads the speculative outputs produced by the
runtime:

- sender-side cumulative `provides`
- receiver-side `requires` derived from what this block actually consumed

Those outputs are inserted into the speculative part of candidate commitments.

Why this is achievable:

- collators already gather post-execution outputs when assembling candidate
  data
- this only adds two more commitment outputs
- the values are derived from executed runtime state rather than guessed
  off-chain

Practical result:

- the candidate now carries the relay-visible speculative commitments

### Step 7: PVF replays the same block deterministically

Backing validators execute the wasm PVF over the candidate's `block_data`.

Because `SpeculativeIngress` was embedded in the block body, validators replay:

- the same ingress call
- the same runtime verification logic
- the same state updates
- the same resulting `provides` and `requires`

Why this is achievable:

- PVF execution already replays parachain blocks deterministically
- we are reusing block-body execution, not introducing hidden PVF-only inputs
- the only required extension is returning speculative outputs in the validation
  result

Practical result:

- speculative messaging stays consensus-safe because validators see exactly what
  the collator proposed

### Step 8: Node-side candidate validation reconstructs commitments

After the PVF returns, node-side candidate validation reconstructs the
candidate commitments from the validation outputs and checks their hash against
the candidate receipt.

For speculative candidates, reconstruction includes:

- `provides`
- `requires`

Why this is achievable:

- the node already reconstructs commitments from validation outputs today
- speculative messaging extends that path instead of replacing it
- the main required change is version-aware decoding and reconstruction

Practical result:

- speculative commitments are validated using the same hash-checking model as
  existing candidate commitments

### Step 9: Relay-chain enactment checks dependency satisfaction

At relay-chain level, speculative dependency matching happens at actual
enactment time, not merely when the candidate first enters pending
availability.

For each receiver candidate, the relay chain checks every
`RequiresCommitment` against:

- same-block enacted provides from source parachains
- or the latest persisted provides root for that source

If enactment succeeds, the relay chain updates the persisted latest
`ProvidesRoots[source]`.

Why this is achievable:

- the relay-chain runtime already distinguishes backing/pending-availability
  from actual enactment
- speculative messaging adds one more enactment rule and a small storage map
- the relay chain still only compares hashes; it does not need to replay proofs

Practical result:

- cross-chain dependencies become relay-visible inclusion rules

## 4. Why This Fits the Current Codebase

This POC is realistic because each piece maps onto an existing seam:

- sender-side tracking is ordinary runtime logic
- ingress uses the existing inherent mechanism
- runtime execution remains the source of truth
- PVF still just replays block execution
- node-side candidate validation still reconstructs and hash-checks commitments
- relay-chain logic still enforces inclusion-time rules
- off-chain networking stays outside consensus

So the POC is not "simple", but it is structurally aligned with how the current
parachain pipeline already works.

## 5. What Is Still Hard

The design is implementation-ready for a minimal POC, but a few areas still
need careful engineering:

### 5.1 Versioning and ABI plumbing

The speculative path needs a consistent cross-layer upgrade for:

- candidate descriptor / receipt versioning
- candidate commitments schema
- validation-result encoding
- node-side commitments reconstruction

This is achievable, but it has to be done carefully across all layers.

### 5.2 Sender/receiver runtime integration

The runtime changes are straightforward conceptually, but still require new
sender-side and receiver-side pallets or helper modules, plus runtime APIs.

### 5.3 Relay-chain enactment integration

The speculative rule must be attached to the actual enactment path, not just
backing-time admission. That distinction is already understood in the current
design and must be preserved in implementation.

### 5.4 Scope discipline

This walkthrough only describes the minimal happy-path POC.

It does **not** yet solve:

- late-block proofs
- robust operation for lagging/core-on-demand destinations
- full eventual-delivery guarantees
- acknowledgement-based low-latency trust-domain behavior

Those remain valid follow-up work, not blockers for the Phase 1 POC.

## 6. Practical Conclusion

The current minimal POC is:

- understandable
- internally coherent
- achievable on the current codebase

It works because the design keeps the consensus model simple:

- off-chain transport only fetches data
- block-body ingress carries the real input
- runtime re-verifies deterministically
- PVF replays deterministically
- relay-chain enactment enforces commitment matching

That is the right boundary for a first implementation slice.

## 7. Related Documents

- [speculative-messaging-design.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-design.md)
- [speculative-messaging-impl-design.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-impl-design.md)
- [speculative-messaging-networking-design.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-networking-design.md)
- [speculative-messaging-follow-up-roadmap.md](/Users/yangrong/Projects/polkadot-sdk/docs/speculative-messaging-follow-up-roadmap.md)
