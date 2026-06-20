# Speculative Messaging — Consumed-Watermark Retention (follow-up to #12350)

Follow-up to the `pallet-spec-messaging` sender work (#12350). Tracks the
acknowledgement / consumed-watermark mechanism that bounds sender-side storage,
plus an orthogonal backlog cap.

## Problem

The sender's `OutgoingMessages` (payload bytes, needed to regenerate inclusion and
late-block proofs) and the historical root index grow without bound — a DoS / state
bloat vector. Payloads cannot simply be aged out on a timer: a slow / on-demand
receiver (e.g. one authoring every few hours) may not have consumed yet, and a
time-window prune would delete the proofs it still needs. Retention must be
**progress-based**, keyed on what the receiver has *provably and finally* consumed.

## Key insight

The acknowledgement already exists: the receiver commits `requires[source] =
consumed_subtree_root` (`speculative-inbox::get_requires_commitments`), and the relay
*verifies* that root at receiver enactment (`inclusion::requires_satisfied`). The
relay does not currently persist it — the only missing piece is **routing** that
already-verified root back to the sender so it can prune.

## Trust model

The ack must originate from relay state (the relay is the only party that verified
it against the provides window). Griefing analysis:

- A receiver can only ack a root that satisfies `requires`, i.e. a root the sender
  actually produced. Forged / future roots are rejected by the relay.
- **Under-reporting** (acking an older root) → sender prunes less → harmless.
- **Over-reporting** is impossible (relay gate).
- The only residual risk is acting on an ack that later **reverts** (dispute) →
  mitigated by finality gating (below).

## Design

### Relay (`runtime/parachains/inclusion`) — Phase 1

- `LatestRequires<source, receiver> = RequiresEntry { root, block }` — overwrite (not
  a window); `block` = relay block at which the receiver's consumption was enacted.
  Written at receiver enactment alongside `update_provides`.
- `evict_requires_after(revert_to)` — mirrors `evict_provides_after`, called from the
  dispute-revert path in `paras_inherent`.
- Runtime API `latest_requires_for_source(source) -> Vec<(receiver, root, block)>` so
  the sender's collator can pull all acks for its channels in one call.

### Finality gating — K-deep, not a GRANDPA light client

Pruning is destructive, so it must not act on an ack that a later dispute could
revert. A GRANDPA light client (bridge-grandpa style: track relay authority set +
verify justifications inside the parachain runtime) would give true finality but is
overkill. Instead use **K-deep**, which inherits the relay's own security: a relay
block buried past the dispute period cannot be reverted.

Crucially this needs **no historical-root verification**: `LatestRequires` stores the
enactment `block`, the `note_consumed` extrinsic proves the *current* value against
the relay parent's state root (already trusted via validation data), and the runtime
gates on `relay_parent_number - entry.block >= K`, with `K` = dispute period.

### Sender outbox pallet — Phase 2

- `ConsumedWatermark<dest> = leaf_count` (monotonic; consumed prefix length).
- `CommittedRootPosition<dest, root> = leaf_count` — durable root→position index for
  every committed root, pruned by the watermark. Replaces `HistoricalSubtreeState`'s
  root-lookup role and removes its 256-block late-proof reach cap.
- `apply_ack(dest, acked_root)`: map root→position, `max` into the watermark (reject
  regressions), then prune `OutgoingMessages` + index below it, capped per call with
  accounted weight.

### Ack channel — Phase 3

- `note_consumed(ConsumedAck { proof, receivers })`, an **optional inherent** (collators
  cannot sign parachain extrinsics, so the codebase injects collator-built relay proofs as
  inherents — same mechanism as the inbox's `ingest_verified_messages`). The runtime verifies
  the proof against the relay-parent state root, reads `LatestRequires<self, receiver>`, gates
  K-deep on the entry's `block`, then `apply_ack`.

### Collator — Phase 4

- Runtime side (done): `note_consumed` is an optional `ProvideInherent`; `client::inject_consumed_acks`
  injects the `ConsumedAck` into inherent data before proposal.
- Orchestration (remaining, needs zombienet): a collator step that each block (or periodically)
  calls the relay `latest_requires_for_source(self)` API, builds the relay state proof over the
  `latest_requires_key` keys via `RelayChainInterface::prove_read`, and injects the `ConsumedAck`.
  Requires adding `latest_requires_for_source` to `RelayChainInterface` (+ impls) and threading the
  ack through the aura collators' `create_inherent_data`.

### Phase 5 — make pruning safe (persistent MMR node store)

**Finding that reshaped this phase:** both proof generators (`outbound_messages_with_proof`,
`generate_late_block_proof`) replayed payloads *from position 0*, so Phase 2 payload pruning would
have silently broken proof generation once any ack advanced the watermark (latent — `apply_ack` had
no production caller until Phase 4's collator wiring). So the real Phase 5 was making proof
generation independent of pruned payloads:

- `OutgoingMmrNodes<dest, node_pos> = H256` — a persistent `mmr_lib` node store, populated as leaves
  are appended (`MMRStoreReadOps`/`MMRStoreWriteOps` adapter `OutboxStore`). `MMRState.mmr_size`
  tracks the node count so the MMR reopens as `MMR::new(mmr_size, store)`.
- Both proof generators build proofs from the node store (O(log n)); payloads are read only for the
  requested/appended slice (all >= the watermark, hence retained) — never from 0. Retires the
  "O(n) replay" cost too.
- `generate_late_block_proof` maps old root -> leaf count via `CommittedRootPosition` (durable,
  watermark-pruned) instead of the 256-block `HistoricalSubtreeState`, removing the reach cap.
- `prune_consumed` keeps the root index entry *at* the watermark (consumption frontier) so the next
  late-block proof can extend from it.

Remaining cleanup (non-blocking): node-store pruning (MMR nodes currently accumulate — keep only the
witness frontier below the watermark); `HistoricalSubtreeState` is now used only by
`block_number_for_subtree_root` and could fold into `CommittedRootPosition`.

### Backlog cap — Phase 6 (parallel track)

The watermark bounds *consumed* data; a receiver that never consumes still grows its
channel unboundedly (inherent to the protocol). Add a per-destination **max-backlog
cap** (bytes and/or count): when exceeded, stop recording new speculative messages for
that dest (fall back to plain HRMP + emit an event). This bounds worst-case storage
regardless of acks and is the real DoS guard.

## Tests

- Watermark monotonicity (ack regression rejected).
- Prune correctness: payloads below watermark gone, at/above retained and still
  provable.
- Slow-receiver retention: no ack for many blocks → nothing pruned → proofs still
  generable.
- Revert safety: an ack from a reverted relay block (within K) does not prune.
- Capped prune resumes across calls; weight bounded.
- Relay `LatestRequires` overwrite + `evict_requires_after`.

## Status

- [x] Phase 1 — relay `LatestRequires` + eviction + `latest_requires_for_source` API
- [x] Phase 2 — sender watermark + `CommittedRootPosition` + capped prune (`apply_ack`,
      `prune_consumed`, `on_idle` drain)
- [x] Phase 3 — `note_consumed` extrinsic + relay-proof verification + K-deep gate
- [x] Phase 4 — runtime inherent + collator orchestration: `RelayChainInterface::latest_requires_for_source`,
      `fetch_consumed_ack` (relay query + `prove_read`), threaded through the aura collators'
      `create_inherent_data`. Compile-checked; end-to-end behaviour still wants a zombienet run.
- [x] Phase 5 — persistent MMR node store (proofs survive payload pruning; late-proof reach cap
      removed). Remaining cleanup: node-store pruning + retire `HistoricalSubtreeState`.
- [x] Phase 6 — per-destination backlog cap (`MaxBacklogPerDestination`, HRMP fallback on overflow)
