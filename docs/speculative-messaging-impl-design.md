# Speculative Messaging — POC Implementation Design

Based on [speculative-messaging-design.md](speculative-messaging-design.md) (the canonical
high-level vision, tracked from [paritytech/polkadot-sdk#10449](https://github.com/paritytech/polkadot-sdk/pull/10449)).
This document is the single source of truth for **what is actually built** in the
inclusion-based speculative-messaging POC on the current codebase.

**Scope — inclusion-based messaging with Late Block Proofs.** Parachains exchange messages
off-chain; the relay chain stays the trust anchor but stores only commitments, never message
bytes. Latency is ~6–12 s (1–2 relay blocks for inclusion). This is the conservative slice of
the broader off-chain-XCMP direction; the *speculative (acknowledged)* and *super-chain* modes
need Low-Latency v2 and are out of scope (see [What's not in the POC](#whats-not-in-the-poc)).

---

## 1. Core concept

**Execute first, match later.** A parachain executes a block containing outbound messages and
commits to them; a receiver optimistically executes against that commitment; the relay chain,
at inclusion time, checks that every dependency a receiver consumed was actually committed by
its source. If the check fails the receiver's candidate is simply not enacted — no funds move,
nothing is corrupted, the collator retries. This reuses the existing parachain lifecycle rather
than adding a second execution path.

The relay chain never sees message contents — only per-`(source, destination)` commitment
hashes. All proof verification happens in the parachain runtime and is replayed
deterministically by the PVF; the relay only does cheap commitment lookups.

## 2. End-to-end workflow

```
Sender (A)                          Receiver (B)                       Relay chain
──────────                          ────────────                       ───────────
1. Execute block; outbox
   records outbound XCM into
   per-destination MMR + node
   store; emits provides as a
   UMP signal.
                                    2. Fetch MessageBatch from A
                                       off-chain; read A's provides
                                       window from the relay; build a
                                       LateBlockProof if the batch root
                                       has aged out of the window.
                                    3. Execute block; inbox re-verifies
                                       the batch against the flat
                                       commitment, dispatches XCM,
                                       emits requires as a UMP signal.
                                                                       4. Backing/PVF replays both
                                                                          blocks deterministically;
                                                                          LBPs transform the requires
                                                                          signal to the current root.
                                                                       5. Enactment: each requires
                                                                          root must be present in the
                                                                          source's provides window;
                                                                          on success the candidate's
                                                                          provides enter the window and
                                                                          its requires are recorded as
                                                                          LatestRequires (the ack).
6. Prune: collator builds a
   note_consumed inherent proving
   LatestRequires; runtime advances
   the watermark (K-deep) and prunes
   acknowledged payloads/nodes.
```

Provides/requires are produced by block execution itself (outbox/inbox runtime state), so the
PVF reproduces them for free — there is no side-channel out of the PVF to reconcile.

## 3. Deviations from the high-level design

The POC preserves the design's **principles** (relay sees only commitments, execute-first /
match-later, late-block proofs, contiguous per-source advancement) but simplifies the
commitment **representation** for a smaller, shippable slice:

| High-level design (#10449) | This POC | Why |
|---|---|---|
| Hierarchical MMR + **top-level Merkle root**; `ProvidesCommitment { root }`; `MessageBatch.subtree_inclusion_proof` | **Flat `CommitmentSet`** of `(destination, subtree_root)`; relay matches by lookup; no top-level tree or inclusion proof | §4 |
| `provides`/`requires` as **`CandidateCommitments` fields** | **UMP signals** (`UMPSignal::ProvidesRoots`/`RequiresRoots`) | §6 |
| Relay matching = **exact single-root** equality | **Provides window** membership | §5 |
| Hash unspecified | **`blake2_256`** (`SpecHasher`) | §4 |
| Custom MMR extension verification | **`mmr_lib`** inclusion + `verify_incremental` ancestry | §4, §7 |
| `SourceState.last_seen_subtree_root` (open TODO) | dropped — receiver tracks only `last_processed` | matches upstream |
| "Message pruning… NOT in POC" | **Consumed-watermark retention** (acks → pruning + backlog cap) | §8 (addition, #12350) |

These choices also resolve the design doc's open `TODO`s: `MessageBatch.subtree_root` is needed
because, under the flat commitment, it *is* the value the relay matches; `position` is part of
the leaf-hash binding; `last_seen_subtree_root` is dropped. Re-sync this table if #10449 merges.

## 4. Primitives and crate layering

Types are split to avoid a `polkadot → cumulus` dependency:

- **Relay-visible**, in `polkadot-primitives::v9` (embedded in / read by `CandidateCommitments`):
  - `CommitmentSet<N>` (`v9/commitment_set.rs`) — a canonical sorted, bounded set of
    `(ParaId, Hash)`; its manual `Decode` **rejects** out-of-order or duplicate entries, so the
    collator, PVF, and relay all produce/accept identical bytes for the same logical set.
  - `ProvidesCommitment = CommitmentSet<MAX_DESTINATIONS_PER_BLOCK>` — one
    `(destination, subtree_root)` per destination messaged this block.
  - `RequiresCommitment = CommitmentSet<MAX_SOURCES_PER_BLOCK>` — one `(source, expected_root)`
    per source consumed.
  - `UMPSignal::ProvidesRoots(..)` / `RequiresRoots(..)`.
- **Parachain machinery**, in `cumulus-primitives-spec-messaging` (re-exported via
  `cumulus-primitives-core`):
  - MMR primitives over `mmr_lib`: the domain-tagged `SpecMerge` (`mmr_lib::Merge`), peaks-only
    `Mmr`, `root_from_peaks`, and `SpecHasher = BlakeTwo256`.
  - Off-chain/runtime types (`message.rs`): `MessageBatch`, `SpeculativeIngress`,
    `LateBlockProof`, `SubtreeExtension`, `SourceState`, `OutgoingMessage`,
    `MaxSpeculativeMessageLen`.

**Flat commitment.** A sender's `provides` is the flat `CommitmentSet` — there is **no**
second-level Merkle root bagging the per-destination subtree roots. Each `(destination,
subtree_root)` is directly observable, so the relay (and any light client) matches a receiver's
`expected_root` by a simple `get(receiver)` lookup with **no inclusion proof**. The only hashing
is the per-message leaf (`OutgoingMessage::hash_leaf`) and the per-destination MMR root
(`mmr_lib` + `SpecMerge`), both `blake2_256` with domain tags.

```rust
// cumulus-primitives-spec-messaging
pub struct OutgoingMessage<MaxMsgLen: Get<u32>> {
    pub source: ParaId,        // bound into hash_leaf → prevents cross-channel replay
    pub destination: ParaId,
    pub position: u64,         // leaf index in the source's per-destination MMR
    pub payload: BoundedVec<u8, MaxMsgLen>,
}

pub struct MessageBatch {
    pub source: ParaId,
    pub source_block: Hash,
    pub source_relay_parent_number: RelayChainBlockNumber,
    pub subtree_root: Hash,        // == the (receiver, root) entry the relay matches
    pub subtree_mmr_size: u64,     // to rebuild the mmr_lib::MerkleProof
    pub messages_proof: Vec<Hash>, // combined inclusion proof over all leaves vs subtree_root
    pub messages: Vec<OutgoingMessage>,
}

pub struct SpeculativeIngress { pub batches: Vec<MessageBatch> }
```

Hashing is generic over `H` (instantiated to `SpecHasher`), so switching hash functions is a
one-line change.

## 5. Relay chain

Logic is inlined in `polkadot/runtime/parachains/src/inclusion/mod.rs` (a production build would
extract it). The relay verifies no proofs — it only does commitment bookkeeping.

**Provides window.** `LatestProvides: (source, destination) → BoundedVec<ProvidesEntry{root,
block}, MAX_PROVIDES_WINDOW_SIZE>` keeps the recent committed roots per pair (operational length
= `HostConfiguration::provides_window_size`, default 8; configuration migration v13→v14). A
`requires` entry matches if its root is **present anywhere in the window** — so a
slightly-stale-but-recent root matches with no proof. Populated at enactment from the
`ProvidesRoots` signal; matched in `sanitize_backed_candidates` and at enactment; reverted by
`evict_provides_after(revert_to)` on disputes.

```rust
fn requires_satisfied(receiver: ParaId, requires: &RequiresCommitment) -> bool {
    requires.iter().all(|(source, root)| provides_window_contains(*source, receiver, root))
}
```

**Enactment.** `process_candidates` admits backed candidates unchanged; the availability/
enactment path gates on `requires_satisfied` (against the relay-parent state only, never roots
written later in the same block — at most one extra relay block of latency when provider and
consumer land together), then appends the candidate's `provides` to the window. Matching is a
per-destination lookup, so churn from *other* destinations never forces a proof — only new
messages **to this receiver** between batch-build and enactment do.

**Acks.** `LatestRequires: (source, receiver) → (root, block)` records the latest consumed root,
written at receiver enactment alongside `update_provides`, evicted by `evict_requires_after` on
disputes. This is the acknowledgement the sender uses to prune (§8).

**Runtime APIs** (`ParachainHost`, api_version 17): `provides_root(para)`,
`provides_window(source, dest)`, `latest_requires_for_source(source)`.

## 6. Commitment transport — UMP signals

Provides/requires travel inside `CandidateCommitments.upward_messages` as
`UMPSignal::ProvidesRoots`/`RequiresRoots` (after `UMP_SEPARATOR`), read via
`CandidateCommitments::ump_signals()` / `parse_ump_signals`. They are **not** dedicated
`CandidateCommitments` fields.

Why: both approaches are covered by the same `commitments_hash`, so the benefit is **migration
safety**, not integrity. UMP signals ride an existing field, so the wire format is unchanged and
rollout is a `node_features` flag (`FeatureIndex::SpeculativeMessaging`) plus a drop-guard
(candidates with speculative signals are dropped while the feature is off; enable only past ⅔ of
validators, so older nodes never hit `TooManyUMPSignals`). A dedicated field would be a
consensus-breaking receipt-format change with a much larger blast radius (a PVF side-channel,
candidate-validation reconstruction, collation-generation mapping, ~14 construction sites).
Descriptor gating uses `CandidateDescriptorV2::new_v4()` (`version() == V4`).

## 7. Sender pallet — `pallet-speculative-outbox`

Wraps the existing XCMP source (`XcmpMessageSource`); recording happens as a side effect of
`take_outbound_messages`. Per destination it maintains:

- `OutgoingMMRState` — peaks + `leaf_count` + `mmr_size` (O(log n) on-chain state).
- `OutgoingMmrNodes: (dest, node_pos) → Hash` — a **persistent `mmr_lib` node store** (via an
  `MMRStoreReadOps`/`MMRStoreWriteOps` adapter) so proofs are generated in O(log n) from stored
  nodes, never by replaying payloads.
- `OutgoingMessages: (dest, pos) → payload`.

**Delta provides.** Only destinations whose root changed this block are committed. Recorded
destinations accumulate in `PendingProvides`, rotate into `ProvidesThisBlock` at
`on_initialize`, and `compute_provides()` emits the `CommitmentSet` over that snapshot. Unchanged
destinations are *not* re-committed — the relay's window retains their last root, and a stale
receiver bridges via a late-block proof.

**Proofs.** `outbound_messages_with_proof(dest, from, max)` builds an `mmr_lib` inclusion proof
against `subtree_root`. `generate_late_block_proof(dest, old_root)` builds a `SubtreeExtension`
(append-only ancestry proof) from `old_root` to the current root; the receiver / PVF verify it
with `MerkleProof::verify_incremental`. Both read only the requested/appended payload slice
(all ≥ the watermark, hence retained).

**Config**: `InnerXcmpMessageSource`, `SelfParaId`, `MaxBacklogPerDestination`, `RelayState`
(`RelaychainStateProvider`), `AckFinalityDepth`.

## 8. Consumed-watermark retention & storage bounds (follow-up #12350)

Payloads and MMR nodes cannot be aged out on a timer — a slow / on-demand receiver may not have
consumed yet, and a time-window prune would delete proofs it still needs. Retention is
**progress-based**, keyed on what the receiver has *provably and finally* consumed.

The acknowledgement already exists: the receiver's `requires[source]` root, verified by the
relay at enactment. The only missing piece is routing that verified root back to the sender.

**Trust model.** A receiver can only ack a root that satisfies `requires` (one the sender
actually produced); under-reporting prunes less (harmless); over-reporting is impossible. The
sole residual risk — acting on an ack a dispute later reverts — is closed by finality gating.

**Finality gate — K-deep, not a GRANDPA light client.** `LatestRequires` carries the enacting
`block`; the inherent proves the *current* value against the relay-parent state root (trusted via
validation data), and the runtime gates on `relay_parent_number - block >= K` (`K = dispute
period`, `Config::AckFinalityDepth`). A relay block buried past the dispute period cannot be
reverted, so this inherits the relay's own security with no light client.

**Ack channel — `note_consumed` inherent.** Collators cannot sign parachain extrinsics, so the
relay proof rides an **optional inherent** (same mechanism as the inbox):
`note_consumed(ConsumedAck { proof, receivers })`. The runtime verifies `proof`
(`RelayChainStateProof`) against the relay-parent root, reads each
`ParaInclusion::LatestRequires(self, receiver)`, applies the K-deep gate, then `apply_ack`.
Permissionless and safe by construction (monotonic watermark + relay-verified roots ⇒ a
malicious caller can only under-report). Collator side: `fetch_consumed_ack` queries
`latest_requires_for_source`, builds the `prove_read` proof over the `latest_requires_key` keys,
and injects via `client::inject_consumed_acks`.

**Watermark & pruning.**
- `ConsumedWatermark<dest>` (consumed prefix), `PrunedUpTo<dest>` (payload cursor),
  `NodePrunedUpTo<dest>` (node cursor).
- `CommittedRootPosition<dest, root> = (leaf_count, block)` — durable root index, pruned by the
  watermark; subsumes the former `HistoricalSubtreeState` (no fixed-block-window cap, so a slow
  receiver can ack an arbitrarily old but not-yet-consumed root). `block_number_for_subtree_root`
  reads the block tag.
- `apply_ack` advances the watermark (rejecting regressions / unknown roots) and prunes payloads,
  the root index, and MMR nodes in capped steps; `on_idle` drains the rest. The index entry *at*
  the watermark (the consumption frontier) is kept so the next late-block proof can extend from it.
- `prune_nodes` reclaims MMR nodes inside fully-consumed subtrees, keeping only the consumed
  prefix's peaks (`get_peaks(leaf_index_to_mmr_size(W - 1))`) as the witness frontier — every
  other node there is inside a completed subtree, summarised by its peak, and never read again.

**Backlog cap.** The watermark bounds *consumed* data; a receiver that never consumes would still
grow its channel. `MaxBacklogPerDestination` caps the unconsumed payload count — over-cap messages
fall back to plain HRMP and emit `BacklogCapReached`. This is the worst-case DoS guard,
independent of acks.

## 9. Receiver pallet — `pallet-speculative-inbox`

Per source it tracks `IncomingState { last_processed }` and, for the current block,
`ConsumedSourcesThisBlock`. `ingest_verified_messages(SpeculativeIngress)` is a mandatory
inherent that, for each batch: verifies `messages_proof` against `subtree_root`
(`mmr_lib::MerkleProof::<_, SpecMerge>::verify`), checks messages advance contiguously from
`last_processed + 1`, dispatches payloads through `T::XcmpMessageHandler`, and records
`(source, subtree_root)` for the block's `requires`. `get_requires_commitments()` returns the
`RequiresCommitment`; `next_expected_message_position(source)` tells the collator where to fetch
from. The inbox keeps **no** local MMR — it only verifies per-batch inclusion proofs.

## 10. PVF / `validate_block`

`validate_block` replays the block (same inherents ⇒ same outbox/inbox state). During
`on_finalize`, `parachain-system` reads `speculative_extension()` and emits
`UMPSignal::ProvidesRoots`/`RequiresRoots` into `upward_messages`. Late Block Proofs ride in
`ParachainBlockData::V2.late_block_proofs` (PoV scaffolding, not block body); the shared
`cumulus_primitives_spec_messaging::apply_late_block_proofs(signals, proofs)` rewrites
`(source, old_root) → (source, new_root)` via `SpecMerge`/`verify_incremental`. The collator and
the PVF call it on the identical signals, so the resulting `upward_messages` — and the
`commitments_hash` — agree byte-for-byte; a bad proof fails the hash check.

## 11. Off-chain networking & collator

- `OutboxQuery` async trait with two impls: `DirectOutboxClient` (in-process) and
  `RpcOutboxClient` (JSON-RPC WebSocket). `SpeculativeMessageSources` holds
  `(ParaId, Arc<dyn OutboxQuery>)`; `--speculative-sender <PARA_ID>=<WS_URL>` wires them at
  startup. (An HTTP provider would be a third `OutboxQuery` impl.)
- `fetch_ingress_for_block` (lookahead collator) fetches batches, and is **window-aware**: it
  reads `provides_window(source, dest)` and only generates a `LateBlockProof` when the batch root
  is outside the window. Returns `(SpeculativeIngress, Vec<LateBlockProof>)`.
- `fetch_consumed_ack` builds the sender-side `ConsumedAck` (§8).
- Both are injected through `Collator::create_inherent_data`
  (`inject_speculative_ingress` / `inject_consumed_acks`); `RelayChainInterface` is extended with
  `provides_root` / `provides_window` / `latest_requires_for_source` / `prove_read`.

Only the lookahead collator path is wired; the slot-based path has no speculative logic.

## 12. Feature gating & HRMP coexistence

- Speculative messaging is gated by `FeatureIndex::SpeculativeMessaging` and the v4 descriptor;
  legacy candidates carry no speculative signals and are unaffected.
- A parachain enables it per-source (configured `SpeculativeMessageSources`); other sources keep
  using HRMP. Adding the pallets requires the usual `construct_runtime!` integration.
- `SpeculativeOutboxApi` / `SpeculativeInboxApi` are bounds on the lookahead collator only;
  non-speculative parachains are unaffected unless they configure speculative sources (stub impls
  suffice). Production should make this opt-in via `ApiExt::has_api`.

## 13. Status

Implemented and tested (unit + Penpal e2e on Rococo-local): primitives + UMP-signal transport;
outbox (peaks + persistent-node MMR, delta provides, inclusion + late-block proofs); inbox
(`ingest_verified_messages`, flat-commitment verification, requires); PVF V4 extension +
`apply_late_block_proofs`; relay provides-window matching, V4 enactment, dispute eviction;
consumed-watermark retention (`note_consumed`, watermark + node-store pruning, backlog cap);
off-chain networking; Penpal integration.

Test coverage includes watermark monotonicity, prune correctness, slow-receiver retention,
revert safety (K-deep), capped-prune resume, relay `LatestRequires` overwrite/eviction, and a
property test that every un-consumed leaf stays provable after node pruning across many MMR
sizes and watermarks.

## What's not in the POC

- **Speculative (acknowledged) and super-chain (intra-block) delivery** — need LLv2 collator-ack
  signatures and/or multi-parachain collator infrastructure (a separate large project,
  [#11413](https://github.com/paritytech/polkadot-sdk/pull/11413)).
- **Trust domains** — declaring trusted peers for acknowledged delivery (needs LLv2). The POC is
  inclusion-based: trust is purely the relay's enforcement of provides/requires.
- **Relaxed/unordered delivery, economic incentives.**

## Open items

- **Zombienet validation of the collator path** — `fetch_consumed_ack` proof-key derivation,
  `prove_read` acceptance, and inherent timing are compile-checked only.
- **Eventual-delivery semantics** — bound max message age / catch-up per block; production retry
  policy (the POC has a basic resubmission loop).
- **Bounds & weights** — explicit per-channel byte limits and priced weight for
  `ingest_verified_messages` / `note_consumed`.
- **LBP beyond the retention window** — if a receiver lags past available history,
  `generate_late_block_proof` returns `None`; consider a checkpoint catch-up scheme.
- **HRMP fallback** — outside the backlog-cap path the speculative outbox is delivery-only; a
  per-destination dual-path (speculative + HRMP, receiver dedupes) would harden liveness.
- **Production commitment versioning** — keep `v9::CandidateCommitments` frozen and version the
  receipt chain rather than gating in place.

## Related documents

- [speculative-messaging-design.md](speculative-messaging-design.md) — canonical high-level
  design (Late Block Proofs, trust domains, super chains, LLv2).
- [xcmp-mmd-minimal-poc.md](xcmp-mmd-minimal-poc.md) — superseded BEEFY-anchored POC (historical).

## Appendix A — Collator per-block flow

The lookahead collator path (`cumulus/client/consensus/aura/src/collators/`), per block built:

1. **Fetch ingress** (`speculative_ingress::fetch_ingress_for_block`). For each configured source:
   read the receiver's `next_expected_message_position(source)`; read the relay
   `provides_window(source, self)`; fetch a `MessageBatch` from the sender (`OutboxQuery`) at the
   block whose root is the newest in the window. If the batch root is *outside* the window, also
   fetch a `LateBlockProof` (`generate_late_block_proof`). → `(SpeculativeIngress, Vec<LateBlockProof>)`.
2. **Fetch acks** (`speculative_ingress::fetch_consumed_ack`). As a *sender*, query
   `latest_requires_for_source(self)`, build the relay `prove_read` proof over the
   `latest_requires_key` keys, and assemble a `ConsumedAck` (`None` when there are no acks).
3. **Assemble inherents** (`Collator::create_inherent_data`). Inject the `SpeculativeIngress`
   (inbox inherent `ingest_verified_messages`) and the `ConsumedAck` (outbox inherent
   `note_consumed`) via `inject_speculative_ingress` / `inject_consumed_acks`.
4. **Execute & build.** The runtime runs the inbox inherent (verify + dispatch + record
   `requires`), the outbox records outbound XCM, and `note_consumed` prunes acknowledged data;
   `on_finalize` emits the `ProvidesRoots`/`RequiresRoots` UMP signals.
5. **Collate** (`Collator::collate` → `CollatorService`). Wrap block(s) and the
   `Vec<LateBlockProof>` into `ParachainBlockData::V2`; build the `CommittedCandidateReceipt`
   (commitments hash + descriptor + signature) and submit `(PoV, receipt)` to backing validators.

The PVF re-executes the same inherents and re-runs `apply_late_block_proofs` on the identical
signals (§10), so the `commitments_hash` matches byte-for-byte. Only the lookahead path is wired;
the slot-based collator has no speculative logic.
