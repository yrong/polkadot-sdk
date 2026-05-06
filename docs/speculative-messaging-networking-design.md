# Speculative Messaging — Relayer / Provider Design

This document captures the **off-chain transport layer** for speculative
messaging, with the **initial POC centered on a separate relayer/provider
process**.

It is intentionally separate from
`speculative-messaging-impl-design.md` so the main implementation document can
stay focused on the consensus-critical runtime / PVF / relay-chain path.

The original source design in
`speculative-messaging-design.md` is written primarily in terms of **native
collator/peer exchange**. This document does not change that end-state
direction. It only records that, for the **minimal POC**, a separate
relayer/provider is a simpler transport realization of the same consensus
model.

## 1. Scope

The important architectural point is simple:

- the destination collator needs a **provider path** for candidate ingress data
  before block construction
- that provider is **not consensus-critical**
- correctness comes from `SpeculativeIngress` being embedded in the block body
  and re-verified during execution

So the required long-term property is:

```text
some off-chain provider
    -> supplies MessageBatch data to destination collator
    -> collator prechecks and embeds SpeculativeIngress
    -> runtime re-verifies deterministically
```

The provider can be:

- a separate relayer
- an indexer / helper process
- a source-side collator service
- a native collator request/response protocol

For the **initial POC**, the preferred path is a **separate relayer/provider
process**. That relayer-first path is the focus of this document. Native
collator transport is treated only as a later optional optimization.

For the **full end-state**, the most natural model is a hybrid one:

- **fast path**: native collator/peer exchange for low-latency trust-domain
  operation
- **fallback / catch-up path**: relayer/provider serving retained history,
  acknowledgements, and proof material for lagging or disconnected receivers

That hybrid model best matches the source design's preference for peer-based
low-latency operation while still supporting practical eventual-delivery and
proof-serving needs.

## 2. Relayer-First Provider Model

For Phase 1, relayer/provider interaction should be **pull-based**:

- destination collators ask a provider for batches they want to import
- the provider may source that data from source-side collators, its own local
  index, or other proof-bearing storage
- if no provider answers, the destination simply skips speculative ingress for
  that source in this block and can still fall back to HRMP where configured

Pull fits the current architecture well because it keeps the destination
collator in control of block selection, works cleanly with inherent-based block
construction, and avoids unsolicited cross-para state propagation into the
node.

One point should stay explicit: even when a relayer/provider is present, the
consensus-relevant block-building step is still **collator pull/select**, not
**relayer push into consensus**. The relayer/provider may retain data, serve
requests, or even notify the collator that new batches are available, but the
destination collator still decides what to fetch, what to include, and what to
embed into `SpeculativeIngress`.

## 3. Sender-Side: Batch Construction and Retention

After each finalized source block, the provider reads the post-execution outbox
state and constructs `MessageBatch` structs for each destination.

### 3.1 Runtime API for Reading Outbox State

The sender runtime exposes APIs that the provider queries after block
finalization. These are not consensus-critical (the provider is untrusted in
the consensus model), but they must return correct data for the receiver to
accept the resulting batches.

```rust
/// Runtime API on the sender parachain. Queried by the provider
/// after each finalized block to construct MessageBatch values.
#[runtime_api]
pub trait SpeculativeOutboxApi {
    /// Return the current cumulative provides root, or None if no outbound
    /// speculative messages exist yet.
    fn provides_root() -> Option<Hash>;

    /// Return the per-destination state for a specific destination.
    /// Returns (subtree_root, leaf_count) if messages exist for that
    /// destination.
    fn destination_state(dest: ParaId) -> Option<(Hash, u64)>;

    /// Return a batch of outbound messages for a destination,
    /// starting from `from_position` (exclusive).
    /// Returns up to `max_messages` messages with their positions and
    /// payloads. Messages are ordered by ascending position.
    fn outbound_messages(
        dest: ParaId,
        from_position: u64,
        max_messages: u32,
    ) -> Vec<(u64, Vec<u8>)>;

    /// Return the subtree inclusion proof showing that a given subtree_root
    /// for a given destination is included in the current provides_root.
    /// Returns None if the subtree_root is not present.
    fn subtree_inclusion_proof(
        dest: ParaId,
        subtree_root: Hash,
    ) -> Option<Vec<Hash>>;
}
```

### 3.2 Constructing a MessageBatch

For each destination that received messages in a given source block:

1. `destination_state(dest)` → get `(subtree_root, leaf_count)`
2. `subtree_inclusion_proof(dest, subtree_root)` → get the Merkle proof that
   this subtree root is in the top-level `provides_root`
3. `outbound_messages(dest, last_known_position, max)` → get the ordered
   messages this destination hasn't seen yet
4. `provides_root()` → get the cumulative root
5. Assemble:

```rust
MessageBatch {
    source: self_para_id,
    source_block: block_hash,
    source_relay_parent_number: relay_parent_number_at_execution,
    provides_root,
    subtree_root,
    subtree_inclusion_proof,
    messages: outbound_messages
        .into_iter()
        .map(|(position, payload)| OutgoingMessage { position, payload })
        .collect(),
}
```

`source_relay_parent_number` is the relay chain block number that was the
relay parent when the source block executed — available from
`frame_system::ParentNumber` or equivalent in the sender runtime.

### 3.3 Retention

The provider retains batches in a bounded in-memory cache:

- Key: `(destination_para_id, provides_root)`
- Retention window: last N finalized source blocks (e.g., N = 64) or last T
  minutes (e.g., T = 10), whichever is shorter
- Batches older than the window are pruned

The cache is purely in-memory for the POC. Persistent storage is unnecessary
since the source chain's runtime state is the canonical store and batches can
always be reconstructed from finalized blocks. If a provider restarts, it
re-indexes from the most recent finalized block.

## 4. Transport: HTTP API Shape

For the POC, a simple HTTP endpoint is sufficient. The provider listens on a
known port and serves batch data on demand.

### 4.1 Endpoint

```
GET /batches/{destination_para_id}?since_provides_root={hash}
```

Parameters:

- `destination_para_id` (path): the parachain requesting batches (used by the
  provider to select the correct destination subtree).
- `since_provides_root` (query, optional): the last provides root the receiver
  has already accepted for this source. The provider returns all batches from
  the oldest retained root up through the current root that the receiver
  hasn't processed.

If `since_provides_root` is omitted or unrecognized, the provider returns
batches starting from the oldest retained root (handles the cold-start case
where a receiver has never received messages from this source).

If no new batches exist (the receiver is up to date), returns an empty list.

### 4.2 Response

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

For the POC, JSON is simplest for debugging. SCALE encoding with a
`Content-Type: application/octet-stream` is preferred if message volumes are
high, but is not required for initial validation.

### 4.3 Provider Implementation

The provider is a separate process from the collator. It connects to the
source chain's node (either embedded or via RPC), subscribes to finalized
blocks, extracts outbox state via the runtime API, and serves the HTTP
endpoint.

```
┌──────────────────┐     runtime API      ┌──────────────┐
│ Source chain     │◄─────────────────────│ Provider     │
│ (full node)      │                      │ process      │
└──────────────────┘                      │              │
                                          │ - watches    │
                                          │   finalized  │
                                          │   blocks     │
                                          │ - builds     │
                                          │   batches    │
                                          │ - caches     │
                                          │ - serves     │
                                          │   HTTP       │
                                          └──────┬───────┘
                                                 │
                                        GET /batches/2000
                                                 │
                                          ┌──────┴───────┐
                                          │ Receiver     │
                                          │ collator     │
                                          └──────────────┘
```

## 5. Receiver-Side: Fetch, Precheck, Inject

The receiver collator integrates the fetch step into its block production
pipeline, immediately before proposal. The full verification model has two
phases:

1. **Collator-local precheck** (this section): an optimization for selecting
   valid batches before block construction. Not consensus-critical.
2. **Runtime re-verification** (in `ingest_verified_messages`): the
   consensus-critical path that validators replay during PVF execution. This
   is described in the implementation design document, section 5.2.

### 5.1 Fetch

Before building a block, the collator's inherent-data provider:

1. Iterates over configured source parachains.
2. For each source, reads the local `IncomingState[source].last_seen_provides_root`
   from the runtime state at the relay parent.
3. Queries each known provider for that source with `since_provides_root`.
4. Collects all returned batches.

The fetch is a blocking step in the collator's proposal loop. For the POC, a
simple sequential query loop is fine. Timeouts (e.g., 2 seconds per provider)
prevent hanging if a provider is unreachable.

### 5.2 Precheck

Each fetched batch goes through the collator-local precheck described in the
implementation design (section 5.2). The precheck:

1. Verifies the `subtree_inclusion_proof` against `provides_root` using the
   destination-keyed leaf format:
   `leaf_hash = keccak256(SCALE(destination_para_id, subtree_root))`
2. Verifies message positions are consecutive from the receiver's last
   processed position for that source.
3. Reconstructs the local subtree MMR by inserting each message hash and checks
   the resulting root matches `subtree_root`.
4. Updates the collator-local `IncomingState` snapshot.

Batches that fail precheck are discarded. Only valid batches are candidates
for inclusion.

### 5.3 Selection

The collator may receive more valid batches than it can include in one block.
Selection policy for the POC:

- Batches are ordered by source priority (configurable) and then by age
  (oldest first).
- The collator selects batches greedily until block weight or size limits are
  met.
- At most one distinct `provides_root` per source per block (enforced by the
  runtime's `MultipleRootsPerSourceInOneBlock` check).

### 5.4 Injection

Selected batches are encoded into `SpeculativeIngress` and injected into
`InherentData` under `SPECULATIVE_INGRESS_IDENTIFIER`. The runtime-side
`ProvideInherent` implementation decodes them and constructs the
`ingest_verified_messages` call.

Conceptually:

```rust
// Collator-side before block proposal
let mut inherent_data = other_inherent_providers.create_inherent_data().await?;

let mut ingress = SpeculativeIngress { batches: Vec::new() };
for source in configured_sources {
    let batches = fetch_and_precheck(source, &providers).await?;
    ingress.batches.extend(batches);
}

inherent_data.put_data(SPECULATIVE_INGRESS_IDENTIFIER, &ingress)?;
```

## 6. Provider Discovery

The initial POC does not need dynamic collator discovery. It starts with static
provider configuration.

```rust
/// Mapping: ParaId -> provider endpoints that can serve
/// speculative batches for that source parachain.
struct ProviderDiscovery {
    providers: HashMap<ParaId, Vec<ProviderId>>,
}

impl ProviderDiscovery {
    fn providers_for_para(&self, para_id: ParaId) -> Vec<ProviderId> {
        self.providers.get(&para_id).cloned().unwrap_or_default()
    }
}
```

For the POC, a simpler approach suffices: hardcode or configure provider
endpoints for each source chain in a config file. Example:

```toml
[speculative_messaging_providers]
1000 = ["http://provider-a.example:9100", "http://provider-a-fallback.example:9100"]
2000 = ["http://provider-b.example:9100"]
```

That means the minimal implementation path is:

1. Start with a static `ParaId -> Vec<ProviderEndpoint>` configuration.
2. Add native collator discovery / request-response only after the end-to-end
   fetch/ingress path works.

## 7. Error Handling and Retry

```text
For each source chain in the configured set:
  1. Try to connect to any known provider
  2. Request MessageBatch data with since_provides_root cursor
  3. If response received -> precheck each batch locally -> encode accepted
     batches into SpeculativeIngress
  4. If timeout or error -> log warning -> SKIP this source for this block
     (The block is built without messages from this source.
      Next block will retry. No provides/requires for this source.)
```

Skipped sources are retried in the next block as long as the relevant data
remains retrievable from some relayer/provider's retained local history.

For the **full design**, eventual-delivery guarantees additionally require the
follow-up work on:

- retention windows
- bounded catch-up behavior
- late-block-proof fallback
- resubmission / retry policy

## 8. Boundedness and Failure Modes

### 8.1 Catch-up window

The provider retains a sliding window. A destination that falls behind by more
than the retention window cannot fetch the missing batches. This is an expected
POC limitation:

- The receiver's precheck rejects batches where `source_relay_parent_number`
  is more than a configurable threshold behind the current relay parent.
- An unservable gap triggers a warning log and the receiver skips speculative
  messages from that source for this block.
- Robust catch-up requires Late Block Proofs, deferred to Phase 2.

### 8.2 Provider failure

If all providers for a source are unreachable, speculative messages from that
source are skipped for this block. The collator logs a warning and continues
with HRMP messages (if configured for that channel). No block production is
ever blocked by networking failures.

### 8.3 Stale batches from forked source blocks

If a provider serves a batch that is technically valid (proofs verify, messages
are consecutive) but the corresponding sender candidate was never included on
the relay chain (e.g., the sender forked), the receiver block's
`RequiresCommitment` will reference a `provides_root` that never appears in
`ProvidesRoots`. At enactment time, the relay chain will reject the receiver
candidate with `UnsatisfiedRequires`, and the candidate will not be included.

This is correct behavior — the receiver block had an unsatisfiable dependency
and must be rebuilt. The receiver collator can reduce the chance of this by
only fetching batches for finalized source blocks, but finalized does not mean
included. This is a fundamental race condition that Late Block Proofs address
more gracefully.

### 8.4 Malicious provider

The transport is untrusted. The receiver re-verifies all proofs in the runtime
during `ingest_verified_messages`. A malicious provider can:

- **Serve invalid proofs**: runtime verification rejects them.
- **Serve stale or out-of-order batches**: precheck or runtime continuity check
  rejects them.
- **Withhold batches**: receiver skips speculative messages from that source
  for this block. No worse than HRMP latency.
- **Serve batches for a forked source block**: relay-chain enactment rejects
  the receiver candidate with `UnsatisfiedRequires`.

No new trust assumptions are introduced.

## 9. Tradeoffs of a Relayer / Provider-First POC

The relayer/provider-first approach is intentionally a **practical POC
simplification**.

In this model, the relayer/provider typically does two jobs:

1. ingest and retain a bounded recent history of batch/proof material from the
   source side
2. serve that retained data to destination collators on request

That does add an extra node-side component, but it still simplifies the first
end-to-end implementation because it avoids coupling the POC to a brand-new
native collator networking path.

### Advantages

- simpler transport implementation for the first POC
- easier debugging and observability
- natural place to keep bounded recent message/proof history
- clear separation between consensus logic and transport logic

### Tradeoffs

- a single provider can become an availability or latency bottleneck
- the provider adds an extra operational component
- this is more centralized than the fuller peer-native end-state
- an extra hop may be worse for strict low-latency trust-domain operation

Importantly, this is **not** a consensus-safety bottleneck. If a provider is
slow or unavailable:

- the destination collator may fail to fetch speculative batches for that block
- the block can still be produced without speculative ingress
- consensus remains correct

So the main downside is degraded speculative-message liveness or latency, not
incorrect state transition.

That is why this tradeoff is acceptable for the minimal POC, but not likely the
best standalone long-term architecture.

## 10. Optional Native Collator Transport Later

Direct collator request/response is still useful as a later native fast path,
but it is not required for the initial POC.

In the fuller architecture, this native path is best understood as the
preferred **low-latency fast path**, while the relayer/provider path remains
useful for:

- lagging destinations
- bounded historical replay
- proof serving
- acknowledgement forwarding
- resilience when direct peer connectivity is weak or temporarily unavailable

When added, it should reuse existing node-side patterns such as:

- registering request/response protocols through the network service
- using `NetworkService::start_request(...)` for point-to-point queries
- wiring the protocol receiver into a background async task

One possible native protocol shape is:

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
of a small request/response protocol wired into the current networking stack.

## 11. What's Deferred

- Native collator request/response protocol (`/polkadot/speculative-messaging/1`)
- Provider discovery beyond static configuration
- Provider health scoring and automatic failover
- Batch de-duplication across multiple providers
- Message-level gossip or broadcast
- Retry and backpressure for slow destinations
- Catch-up for destinations that fall behind the provider retention window
  (requires Late Block Proofs)
- Persistent provider-side batch storage (in-memory cache is sufficient for POC)
