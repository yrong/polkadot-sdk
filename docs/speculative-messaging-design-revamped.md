# Speculative Messaging

## Design Document

| Field | Value |
|-------|-------|
| **Authors** | eskimor |
| **Status** | Draft |
| **Version** | 0.2 |
| **Related Designs** | Low-Latency Parachains v2 ([PR #11413](https://github.com/paritytech/polkadot-sdk/pull/11413) — not in this branch) |

---

## Table of Contents

1. [Introduction](#introduction)
2. [Motivation](#motivation)
3. [Goals](#goals)
4. [Non-Goals](#non-goals)
5. [Background](#background)
6. [Solution Overview](#solution-overview)
7. [Detailed Design](#detailed-design)
   - [Parachain Communication](#parachain-communication)
   - [Message Accumulators](#message-accumulators)
   - [Candidate Commitments](#candidate-commitments-verified-by-relay-chain)
   - [Parachain Runtime State](#parachain-runtime-state-internal)
   - [Off-Chain Communication](#off-chain-communication-between-collators)
   - [Relay Chain Matching](#relay-chain-matching)
   - [Late Block Proofs](#late-block-proofs)
   - [Proof Size Considerations](#proof-size-considerations)
   - [Acknowledgement Extensions](#acknowledgement-extensions)
   - [Cycle Prevention](#cycle-prevention)
   - [Super Chains](#super-chains)
8. [Trust Domains](#trust-domains)
9. [Censorship Considerations](#censorship-considerations)
10. [Comparison with Alternatives](#comparison-with-alternatives)
11. [Phasing / MVP Scope](#phasing--mvp-scope)
12. [Implementation Considerations](#implementation-considerations)
13. [Security Analysis](#security-analysis)

---

## Introduction

Speculative Messaging introduces a new cross-chain messaging mechanism for
Polkadot that replaces HRMP with a more scalable, lower-latency alternative. By
using cryptographic accumulators (such as Merkle Mountain Ranges) to commit to
messages off-chain and having the relay chain enforce these commitments at
inclusion time, we achieve:

- **Lower latency**: Messaging at parachain block times rather than relay chain
  block times
- **Better scalability**: Off-chain message passing with on-chain commitment
  verification
- **Compatibility with Low-Latency v2**: Works seamlessly with older relay
  parents

This design builds upon and complements the Low-Latency Parachains v2 design.
While that design introduces older relay parents (for relay chain fork immunity), it would normally increase messaging latency. Speculative Messaging
solves this problem entirely by decoupling message passing from relay parents.

---

## Motivation

### The Problem with Current Messaging (HRMP)

Current cross-chain messaging in Polkadot (HRMP) relies on the relay chain as
the coordination layer:

1. Parachain A produces a block that sends a message
2. The block gets backed and included on the relay chain
3. The relay chain stores the message in its state
4. Parachain B observes the message via its relay parent
5. Parachain B can now receive the message in its next block

This process takes a minimum of 2-3 relay chain blocks (~12-18 seconds) under
ideal conditions. With Low-Latency v2 recommending finalized relay parents (for
fork immunity), this latency would increase significantly if we relied on HRMP.

Additionally, HRMP has scalability concerns:
- Messages flow through relay chain state
- Relay chain must store and manage message queues
- Every validator processes message routing

### Why This Matters

For many cross-chain use cases, 12-18+ second messaging latency is prohibitive:

- **DeFi**: Cross-chain arbitrage, liquidations, and atomic swaps require fast
  execution
- **Gaming**: Interactive cross-chain gameplay needs sub-second responses
- **User Experience**: Multi-chain dApps feel sluggish when every cross-chain
  action takes 20+ seconds

### The Opportunity

By moving message coordination off-chain and using cryptographic commitments for
verification, we can:

1. Achieve messaging latencies comparable to parachain block times
2. Remove message data from relay chain state entirely
3. Build super chains

---

## Goals

1. **Replace HRMP**: Provide a complete replacement for HRMP that is faster and
   more scalable.

2. **Low-Latency Messaging**: Reduce cross-chain messaging latency to parachain
   block times for chains in the same trust domain.

3. **Intra-Block Messaging**: Enable "super chains" (multiple parachains run by
   the same collator set) to exchange messages within the same block production
   cycle.

4. **Off-Chain Scalability**: Keep message data off the relay chain; only
   commitments are verified on-chain.

5. **Graceful Degradation**: When speculative messaging acknowledgements aren't
   available, fall back to inclusion-based commitment matching (still faster
   than HRMP).

6. **Horizontal Scaling**: Maintain Polkadot's horizontal scaling
   properties—full nodes only need to follow chains they care about.

---

## Background

### Relay Parent and Message Context

In current Polkadot, a parachain block's relay parent determines its "view" of
the world, including what messages are available to receive.

With Low-Latency v2, we decouple scheduling from the relay parent, allowing
older (finalized) relay parents for fork immunity. This means the relay
parent—and thus any HRMP-based message receiving context—could be significantly
behind the current relay chain head, making HRMP impractical.

### Low-Latency v2

Low-Latency v2 introduces acknowledgement signatures where collators commit to
blocks becoming canonical and decoupling of candidates from parachain blocks. We
build on those features in this design.

### Merkle Mountain Ranges (MMR)

An MMR is an append-only authenticated data structure that allows:
- Efficient appending of new elements
- Compact proofs of inclusion for any element
- Compact proofs connecting any two points in the accumulator's history

This makes MMRs ideal for accumulating messages over time while allowing
efficient proofs for late-arriving blocks.

---

## Solution Overview

Instead of routing messages through relay chain state, we:

1. **Accumulate Messages**: Each chain maintains an MMR of all outgoing messages
   to all destinations.

2. **Emit Commitments**: Sending chains emit a "provides" commitment (the MMR
   root); receiving chains emit "requires" commitments (per source chain).

3. **Off-Chain Coordination**: Collators exchange messages directly, without
   relay chain involvement.

4. **Relay Chain Enforcement**: At inclusion time, the relay chain verifies that
   all "requires" are satisfied by corresponding "provides".

5. **Late Block Proofs**: When blocks arrive at different times, the late block
   includes a proof in its POV connecting the current provides to its older
   requires.

```
┌──────────────────────────────────────────────────────────────────────┐
│                     Current HRMP Flow (Slow)                         │
├──────────────────────────────────────────────────────────────────────┤
│  Chain A Block    →    Relay Chain     →    Relay Chain  →  Chain B  │
│  (sends msg)           stores msg           State lookup    receives │
│                        ~12-18s total                                 │
└──────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│                  Speculative Messaging (Fast)                       │
├─────────────────────────────────────────────────────────────────────┤
│  Chain A Block    →    Off-chain     →    Chain B Block             │
│  (provides: MMR)       msg passing        (requires: A's MMR pos)   │
│                        ~block time                                  │
│                                                                     │
│  Relay chain only verifies: provides(A) satisfies requires(B)       │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│              Late Block with Proof (Fallback)                       │
├─────────────────────────────────────────────────────────────────────┤
│  Chain A Block N   ...time passes...   Chain A Block N+K            │
│  (provides: R_N)                       (provides: R_{N+K})          │
│                                                                     │
│  Chain B Block M (late, requires A at position P from block N)      │
│  POV includes: proof that R_{N+K} extends R_N covering position P   │
│  Commitment includes: matched requires with proof reference         │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Detailed Design

### Parachain Communication

Parachain collators operating on different peer-to-peer (P2P) networks need a way to exchange messages off-chain.
The relay chain only processes message commitments, not the messages themselves. Direct communication between
collators of different parachains is not possible due to different genesis hashes and sync protocols.

To enable off-chain communication between collators, a dedicated P2P network is created.
This **Speculative Messaging Network** includes collators from all parachains that opt into
speculative messaging.

Alternative architectures were considered:
- Routing through relay chain peers: Adds unnecessary load and stress on the relay chain,
  as well as new protocols for message exchange between collators.
- Spawning a dedicated network backend for each parachain: Highly resource-intensive and doesn't scale
  well with the number of parachains.

By deploying a single network backend for the entire Speculative Messaging Network, we keep the relay chain side
changes to a minimum (needed for JAM compatibility) and we can leverage the existing bootnodes-on-DHT
mechanism (RFC 08) for collator discovery.

The **Speculative Messaging Network** exposes the protocols below. How each protocol's *wire name* is derived matters
(see [Spec-Msg Network Construction](#spec-msg-network-construction)): discovery/notification names are derived from the
network's `genesis_hash`, whereas request/response names are literal strings.

- **Kademlia DHT** (peer discovery): the primary protocol name is `/<spec-msg-domain>/kad`, derived from the shared *domain separator* passed as the network's `genesis_hash` — sharing that constant is precisely what makes all participants form **one** DHT. `/spec-msg/kad` additionally exists as the `protocol_id`-derived legacy alias.
- **Identify and Ping** (peer addresses, keep-alive): substrate/libp2p standard protocols (ping is the standard `/ipfs/ping/1.0.0`); they are not themselves spec-msg-namespaced. Isolation is enforced by the Kademlia name above and the notification-handshake genesis check, not by these.
- **Speculative Messaging Protocol — `/spec-msg/exchange/1`**: a request/response protocol with a *literal* name (request/response names are not auto-prefixed). Specified in [Message Exchange Protocol](#message-exchange-protocol-spec-msgexchange).
- **Light Client Request-Response — `/spec-msg/light/2`**: a request/response protocol with a literal name. Initially for fetching authority discovery keys, but it is the general light-client read primitive: it is intended to generalize to **messages** (proven against the sender's authority-authored block) and, in a later phase, **acknowledgment signatures**. The `/2` mirrors Substrate's light-client protocol versioning.

Parachains that don't participate in speculative messaging can simply ignore the Speculative Messaging
Network and not register themselves in the DHT. (Being in a *different trust domain* from a peer is not the same as
non-participation: such chains still exchange over this network, just in inclusion-based mode.)

#### Discovery happens in two ordered layers

Discovery has two distinct layers that must not be conflated — one gets a collator *into* the network, the other finds
*specific peers once inside* — and the second strictly depends on the first:

1. **Network entry (relay-DHT bootnodes).** A fresh node has an empty Kademlia routing table and can issue *no* DHT
   query at all. It first discovers spec-msg **bootnodes** via the relay chain DHT — the extended `/paranode` flow
   ([Bootnodes for the Speculative Messaging Network](#bootnodes-for-the-speculative-messaging-network)). This is the
   only step that bootstraps from a network the collator is *already* in (the relay network, via its embedded relay
   node), and it populates the routing table so the spec-msg DHT becomes usable. Bootnodes are *unauthenticated* entry
   points — dialed only to bootstrap Kademlia, not trusted.
2. **In-network resolution (per-chain, authenticated).** *Once a member*, the collator resolves a specific foreign
   chain's collators: `GET_PROVIDERS` under that chain's `para ID || epoch randomness` (entry points) → a light-client
   proof of its `AuthorityDiscovery::Keys` ([Trust Model](#trust-model-for-collators)) → `GET_VALUE(sha256(audi_key))`
   for the authenticated addresses.

The ordering is a hard dependency: **every step in layer 2 is a DHT operation, so it only works after layer 1 has
joined the network.** The light-client read in particular must be served by a node holding the foreign chain's state
(its collator) *on the spec-msg network* — reachable only once you are a member. So the relay-DHT bootnode step is not
redundant with the light-client bootstrap; it is its prerequisite. The subsections below cover layer 1 (Bootnodes),
then layer 2 (Speculative Messaging Network + Trust Model).

(This is the *runtime* ordering of discovery. It is orthogonal to the *build-time* split in
[Discovery Architecture: Two Layers](#discovery-architecture-two-layers), which divides the implementation into the
shared-network construction and the cross-parachain lookup task — layer 2 here is what that task performs.)

#### Bootnodes for the Speculative Messaging Network

The architecture leverages the existing bootnodes on DHT mechanism on the relay chain side. 
For more info, see [RFC 08](https://github.com/polkadot-fellows/RFCs/blob/main/text/0008-parachain-bootnodes-dht.md).

Typically, relay chain peers of parachains advertise themselves as providers under the key `para ID || epoch randomness`
in the relay chain DHT. Only the 20 closest peer IDs to this key are kept as providers, and the provider set is updated on every epoch change.

> **Note on key construction.** RFC 08 describes this key as `sha256(concat(scale_compact(para ID), epoch randomness))`,
> but the Cumulus implementation (`cumulus/client/bootnodes`, `BootnodeAdvertisement::epoch_key`) uses the raw
> concatenation `scale_compact(para ID) || epoch randomness` without hashing — which is the `para ID || epoch randomness`
> form referenced above. The two diverge; this design follows the implementation's raw-concat convention for the
> per-parachain key.

**Reusing existing bootnode entries — no separate spec-msg registration.** Rather than maintaining a *second* relay-DHT
provider registration specifically for spec-msg bootnodes, we reuse the per-parachain bootnode entries that every
parachain node already advertises (above). All of this runs on the **relay network/DHT** — which is necessary, since the
spec-msg network does not exist yet at bootstrap time. The only change is one backwards-compatible field added to the
`/paranode` *response*; the request is unchanged, so there is no second input form to disambiguate.

This assumes spec-msg opt-in is **chain-wide**: if a parachain participates, its bootnodes serve spec-msg addresses.
(A per-collator opt-in would still work, but a queried bootnode might return no spec-msg addresses, requiring the caller
to try another.)

**Schema change.** The `/paranode` response (`cumulus/client/bootnodes/src/schema`, proto2) gains one optional field:

```proto
message Response {
    required bytes  peer_id        = 1;
    repeated bytes  addrs          = 2;  // parachain-side addresses (unchanged)
    required bytes  genesis_hash   = 3;
    optional string fork_id        = 4;
    repeated bytes  spec_msg_addrs = 5;  // NEW: spec-msg listen addresses; empty if not participating
}
```

proto2 ignores unknown fields, so old responders return nothing in field 5 and old requesters ignore it — no protocol
version bump.

**Responder side** (every participating parachain node) — almost entirely reuse:
1. *(unchanged)* It already advertises in the relay DHT under `para ID || epoch randomness` (`BootnodeAdvertisement::epoch_key`) and already serves `/paranode`.
2. **[new]** Thread the node's spec-msg `NetworkService` (or its resolved listen addresses) into the advertisement task, add a `spec_msg_addresses()` builder alongside the existing `paranode_addresses()`, and populate field 5. Non-participants leave it empty.

**Requester side** (a collator joining the spec-msg network): iterate the parachains it actually communicates with —
its **HRMP channel peers**, read from relay state (`HrmpIngressChannelsIndex[my_para]` for sources it receives from,
plus `HrmpEgressChannelsIndex` for destinations; see [Establishing Trust](#establishing-trust)) — and for each perform
the standard RFC 08 lookup it already relies on (the existing `discovery.rs` client path, reading the new field):
1. Obtain epoch randomness via `BabeApi_currentEpoch` (and `next_epoch`, to survive rotation) and form the key `para ID || epoch randomness`.
2. `GET_PROVIDERS` on the relay DHT for that key → up to 20 relay-side peer IDs.
3. Send `/paranode` (payload = that `para ID`, unchanged) to each, and read `spec_msg_addrs` from the response; empty ⇒ skip (non-participant).

The collected addresses seed the spec-msg network (see [Spec-Msg Network Construction](#spec-msg-network-construction)).
They only need to yield **one** reachable spec-msg peer: once connected, the spec-msg network's own Kademlia DHT
takes over for steady-state discovery, so the relay-DHT step is purely the entry ramp, not on the hot path.
Because this piggybacks on the per-parachain bootnode advertisement that exists regardless of spec-msg, there is no new
relay-DHT registration to seed or maintain, and no separate global key — which also removes the bootstrapping-seed
question (the participant set *is* the seed, via entries every parachain already publishes).

#### Speculative Messaging Network

Once a collator obtains the bootnode list from the relay chain, it spawns a dedicated network backend for the 
Speculative Messaging Network and connects to the bootnodes. Because the network connects collators from
*all* parachains, collators from Parachain A must establish communication with collators from Parachain B.

Peers register themselves in the Speculative Messaging DHT as providers under the key `para ID || epoch randomness`,
exactly as parachain bootnodes register in the relay chain DHT (the `para ID || epoch randomness` form from RFC 08),
using the `ADD_PROVIDER` mechanism. Note this is a registration *within the spec-msg network's own DHT* (distinct from
the relay-chain bootnode discovery above). The epoch randomness is obtained from the relay chain (`BabeApi_currentEpoch`)
so that the key is deterministic across all participants and rotates each epoch.
This allows collators to quickly discover the 20 closest peers for a given parachain. These peers serve as explicit
entry points: they answer the light-client storage-proof requests (see [Trust Model](#trust-model-for-collators))
used to learn that parachain's authority discovery keys.

Separately from the `ADD_PROVIDER` mechanism, collators publish their `SignedCollatorAuthorityRecord` records into the
DHT using the `PUT_VALUE` Kademlia mechanism, keyed by `sha256(authority_discovery_public_key)`. This lets peers fetch
and *verify* the addresses of other collators. This mechanism mirrors authority discovery on the relay chain for
validators (`substrate/client/authority-discovery`), and the record below is structurally Substrate's
`SignedAuthorityRecord` (the authority-discovery `dht-v3` schema), so the existing schema and verification helpers can
be reused directly.

Publishing and lookup follow the same pattern as Substrate's authority-discovery:

- **Publish (`PUT_VALUE`)**: a collator serializes its `CollatorAuthority`, signs the serialized bytes with its libp2p
  network key (`peer_signature`) and with each of its authority discovery keys (`auth_signature`), and stores the
  resulting `SignedCollatorAuthorityRecord` under `sha256(authority_discovery_public_key)` — one entry per authority key.
- **Lookup (`GET_VALUE`)**: a peer computes `sha256(key)` for each authority discovery key it is resolving and issues
  `GET_VALUE` for that hash.

The `SignedCollatorAuthorityRecord` record has the following format:

```rust
/// Collator record providing the publicly reachable addresses for a collator and
/// the creation time of the record. This is the payload that gets signed; it is
/// stored in `SignedCollatorAuthorityRecord.record` in its serialized form.
pub struct CollatorAuthority {
    /// Multiaddresses through which the collator can be reached, SCALE-encoded.
    /// Every address MUST end with a `/p2p/<PeerId>` component, and all addresses
    /// MUST carry the same `PeerId`. This is required by the record scheme (not by
    /// libp2p itself, which can dial bare addresses): the verifier extracts this one
    /// `PeerId` to check `peer_signature` against. Addresses without a `/p2p`
    /// component are dropped; differing `PeerId`s make the record invalid.
    ///
    /// Note: the single `PeerId` is *per record* — it is the publishing collator's
    /// own node identity. Multiple addresses simply reflect the same node being
    /// reachable several ways (different IPs/transports/DNS). It is NOT a
    /// network-wide identity: each collator publishes its own record (keyed by
    /// `sha256(its audi key)`) carrying its own distinct `PeerId`.
    pub addresses: Vec<Vec<u8>>,
    /// Time since UNIX_EPOCH in nanoseconds, SCALE-encoded.
    /// As in authority-discovery, this lets peers replace stale records with fresher ones.
    pub creation_time: Vec<u8>,
}

/// Signature by the collator's libp2p network key, proving that the node owning the
/// `PeerId` embedded in `addresses` authorized this record.
pub struct PeerSignature {
    /// Signature over the serialized `CollatorAuthority`.
    pub signature: Vec<u8>,
    /// The libp2p public key; must correspond to the `PeerId` embedded in `addresses`.
    pub public_key: Vec<u8>,
}

/// Record published in the DHT under the key `sha256(authority_discovery_public_key)`.
///
/// Mirrors Substrate's `SignedAuthorityRecord` (authority-discovery `dht-v3`).
pub struct SignedCollatorAuthorityRecord {
    /// The serialized `CollatorAuthority`. Both signatures below are computed over
    /// exactly these bytes, so the record is stored serialized to keep verification
    /// deterministic (re-encoding a typed struct risks a non-canonical byte string).
    pub record: Vec<u8>,
    /// `record` signed by the collator's authority discovery key — the key whose
    /// `sha256` is the DHT key. SCALE-encoded. Carries no public key: the looker
    /// verifies it against the specific key whose hash it queried (see Trust Model).
    pub auth_signature: Vec<u8>,
    /// `record` signed by the collator's libp2p network key.
    pub peer_signature: PeerSignature,
}
```

Note: the parachain is not stored in the record. The DHT key is derived solely from the authority discovery key, and
the looker already knows which parachain's key set it is resolving (it fetched that set from the parachain's state, see
below), so a `parachain_id` field would be redundant for both routing and verification.

#### Trust Model for Collators

For Parachain A to securely exchange messages with Parachain B, it must first learn Parachain B's authority discovery
keys. With those keys it can resolve B's collator addresses from the DHT and verify every record it receives.

Unlike Substrate's authority-discovery — where a node learns its own authority set with a local runtime call
(`AuthorityDiscoveryApi::authorities`) — Parachain A cannot call into Parachain B's runtime. Instead it reads B's
`pallet-authority-discovery` `Keys` directly from B's state using a light-client storage proof anchored to the relay
chain. Participating parachains must therefore run `pallet-authority-discovery` and register their collators' discovery
keys in it.

The relevant key throughout this section is the dedicated **authority discovery key** (an sr25519 key with key type
`audi`, `sp_authority_discovery::AuthorityId`) — *not* the collator's Aura block-authoring key nor the relay chain's
Babe key. This is the key whose `sha256` indexes the DHT record, the key that produces `auth_signature`, and the key
returned by B's `pallet-authority-discovery::Keys`. Collators must therefore be configured with an `audi` session key
(registered in `pallet-authority-discovery`), not only an Aura key, to participate in the Speculative Messaging Network.

> **Implementation status.** This is net-new wiring, not reuse of existing parachain behavior. While the
> `sc-authority-discovery` *verification logic and record schema* are reusable, no parachain runs authority-discovery
> for its own collator set today: a collator only runs an authority-discovery worker inside its embedded relay node, in
> `Role::Discover` mode against the **relay chain** network (`cumulus/client/relay-chain-minimal-node`), to find relay
> validators. The parachain node service crates (`cumulus/client/service`, `cumulus/polkadot-omni-node`, the parachain
> template) do not instantiate `sc-authority-discovery` at all, and stock Aura parachains do not run
> `pallet-authority-discovery`. Speculative messaging therefore needs net-new wiring around the reused
> authority-discovery core — enabling `pallet-authority-discovery` (collators under `audi`), standing up a shared
> spec-msg network, and running publish + cross-para lookup over it. See
> [Discovery Architecture: Two Layers](#discovery-architecture-two-layers) for the precise reuse boundary.

The flow (**Stage 1** — establish B's verified `audi` key set; the DHT is not involved):
- 1. Read relay header: Parachain A reads Parachain B's head from the relay chain via `paras::Heads::get(Para B)`. This storage entry is located at [relay_well_known_keys::para_head(Para B)](https://github.com/paritytech/polkadot-sdk/blob/master/polkadot/primitives/src/v9/mod.rs#L269). The stored `HeadData` is B's SCALE-encoded header; participating parachains must use standard headers that expose a `state_root`.
- 2. Extract state root: The header is decoded to obtain the `state_root` of the block, and its hash is computed.
- 3. Craft storage key: We craft the key for the storage read `twox_128("AuthorityDiscovery") ++ twox_128("Keys")`. This assumes the pallet is instanced as `AuthorityDiscovery` in B's runtime; the prefix changes if it is configured under a different name.
- 4. Query peers: A request is made to the 20 closest peers that registered as providers under the `para ID || epoch randomness` key.
- 5. Submit request: The request is submitted over `/spec-msg/light/2` and includes a protobuf-encoded `RemoteReadRequest { block, keys }`, where `block` is the hash of the head from step 1 — the *included* head, for which responding collators are guaranteed to hold state.
- 6. Receive proof: The response contains a `RemoteReadResponse` carrying a storage proof.
- 7. Verify proof: Parachain A verifies the proof via [read_proof_check()](https://github.com/paritytech/polkadot-sdk/blob/acf45cfbb1080f123aab1f2001967073977798c2/substrate/primitives/state-machine/src/lib.rs#L828-L833), passing in the `state_root` (step 2), the crafted key (step 3), and the provided storage proof (step 6), obtaining B's verified set of authority discovery keys.

**Stage 2** — with B's key set verified, A then resolves each key to addresses via `GET_VALUE` under `sha256(key)` and
authenticates the returned `SignedCollatorAuthorityRecord` before connecting, exactly as authority-discovery does. The
two signature checks and what they guarantee are detailed in [Record Verification](#record-verification-stage-2) below.

#### Trust Chain

The two stages establish a strictly one-way chain of trust. Crucially, the `audi` key set is **not** verified via the
DHT — it is verified by the relay-anchored state proof, and the DHT is only trusted transitively through those keys:

```
relay chain state (A follows it; trusted, read locally)
  └─ paras::Heads[Para B] → B's header → state_root          ◄── trust anchor
        │
        │  Stage 1: verify B's audi key SET (DHT not involved)
        ▼
     RemoteReadRequest over /spec-msg/light/2  (peer UNTRUSTED)
        └─ storage proof for AuthorityDiscovery::Keys
              └─ read_proof_check(state_root, proof, key)     ◄── proof must hash up to state_root
                    └─ B's verified set of `audi` public keys
                          │
                          │  Stage 2: authenticate DHT records USING those keys
                          ▼
                       for each verified key K:
                         GET_VALUE(sha256(K))  (DHT UNTRUSTED)
                           └─ SignedCollatorAuthorityRecord
                                ├─ verify auth_signature over `record` with K   ◄── defeats impersonation
                                └─ verify peer_signature vs PeerId in addresses  ◄── binds record to libp2p node
                                      └─ trusted collator multiaddresses
                                            └─ connect over /spec-msg/exchange
```

Note that the `block` hash in `RemoteReadRequest` is only a routing hint telling the responder which state to prove;
the security comes entirely from `state_root`. A proof generated against any other block simply will not hash up to
A's `state_root`, so `read_proof_check` rejects it.

#### Record Verification (Stage 2)

A returned `SignedCollatorAuthorityRecord` carries three fields, and **both signatures are computed over the same
bytes** — the serialized `record` (the `CollatorAuthority` payload, which itself contains the addresses). This is why
`record` is stored serialized rather than as a typed struct: both verifiers re-hash *those exact bytes*. The two checks
answer different questions (mirroring `check_record_signed_with_authority_id` and `check_record_signed_with_network_key`
in `substrate/client/authority-discovery`):

**1. `auth_signature` over `record` with `K`** — "is this address set authorized by a legitimate collator of B?"
This is an sr25519 verification with message = the `record` bytes, public key = `K` (the `audi` key proven in Stage 1),
signature = `auth_signature`. Passing proves a holder of `K`'s private key endorsed exactly these addresses. `K` is not
carried in the record; A verifies against the specific key whose `sha256` it queried.

**2. `peer_signature` vs the `PeerId` in `addresses`** — "does the node A is about to dial actually control that
network identity?" The `PeerId` is extracted from the `/p2p/<multihash>` component of the multiaddresses *inside* the
record (all addresses must share the same `PeerId`). The libp2p check verifies both that `peer_signature.public_key`
corresponds to that `PeerId` (a `PeerId` is a hash of the public key) and that this key signed the `record` bytes —
i.e. proof of possession of the advertised network identity.

| Signature | Key used | Proves | Defeats |
|---|---|---|---|
| `auth_signature` | `audi` key `K` (proven in Stage 1) | a legitimate collator authorized *these addresses* | impersonation / fake address injection |
| `peer_signature` | libp2p network key (hashed to the `PeerId`) | the node at that `PeerId` holds the matching key | advertising a `PeerId` whose key the publisher doesn't control (forged/stale identity, replay) |

Only after **both** pass does A dial the `PeerId` over `/spec-msg/exchange`: the peer is provably endorsed by a
legitimate `audi` key *and* in possession of the advertised identity. `peer_signature` is optional in Substrate
(rejected only under `strict_record_validation`); for speculative messaging it should be **mandatory**, since the
eclipse-resistance argument depends on it.

These signatures guarantee **authenticity**: A cannot be fooled into accepting addresses that a legitimate B collator
did not authorize, which defeats impersonation and address tampering (eclipse via a forged identity). They do not by
themselves prevent a malicious provider from *withholding* or serving stale records; that residual censorship risk is
mitigated by querying several of the 20 closest providers and by connecting to a sufficient number of
independently-discovered honest collators.


### Message Accumulators

Each parachain maintains a Merkle Mountain Range (MMR) accumulating all outgoing
messages:

We use a hierarchical structure: per-destination MMRs with a top-level Merkle
commitment.

``` Top-Level Root (Merkle tree over per-destination MMR roots) ├── Chain B:
MMR_B Root → [Msg1, Msg2, Msg3, ...] ├── Chain C: MMR_C Root → [Msg1, Msg2, ...]
├── Chain D: MMR_D Root → [Msg1, ...] └── ... ```

**Why hierarchical?**
- Receiver only needs to prove their subtree, not traverse all messages
- Proof size: O(log D + log m) where D = destinations, m = messages to receiver
- Much better than O(k log n) for a flat structure where k =number of messages
  to prove, n = total number of messages sent by the chain.
- Late block proofs only grow with messages to that specific receiver

#### Per-destination MMR lifecycle

The per-destination MMRs are tied to the **HRMP channel** lifecycle (already on-chain, and already the connectivity
source — see [Establishing Trust](#establishing-trust)), so no new channel primitive is introduced. Positions are scoped
to a **channel instance** — one open/close lifetime of a channel — discriminated by an on-chain open marker (e.g. the
relay block at which the channel opened, or a per-pair open counter).

- **Creation.** Lazily, on the **first send** to a destination; an open channel `A→dest` is the precondition (you cannot
  send without one). The channel-open event is the on-chain anchor; the MMR is materialized on first send within that
  instance.
- **"Append-only" — scope.** Append-only applies *within a channel instance's lifetime*: leaves are never rewritten or
  reordered while the channel is open. It does **not** mean the structure persists forever.
- **Persistence / pruning.** Only the MMR **peaks** (`O(log n)` hashes) plus the current **position** need persist to
  keep appending and to serve proofs; **leaf data is prunable** once messages are consumed (acknowledged / included).
- **Removal (GC).** After the channel **closes *and* drains** — all in-flight messages consumed by the receiver and
  confirmed — the per-destination entry is dropped. Closed channels do not live forever; the top-level root simply
  recomputes over the remaining per-destination roots.
- **Reopen.** A reopened channel is a **new instance**. Positions are scoped to `(destination, instance)`, so position 0
  of the new instance is unambiguous vs the old. The receiver, on observing the on-chain reopen, **resets
  `last_processed` to 0 for the new instance** — avoiding the position-reuse hazard (skipped or replayed messages) that a
  naïve reset without a discriminator would cause.

### Candidate Commitments (Verified by Relay Chain)

The commitments in candidate receipts are minimal—just the hashes needed for
relay chain verification:

```rust
/// In candidate commitments - what the relay chain verifies
struct ProvidesCommitment {
    /// Top-level Merkle root over all per-destination MMR roots
    root: Hash,
}

struct RequiresCommitment {
    /// Source parachain we're receiving from
    source: ParaId,
    /// The root we built against (from source's provides)
    expected_root: Hash,
}
```

The relay chain verifies matches the "requires" commitment with the
corresponding "provides" commitment. A parachain block will only be made
available/enacted when all its "requires" are provided.

### Parachain Runtime State (Internal)

Each parachain runtime maintains internal state for message tracking.

```rust
/// Identifies one open/close lifetime of a channel, so positions in a reopened
/// channel are unambiguous vs the previous instance. Derived from an on-chain open
/// marker (e.g. the relay block the channel opened at, or a per-pair open counter).
type ChannelInstance = u64;

/// Sender-side: tracking outgoing messages (in parachain runtime)
struct OutgoingMessageState {
    /// Per-destination MMRs, keyed by (destination, channel instance). Created lazily
    /// on first send over an open channel; dropped after the channel closes and drains
    /// (see "Per-destination MMR lifecycle").
    per_destination: BTreeMap<(ParaId, ChannelInstance), MMR>,
    /// Current top-level root (this goes into ProvidesCommitment)
    current_root: Hash,
}

/// Receiver-side: tracking incoming messages (in parachain runtime)  
struct IncomingMessageState {
    /// Per-(source, channel instance) tracking. Scoping by instance is what lets
    /// `last_processed` reset cleanly across a channel close/reopen.
    per_source: BTreeMap<(ParaId, ChannelInstance), SourceState>,
}

struct SourceState {
    /// Last processed position in this source/instance's per-destination MMR for us.
    last_processed: u64,
    /// The source's top-level root we last built against.
    last_seen_root: Hash,
    /// The source's per-destination subtree root for us at `last_seen_root`. Lets us
    /// confirm an incoming batch's `subtree_root` is continuous with what we last saw —
    /// detecting a sender that swaps our subtree between roots.
    last_seen_subtree_root: Hash,
}
```

### Off-Chain Communication (Between Collators)

Messages are exchanged off-chain between collators. The relay chain never sees
message contents—only commitments.

```rust
/// Message exchanged off-chain between collators
struct OutgoingMessage {
    /// Destination parachain
    destination: ParaId,
    /// Message payload (actual XCM or other data)
    payload: Vec<u8>,
    /// Position in sender's per-destination MMR. Needed so the receiver can pull only
    /// the unprocessed suffix (`from_position = last_processed + 1`) and enforce
    /// sequential processing (see Message Exchange Protocol).
    position: u64,
}

/// What a sender shares with receivers (off-chain), self-contained enough for the
/// receiver to verify *without trusting the peer*. Two groups of fields:
///   - light-client proof → fetch-time authenticity (authored block + genuine output)
///   - MMR commitment      → on-chain matching anchor + basis for late-block proofs
struct MessageBatch {
    /// Source chain.
    source: ParaId,
    /// Source block that produced these messages.
    source_block: Hash,

    // --- light-client proof: binds the data to a block authored by a real authority ---
    /// SCALE-encoded header of `source_block`. Its consensus seal is verified against the
    /// sender's collator set (authorship), and its `state_root` anchors `messages_proof`.
    source_header: Vec<u8>,
    /// Proof, against the header's `state_root`, that `provides_root` (and the message
    /// leaves if not otherwise derivable) is the genuine committed output at `source_block`.
    /// By default a *storage proof* checked with `read_proof_check` (same primitive as the
    /// `audi` read); a runtime-call proof of `SpecMsgApi_messages(...)` is the heavier
    /// fallback (see the Message Exchange Protocol trust rules). This is what makes the peer
    /// untrusted: a forged batch fails this check.
    messages_proof: StorageProof,

    // --- MMR commitment: matched on-chain, and extended by late-block proofs ---
    /// The provides root committed by `source_block` (== `ProvidesCommitment.root`).
    /// The receiver commits this as `requires.expected_root`.
    provides_root: Hash,
    /// The receiver's per-destination MMR root: the target `subtree_inclusion_proof`
    /// proves, and the base a late-block proof extends when the root advances. (Needed
    /// for the late-block path; omittable in the simple live case where the receiver
    /// already has `provides_root` from `messages_proof`.)
    subtree_root: Hash,
    /// Proof that `subtree_root` is in `provides_root`.
    subtree_inclusion_proof: MerkleProof,

    /// The actual messages (authenticity established by `messages_proof` above).
    messages: Vec<OutgoingMessage>,
}
```

Receivers verify:
0. `provides_root` belongs to a block **authored by an actual sender authority** — via a light-client proof against
   that block (see the [Message Exchange Protocol](#message-exchange-protocol-spec-msgexchange) trust rules below). The
   MMR checks alone bind messages to a *root*; this binds the root to a *real block*, so a peer can't fabricate one.
1. `subtree_inclusion_proof` proves `subtree_root` is in `provides_root`
2. Messages hash to leaves in the subtree MMR
3. Messages are sequential from last processed position

#### Message Exchange Protocol (`/spec-msg/exchange`)

This is the wire protocol over which collators transfer the `MessageBatch`es defined above. It is **pull-only**: a
single libp2p request/response sub-protocol, `/spec-msg/exchange/1`. The receiver fetches; there is no sender push.

Roles are symmetric at the protocol level (every collator runs both the client and the server side, since any chain can
be a sender or a receiver), but a given transfer is always initiated by the receiving side.

**Why pull-only.** A receiver can only act on messages by producing a block, and it produces blocks at its own slot
cadence — so the delivery-latency floor is the receiver's block time regardless of how it is notified. The receiver
therefore pulls from each source at the start of every block-production attempt, requesting only the unprocessed suffix
(`from_position = last_processed + 1`). This keeps the protocol to one stateless, idempotent request/response with no
connection bookkeeping on the sender. A push/announcement channel would only reduce latency for *event-driven* block
production that reacts sub-slot; if that is ever needed it can be added later as a pure optimization, without changing
the pull path.

**Encoding.** All payloads are SCALE-encoded — unlike the discovery records (which are protobuf, mirroring
authority-discovery). SCALE is used here because these payloads carry runtime types (`ParaId`, `Hash`, MMR/Merkle and
light-client storage proofs) that already have canonical SCALE encodings shared with the parachain runtime.

**Trust — verify the data, never trust the peer.** The connection is authenticated to the peer's `PeerId` (Noise
handshake, over an address verified per the [Trust Model](#trust-model-for-collators)), but **authorizing a peer grants
nothing about the data it serves**: a connected peer must be unable to do anything worse than *not serve*. So the
receiver does not trust a peer-supplied `provides_root` — it verifies the batch with a **light-client proof against the
sender's block**, the same primitive used to fetch authority keys:
- messages are proven to be the genuine output at the sender's block, via a light-client proof against that block's `state_root` (see proof flavors below); and
- that block's **authorship is verified against the sender's collator set** (its consensus seal), binding the data to a block authored by an *actual* authority — not merely to a root the peer asserted.

**Two proof flavors for the `state_root` check.** By default this is a **storage proof** (Flavor A): the responder
proves `provides_root` (the committed MMR root, stored in `OutgoingMessageState`) — and the message leaves if not
otherwise derivable — with `sp_state_machine::prove_read`, and the receiver checks it with `read_proof_check(state_root,
proof, keys)`. This is the **same primitive used for the `audi` read** (`RemoteReadRequest` over `/spec-msg/light/2`), needs
**no runtime execution**, and the MMR fields in `MessageBatch` then verify the messages against the proven
`provides_root`. The heavier fallback is a **runtime-call proof** (Flavor B): execute a standardized
`SpecMsgApi_messages(...)` API in-proof (`prove_execution` → `execution_proof_check`, via `RemoteCallRequest`), used only
when the messages can't be read from a known storage layout — it additionally requires the sender's runtime WASM
(`:code`), so it is reserved for that case. `messages_proof: StorageProof` carries either form.

The `provides_root` / MMR commitment stays the **on-chain matching anchor**: the receiver commits it as
`requires.expected_root`, which the relay chain (or a late-block proof) later matches against the sender's real
`ProvidesCommitment.root`. So integrity comes from proof + commitments, never the channel, and **no signatures are added
to the exchange wire**.

Why this is stronger than an MMR check against a peer-supplied root: without authorship verification, a
malicious-but-authorized peer could serve a well-formed batch under a root *no real block committed*, inducing a doomed
speculative block that fails inclusion later — worse than withholding. Binding the fetch to an authority-authored block
caps the worst case back at withholding. The cost: verifying a *not-yet-included* sender block's authorship needs the
sender chain's consensus check against its collator set (read from the last included head's state) — heavier than an MMR
self-check, and a real factor on the hottest speculative path.

##### Verifying authorship: the Aura seal check

"Authored by an actual authority" concretely means verifying the sender block's **Aura seal** — the same logic as
`sc_consensus_aura::import_queue::check_header`. Given the `source_header` H carried in the `MessageBatch`:

1. **Extract slot and seal from H** (local decode — `H` is peer-supplied, no cross-chain access) via
   `sp_consensus_aura::digests::CompatibleDigestItem`: slot ← `as_aura_pre_digest()` (the `PreRuntime(AURA_ENGINE_ID,
   slot)` digest); signature ← `as_aura_seal()` (the trailing `Seal(AURA_ENGINE_ID, …)` digest).
2. **Fetch the trusted authority set.** Read the sender's **`aura`** set — `pallet_aura::Authorities`
   (== `AuraApi::authorities()`) — via a relay-anchored light-client proof from the last *included* head, the **same
   mechanism** as the `audi` read in the Trust Model (different storage key). This is the **`aura`** *authoring* set,
   distinct from the **`audi`** *discovery* set: authorship uses `aura`, peer discovery uses `audi`.
3. **Compute the expected author** = `authorities[slot % authorities.len()]` (round-robin slot assignment). This needs
   *both* the slot (step 1) and the set (step 2) — the set must be fetched before the author can be derived; the header
   alone yields only the slot.
4. **Pre-seal hash** = hash of H with the `Seal` digest removed (the seal signs the header minus itself).
5. **Verify** the sr25519 signature over the pre-seal hash against the expected author's key. Pass ⇒ H was authored by a
   legitimate sender authority.

**Authority-set freshness.** H is a *descendant* of the included head the set was read from, so the set is valid for H
**within the same session**. Across a session/authority rotation the included-head set may be stale — for the PoC,
restrict speculation to within-session (sessions are long, the speculative window is a few blocks); a later phase can
follow the sender's header chain across the rotation to verify the new set. (Parachains on a different slot-based
consensus substitute the equivalent seal/author check; Aura is the common case.)

**Where the inputs come from, and where this runs.** Two points that are easy to get wrong:

- **The sender header `H` is *peer-supplied*, not read from the sender chain.** It arrives in the `MessageBatch`
  (`source_header`), so extracting the slot and seal (step 2) is purely *local decoding* — no cross-chain access. `H` is
  untrusted input; the seal check (steps 1, 3–5) is exactly what validates it. The **only** trustless cross-chain read
  is the `aura` authority set in step 1, fetched via the relay-anchored `/spec-msg/light/2` proof of
  `pallet_aura::Authorities` from the *included* head — the peer serving it can't forge it, because it is checked
  against the relay-anchored `state_root` (same as the `audi` read). Likewise the messages are peer-supplied and
  validated by `messages_proof` against `H.state_root`.
- **This is collator (node) work, not runtime work, and it's a liveness optimization — not the correctness backstop.**
  Correctness is guaranteed on-chain: the receiver commits `requires.expected_root = provides_root`, and the relay chain
  matches it against the sender's real `provides` at inclusion, so a bad batch can only make the receiver's block *fail
  inclusion* — never enact bad state. The collator-side seal check exists so the receiver does not *build a doomed block*
  on a fabricated or non-canonical header in the first place (eskimor's "can't do worse than not serving").

> **Later phase — acknowledgements.** The speculative path will eventually verify that the source block was
> **acknowledged** (see [Acknowledgement Extensions](#acknowledgement-extensions)), not just authored. The light-client /
> request-response layer (`/spec-msg/light/2`) is therefore meant to generalize beyond authority keys — to messages and
> acknowledgment signatures — and must make ack-signature fetching efficient. Out of PoC scope, but the layer should not
> be designed key-only.

##### Request / response

```rust
/// Request over /spec-msg/exchange/1, sent by the receiver.
struct GetMessages {
    /// Source chain whose messages we want (the responder's own ParaId).
    source: ParaId,
    /// Next expected position == receiver's `last_processed + 1` (see `SourceState`).
    /// The responder returns only messages for the requester at this position and above.
    from_position: u64,
    /// Optionally pin the provides root to build the batch against (e.g. to re-fetch a
    /// specific historical root for a late-block proof). If `None`, the responder uses
    /// its latest committed root.
    at_provides_root: Option<Hash>,
}

enum ExchangeResponse {
    /// The unprocessed suffix for the requester, carried with the light-client proof
    /// needed to verify it: the sender's block header (authorship checked against the
    /// sender's collator set) plus the state/call proof of the `SpecMsgApi_messages`
    /// output at that block (which also yields `provides_root` for on-chain matching).
    Batch(MessageBatch),
    /// Nothing new: the requester is already up to date at `provides_root`.
    UpToDate { provides_root: Hash },
    /// The responder cannot serve the requested root/position (e.g. pruned, or not yet
    /// produced). The requester retries another source collator or backs off.
    Unavailable,
}
```

The response's `provides_root` (in `Batch` or `UpToDate`) is the root the receiver then commits as
`RequiresCommitment.expected_root`. `from_position` is also what makes `OutgoingMessage.position` /
`SourceState.last_processed` necessary — the receiver pulls exactly the unprocessed suffix, which is not reconstructible
from the proof alone.

##### End-to-end flow

```
receiver R (about to author a block)               sender S
────────────────────────────────────              ────────
for each source S in R's ingress channels:
  GetMessages{S, last_processed_S + 1, None}  ───►  (S has block N: provides_root R_N)
                                              ◄───  Batch(MessageBatch{ provides_root: R_N,
                                                                        messages: [pos..], .. })
verify: subtree inclusion in R_N,
        msg hashes, sequentiality
process messages, advance last_processed_S
produce R's block committing
  requires{ source: S, expected_root: R_N }
```

If the response is `UpToDate`, R emits no new `requires` for that source in this block. The `provides_root` carried in
the batch is the anchor tying the off-chain transfer to on-chain verification: it equals the sender's
`ProvidesCommitment.root`, and the receiver commits to it as `RequiresCommitment.expected_root`, which the relay chain
(or a late-block proof) later matches.

##### Liveness, limits, retries

- **Cadence:** R pulls from each source once per block-production attempt; latency is bounded by R's own block time.
- **Multiple sources per peer:** R may have discovered up to 20 collators per source chain; on `Unavailable`/timeout it
  retries another.
- **Response size:** bounded; one response carries one `MessageBatch`. Senders SHOULD cap messages per batch and
  receivers paginate via repeated `GetMessages` with an advancing `from_position`.
- **Idempotency:** `GetMessages` is idempotent for a fixed `(source, from_position, at_provides_root)`, so retries are safe.
- **Implementation:** use `sc-network`'s `RequestResponseConfig` for `/spec-msg/exchange/1`, mirroring existing Substrate
  request-response handlers.

### Relay Chain Matching

When the relay chain processes candidates for inclusion, it performs commitment
matching. The relay chain only sees the minimal commitments (hashes), not
internal state.

#### Live Communication (Simultaneous Arrival)

When both sending and receiving blocks arrive at the relay chain at
approximately the same time:

```rust
fn verify_live_matching(
    sender_candidate: &CandidateReceipt,
    receiver_candidate: &CandidateReceipt,
) -> Result<(), Error> {
    let provides = &sender_candidate.commitments.provides;
    let requires = receiver_candidate.commitments.requires
        .iter()
        .find(|r| r.source == sender_candidate.para_id)
        .ok_or(Error::NoRequirement)?;
    
    // Direct match: receiver expects exactly what sender provides
    if requires.expected_root == provides.root {
        return Ok(());
    }
    
    Err(Error::MissingRequirement)
}
```

#### Matching with Included Blocks

For requirements against already-included blocks:

```rust
fn verify_against_included(
    receiver_candidate: &CandidateReceipt,
    included_provides: &BTreeMap<ParaId, Hash>,  // Just the roots
) -> Result<(), Error> {
    for requires in &receiver_candidate.commitments.requires {
        let provides_root = included_provides
            .get(&requires.source)
            .ok_or(Error::MissingRequirement)?;
        
        if &requires.expected_root == provides_root {
            // Exact match
            continue;
        }
        
        // Roots don't match - need late block proof in POV
        return Err(Error::RequiresProof);
    }
    Ok(())
}
```

### Late Block Proofs

When a receiving block's requirements reference an older state than what's
currently available, we need a proof mechanism. This is similar to the
scheduling parent header chain in Low-Latency v2.

#### The Problem

```
Timeline:
  Block A_N: provides root R_N
  Block A_{N+1}: provides root R_{N+1}
  Block A_{N+2}: provides root R_{N+2}
  
  Block B_M: built expecting A_N's state (requires.expected_root = R_N)
  
  By the time B_M arrives at relay chain, A_{N+2} is already included.
  B_M's requires (R_N) doesn't match current provides (R_{N+2}).
```

#### The Solution

The late block includes a proof in its POV (outside the block itself)
demonstrating that the messages it processed are still valid under the current
provides root.

With the hierarchical structure, B only needs to prove its subtree:

```rust
/// Late block proof included in POV (not in commitments!)
struct LateBlockProof {
    /// Source chain this proof is for
    source: ParaId,
    
    /// Prove our subtree was in the old (expected) root
    old_subtree_root: Hash,
    old_subtree_proof: MerkleProof,
    
    /// The current provides root we're updating to
    new_provides_root: Hash,
    
    /// Prove our subtree is in the new (current) root
    new_subtree_root: Hash,
    new_subtree_proof: MerkleProof,
    
    /// If our subtree's MMR grew, prove it extended correctly
    subtree_extension: Option<MMRExtensionProof>,
}

/// MMR extension proof (only needed if new messages were added to our subtree)
struct MMRExtensionProof {
    /// MMR proof data (peaks and connecting nodes)
    proof: Vec<Hash>,
}
```

#### Verification

The PVF verifies the late block proof and **transforms** the block's original
`requires` commitment into an updated one that references the current `provides`
root. This way, the relay chain only ever sees a commitment it can verify
against currently-available state.

```rust
fn process_late_block_requires(
    block_requires: &RequiresCommitment,  // From the block itself (references old root)
    proof: &LateBlockProof,               // From POV
) -> Result<RequiresCommitment, Error> {
    // 1. Verify old subtree was in the root the block expected
    verify_merkle_proof(
        block_requires.expected_root,
        &proof.old_subtree_proof,
        (block_requires.source, proof.old_subtree_root),
    )?;
    
    // 2. Verify new subtree is in the current root (which we'll output)
    verify_merkle_proof(
        proof.new_provides_root,
        &proof.new_subtree_proof,
        (block_requires.source, proof.new_subtree_root),
    )?;
    
    // 3. Verify subtrees are related (same or extended)
    if proof.old_subtree_root != proof.new_subtree_root {
        if let Some(ext) = &proof.subtree_extension {
            // Subtree grew - verify extension
            verify_mmr_extension(
                proof.old_subtree_root,
                proof.new_subtree_root,
                ext,
            )?;
        } else {
            return Err(Error::SubtreeChangedWithoutProof);
        }
    }
    
    // 4. Return UPDATED commitment for the candidate
    // The relay chain will verify this against the current provides root
    Ok(RequiresCommitment {
        source: block_requires.source,
        expected_root: proof.new_provides_root,
    })
}
```

Note: The PVF verifies the proof—the relay chain only sees the transformed
commitment. Message ranges, MMR sizes, and proof details are all internal to the
parachain. The proof just demonstrates that the receiver's view of their subtree
is consistent with the current provides root.

### Proof Size Considerations

With the hierarchical structure and Low-Latency v2 allowing relay parents up to
~14,400 blocks old (24 hours), we must consider proof sizes for worst-case
scenarios.

#### Late Block Proof Components

A late block proof consists of:
1. **Top-level Merkle proofs**: O(log D) where D = number of destinations
2. **Subtree MMR extension proof**: O(log m) where m = messages to this receiver

#### Proof Size Analysis

For a sender with 100 destinations, receiver getting 1000 messages:
- Top-level proofs: ~2 × log₂(100) ≈ 14 hashes ≈ 450 bytes
- Subtree extension: ~log₂(1000) ≈ 10 hashes ≈ 320 bytes
- **Total: ~770 bytes**

Worst case (1000 destinations, 24 hours of messages to one receiver):
- Top-level proofs: ~2 × log₂(1000) ≈ 20 hashes ≈ 640 bytes
- Subtree extension: ~30 hashes ≈ 960 bytes
- **Total: ~1.6 KB**

This is much better than a flat structure where proof size depends on ALL
messages, not just messages to the receiver.

#### Practical Limits

Proofs are expected to stay small and should therefore practically fit into any
POV. To be sure, we should nevertheless set aside a few kB (e.g. 50) for not
breaking the late submission opportunity due to the POV getting too large.

The hierarchical structure naturally keeps proofs small because:
- Receiver only proves their subtree
- Subtree only contains messages to that specific receiver
- High volume to other chains doesn't affect proof size

### Acknowledgement Extensions

> **Background: what an acknowledgement is.** (Summarized from Low-Latency v2,
> [PR #11413](https://github.com/paritytech/polkadot-sdk/pull/11413); that design doc is not in this branch.)
> Instead of waiting ~36s for relay-chain finality, collators sign **acknowledgements** that commit to a parachain
> block becoming canonical (submitted to and included by the relay chain on the canonical fork). An acknowledgement
> `ACK_X(N)` is collator X's signature — with its authoring (`aura`) key, over the block identity (block hash + slot) —
> attesting "I will build on N or a descendant in my slot, and I won't acknowledge a conflicting block with the same
> parent." It is a commitment **with accountability**: collators are rewarded for timely acks and **slashed** if an
> acknowledged block fails to become canonical (the relay-chain decoupling — older relay parents, scheduling parent,
> guaranteed resubmission — is what gives collators enough control to be fairly blamed). A block counts as
> **"acknowledged"** once the producer *and* the next collator have signed: **2 signatures in-slot, 3 at a slot
> boundary** (minimum). That is the bar that gives a block low-latency, pre-inclusion confidence — and the
> "sufficiently confirmed" threshold the rule below refers to.
>
> For speculative messaging this means: verifying a source block is *acknowledged* (not just *authored*) requires
> fetching those ack signatures and checking them against the sender's `aura` set — the same light-client mechanism as
> the [seal check](#message-exchange-protocol-spec-msgexchange), one reason `/spec-msg/light/2` must generalize beyond
> authority keys.
>
> **Single-collator parachains.** The "2-in-slot / 3-at-boundary" guarantee assumes *distinct* signers. With a single
> collator (common today), producer and "next collator" are the same key, so the scheme degenerates to the producer's
> **self-acknowledgement**: it still runs and still yields a *slashable, anti-equivocation commitment* (and recovery via
> resubmission if reneged), but it loses the independent-confirmation property — a malicious single collator is only
> *deterred* (slashing) and *recovered from* (resubmit), not *prevented*. So the strong acknowledgement guarantee needs
> **≥2 collators** (and shines with diverse sets / super-chains); a single-collator or otherwise untrusted source should
> simply use **inclusion-based** messaging, which uses no acknowledgements at all (it waits for the sender's `provides`
> to be included) and is still faster than HRMP. Note also that "slashable" presumes collators are bonded — itself
> net-new Low-Latency v2 infrastructure, and the *only* deterrent in the single-collator case.

For low-latency chains using speculative messaging, the acknowledgement rules
from Low-Latency v2 are extended:

#### Extended Rule for Message Dependencies

> A collator must not acknowledge a block if it depends on speculative messages
  from blocks that are not yet sufficiently confirmed.

"Sufficiently confirmed" depends on the trust relationship:

| Source Chain Type | Confirmation Required |
|-------------------|----------------------|
| Same super-chain | Same super-block (co-authored) |
| Same trust domain (low-latency) | Acknowledged by source chain collators |
| Different trust domain | Included on relay chain |

#### Acknowledgement Timing

```
Timeline for Block B receiving message from Block A (same trust domain):

t=0:    Chain A collator produces Block A (sends message, provides P_A)
t=1:    Chain B collator sees Block A + messages, produces Block B (requires P_A)
t=1:    Chain A collator acknowledges Block A (in parallel with above)
t=2:    Chain B collator sees A's acknowledgement, acknowledges Block B
...
t=N:    Both blocks included on relay chain, commitments verified
```

For different trust domains, acknowledgement of Block B depends on relay chain
inclusion of Block A instead of collator acknowledgement.

### Cycle Prevention

When two chains want to exchange messages speculatively in the same block, we
risk deadlock: each waits for the other's acknowledgement. For non-super chains
(above scenario), we trivially break cycles, by sticking to the procedure
above. In particular t=1: We only process the messages in block `A` once we
have seen the entire block. By doing this both ways, block `A` can not depend
on the current block `B`, because it did not exist when `A` was built. This
holds even for multi-party communication.

Conclusion: By not allowing intra-block communication, no cycles between blocks
can exist and above acknowledgment procedure is sound. For Basti Blocks, we
will end up with cycles between POVs, but those don't seem problematic, apart
from the fact that those candidates can only become available atomically: All
or nothing.

### Super Chains

Super chains are a set of parachains operated by the same collator set, enabling
the tightest possible integration including intra-block messaging.

#### Definition

```rust
struct SuperChainConfig {
    /// The parachains that form this super chain
    member_chains: BTreeSet<ParaId>,
    
    /// Collator set (must be identical across all members)  
    collators: Vec<CollatorId>,
    
    /// Slot duration (must be synchronized)
    slot_duration: Duration,
}
```

#### Super-Block Production

When a collator's slot arrives, they produce blocks for ALL member chains atomically:

```rust
struct SuperBlock {
    /// Individual chain blocks, keyed by ParaId
    blocks: BTreeMap<ParaId, Block>,
    
    /// Slot this super-block was produced in
    slot: Slot,
    
    /// The collator who produced this super-block
    author: CollatorId,
}

impl SuperBlock {
    fn hash(&self) -> Hash {
        // Merkle root of constituent block hashes for efficient individual proofs
        let block_hashes: Vec<(ParaId, Hash)> = self.blocks
            .iter()
            .map(|(id, b)| (*id, b.hash()))
            .collect();
        merkle_root(&block_hashes)
    }
}
```

#### Intra-Block Messaging

Within a super-block, messages can flow in both directions between any member
chains because:

1. The same collator produces all blocks
2. They have access to all chains' state simultaneously 
3. They can resolve message dependencies during block production
4. Cycles are fine and supported

```
┌─────────────────────────────────────────────────────────────────┐
│                     Super-Block N (Slot S)                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│   Chain A Block    ←──── messages ────→    Chain B Block        │
│        │                                        │               │
│        │           ←──── messages ────→         │               │
│        ↓                                        ↓               │
│   Chain C Block    ←──── messages ────→    Chain D Block        │
│                                                                 │
│   All blocks co-authored, bidirectional messages in one cycle   │
└─────────────────────────────────────────────────────────────────┘
```

#### Super-Block Acknowledgements

Instead of acknowledging individual blocks, collators acknowledge the entire
super-block:

```rust
struct SuperBlockAcknowledgement {
    /// Merkle root of constituent block hashes
    super_block_hash: Hash,
    
    /// Slot the super-block was produced in
    slot: Slot,
    
    /// Signature from the acknowledging collator
    signature: Signature,
}
```

This binds all constituent blocks together—either all make it to the relay
chain, or the acknowledging collators are slashable.

#### Partial Failures

If a collator cannot produce a block for one member chain (e.g., state
unavailable):

1. **Independent chains**: If the failing chain has no message dependencies with
   others in this super-block, other chains can proceed normally.

2. **Dependent chains**: Chains with message dependencies on the failing chain
   must also skip this super-block.

3. **Next collator takes over**: The next collator in the slot rotation handles
   the skipped chains.

---

## Trust Domains

Not all chains trust each other equally. A "trust domain" is a *conceptual* grouping — it is **not** a registered or
symmetric relay-chain construct. It emerges from each chain's **local, per-source** configuration (see
[Establishing Trust](#establishing-trust)): a set of chains that all configure the speculative (`BestBlock`) mode for
one another effectively forms a domain. The diagram below is that emergent view; the mechanism underneath is
receiver-local and may be asymmetric.

```
┌─────────────────────────────────────────────────────────────────┐
│                         Trust Domain A                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐              │
│  │  Chain 1    │←→│  Chain 2    │←→│  Chain 3    │              │
│  │  (super)    │  │  (super)    │  │             │              │
│  └─────────────┘  └─────────────┘  └─────────────┘              │
│         ↑               ↑               ↑                       │
│         └───────────────┴───────────────┘                       │
│              Fast speculative messaging                         │
│              (acknowledgement-based)                            │
└─────────────────────────────────────────────────────────────────┘
          │ 
          │ Inclusion-based (still faster than HRMP,
          │ still off-chain, just waits for provides inclusion)
          ↓
┌─────────────────────────────────────────────────────────────────┐
│                         Trust Domain B                          │
│  ┌─────────────┐  ┌─────────────┐                               │
│  │  Chain 4    │←→│  Chain 5    │                               │
│  └─────────────┘  └─────────────┘                               │
└─────────────────────────────────────────────────────────────────┘
```

#### Within a Trust Domain

- Speculative messaging based on acknowledgements
- Low latency (parachain block times)
- Chains trust each other's collators to acknowledge honestly

#### Across Trust Domains

- Inclusion-based messaging (wait for provides to be included)
- Higher latency but no trust assumptions beyond relay chain
- Still faster than HRMP (off-chain message passing, on-chain commitment
  verification only)

#### Establishing Trust

Two separate concerns must not be conflated:

- **Connectivity (who you communicate with)** is defined by the **HRMP channel graph**, which spec-msg reuses rather
  than reinventing — spec-msg is HRMP's replacement, so the existing channel registry *is* the communication topology.
  It is read from relay state: `HrmpIngressChannelsIndex[my_para]` gives the sources a chain receives from (the set to
  discover, connect to, and pull from), and `HrmpEgressChannelsIndex` gives its destinations. This reuses only the
  channel *topology* (a small per-pair record), not HRMP's relay-state message path, so it does not reintroduce HRMP's
  scaling cost. *All* channel peers are exchanged with over the spec-msg network, regardless of their per-source speculation mode.

- **Trust (how/when you act on a message)** is an additional classification configured **locally in the receiving
  parachain's runtime** — it is *not* derived from the channel graph and is *not* registered on the relay chain. An open
  HRMP channel means "we agreed to communicate," *not* "I trust your collators to acknowledge honestly," so it is
  declared separately. Among the chains it communicates with, a parachain picks, **per source**, how soon to act on that
  source's messages:

```rust
// In the receiving parachain's runtime — local, asymmetric, no relay registration.
parameter_types! {
    // Per source: how soon THIS chain acts on that source's messages.
    pub MessageSources: BTreeMap<ParaId, SpeculationMode> = btreemap! {
        ParaId(1001) => SpeculationMode::BestBlock,    // lowest latency; trust 1001's collators
        ParaId(1002) => SpeculationMode::BackedBased,  // act once backed on the relay chain
        // an open-channel source not listed defaults to InclusionBased
    };
}

enum SpeculationMode {
    /// Act only once the source's `provides` is *included* on the relay chain. No trust
    /// beyond the relay chain; the safe default (still faster than HRMP).
    InclusionBased,
    /// Act once the source candidate is *backed* on the relay chain. Intermediate latency.
    BackedBased,
    /// Act on the source's *acknowledged*, pre-inclusion block. Lowest latency; requires
    /// trusting the source's collators + Low-Latency v2 acknowledgement signatures.
    BestBlock,
}
```

This is deliberately **receiver-local and asymmetric**, with **no relay-chain "trust domain" pallet**:

- **Risk sits with the actor.** A receiver that speculates too eagerly only risks *its own* block failing inclusion (it
  resubmits) — it cannot corrupt or harm the source. So the speculation mode is the receiving collator's tunable, not a
  network-wide enforced policy: "I trust your collators" is B's statement about A, and A need neither consent nor know.
- **No relay registration is needed.** Connectivity is already on the relay chain via the HRMP channel graph (above);
  and accountability does not need the relay to know a "domain" exists — under Low-Latency v2 a source collator is
  slashable for *its own* equivocated or non-canonical acknowledged block, provable from its own signatures, independent
  of who relied on it. There is no cross-chain "A owes B" obligation for the relay to attest.
- **Asymmetry is fine, not a misconfiguration.** If B trusts A but A does not reciprocate, B still speculates on A and A
  simply uses its own modes for its own sources. Messaging mode is directional and per-receiver by nature.

> **Alternative considered — a relay-chain `trust-domain` pallet** (symmetric domains created/accepted/left on the relay
> chain, with per-domain enforced config). Rejected for the base design: it adds relay-chain consensus coordination
> against the goal of minimal relay changes, and re-solves problems already handled — connectivity (HRMP channel graph)
> and accountability (per-collator, per-own-block Low-Latency v2 slashing, which needs no domain attestation). A relay
> registry would only be warranted for relay-*enforced* cross-chain slashing or coordinated domain-wide policy, neither
> of which this design's risk model requires. The `SpeculationMode` taxonomy from that proposal is kept here, as local
> config.

---

## Censorship Considerations

Speculative messaging introduces new censorship dynamics that must be
understood.

### Cascading Dependencies

If Chain A's backing group censors Chain A's block, and Chain B has a `requires`
dependency on that block:

- Chain B's block cannot be included until Chain A's block is included
- If Chain A is delayed long enough, Chain B's availability will time out and B
  must be resubmitted
- When both are resubmitted (likely around the same time), they'll typically
  arrive together—no late block proof needed

### Mitigation Strategies

#### 1. Domain Size Limits

Limit trust domains to a reasonable size (e.g., 5-10 chains). This bounds the
"blast radius" of cascading delays.

#### 2. Resubmission

If Chain A is censored long enough that Chain B's availability times out, Chain
B simply resubmits. Since both chains are likely resubmitting around the same
time, they'll typically be included together without needing late block proofs,
although they are available if necessary, adding robustness.

#### 3. On-Demand Parachains

If a chain detects persistent censorship, it can use on-demand parachain slots
(different backing group) to get a block included.

#### 4. Cross-Domain Independence

Organize chains such that critical paths don't depend on speculative messaging
across many chains. Keep the speculative "hot path" short; use inclusion-based
for less time-sensitive communication.

---

## Comparison with Alternatives

### vs. Current HRMP

| Aspect | HRMP | Speculative Messaging |
|--------|------|----------------------|
| Latency | 12-18+ seconds | Parachain block time (speculative) or 2 relay blocks (inclusion-based) |
| Scalability | Limited (relay chain state) | High (off-chain, only commitments on-chain) |
| Trust | Relay chain only | Relay chain + optional collator acknowledgements |
| Message data | Flows through relay chain | Never touches relay chain |

### vs. Parallel Processing Runtimes (Solana-style)

| Aspect | Parallel Runtime | Super Chains |
|--------|------------------|--------------|
| Scaling | Vertical (all nodes process everything) | Horizontal (load distributed) |
| State | All nodes hold all state | Sharded across chains |
| Development | Implicit parallelism | Explicit sharding |
| Hardware | High requirements for all nodes | Lower requirements, specialized by chain |

Super chains provide similar developer experience (tight integration, fast
messaging) while maintaining horizontal scaling.

### vs. Ethereum L2 Preconfirmations

| Aspect | Preconfirmations | Speculative Messaging |
|--------|------------------|----------------------|
| Confirmation source | L1 validators | Parachain collators |
| Complexity | Very high (L1 understands L2 txs) | Moderate (chain-agnostic commitments) |
| Decentralization | Often centralized sequencers | Decentralized collator sets |
| Enforcement | Limited (many failure modes) | Higher (clear rules) |

---

## Phasing / MVP Scope

The full design above optimizes for latency. But the **primary goal — replacing HRMP — does not require the latency
machinery.** It can be delivered first as an **inclusion-based-only** MVP, with speculation added later. The decisive
win at every phase is the same: **message data never touches relay-chain state** (HRMP's scaling bottleneck); only
commitments do. Latency for the MVP is comparable to HRMP (both gated by relay inclusion) — the latency *optimization* is
explicitly deferred.

This maps directly onto the [`SpeculationMode`](#establishing-trust) ladder.

### Phase 1 — MVP: inclusion-based only (`SpeculationMode::InclusionBased`)

The receiver acts on a source's messages only once that source's `provides` is **included** on the relay chain. Relay
inclusion then subsumes most of the trust machinery, which lets the MVP **omit** several things the full design needs:

- **No authorship verification.** The `provides_root` the receiver builds against is recorded in relay state from an
  *already-validated, included* candidate — so the relay chain has effectively vouched for it. The
  [Aura seal check](#message-exchange-protocol-spec-msgexchange) is unnecessary.
- **No authenticated discovery.** Because a fetched batch is self-verifying against the on-chain `provides_root`, a peer
  can only *withhold*, not forge. So the MVP needs only **connectivity** — shared DHT + bootnode discovery
  (`/paranode`) + a para-ID provider lookup to find *some* source collator + the pull exchange protocol. The `audi`-key
  [Trust Model](#trust-model-for-collators) (`SignedCollatorAuthorityRecord`, `/spec-msg/light/2` key fetching, the
  two-signature verification) is **not** needed.
- **No acknowledgements.** No pre-inclusion speculation ⇒ no collator-based finality, and **no Low-Latency v2 dependency
  for this feature**.
- **No trust-domain / mode configuration.** Everything is `InclusionBased`; there is no per-source choice to configure.
- **No late-block proofs.** Staleness (the source advanced before the receiver's block is included) is handled by
  **resubmission** — build against the latest included root, rebuild if it no longer matches. MMR extension proofs are a
  later optimization.

**Phase 1 retains:** the cross-parachain P2P network (connectivity only), MMR accumulators + `provides` commitment,
`requires` commitment, the pull exchange protocol anchored to the included `provides_root`, and relay-chain
`requires`/`provides` matching (the core enforcement and security backstop).

### Phase 2 — `SpeculationMode::BackedBased`

Act once the source candidate is *backed* (before full inclusion/availability). Intermediate latency; introduces the
machinery to verify a backed-but-not-included root.

### Phase 3 — `SpeculationMode::BestBlock` (full speculative path)

Act on the source's *acknowledged*, pre-inclusion block. This re-introduces everything the MVP deferred, because the
receiver now acts on data the relay chain has **not** yet vouched for:

- authenticated discovery (the `audi`-key Trust Model);
- block-authorship verification (the Aura seal check) + the `messages_proof` against `source_header.state_root`;
- acknowledgement fetching/verification (`/spec-msg/light/2` generalized beyond keys) and the Low-Latency v2 dependency;
- late-block proofs for staleness across the (now parachain-block-cadence) speculative window.

## Implementation Considerations

### Relay Chain Runtime Changes

1. **New commitment types**: Add `provides` and `requires` to candidate commitments
2. **Commitment matching**: At inclusion time, verify that each `requires.expected_root` matches a `provides.root` from a currently backed or included candidate

Note: The relay chain has no MMR verification logic and does not track history. All proof verification happens in the PVF, which transforms commitments before the relay chain sees them. The relay chain only performs simple hash matching on current candidates.

### PVF Changes

Similar to how Low-Latency v2 introduces a separate PVF entry point for
scheduling information (verifying header chains and signed core selection),
speculative messaging requires PVF logic for processing late block proofs and
transforming commitments.

The PVF receives additional inputs via the POV (outside the block itself):

```rust
struct MessagingProofInputs {
    /// Late block proofs for each source chain where the block's requires
    /// references an older root than currently available
    late_block_proofs: Vec<LateBlockProof>,
}
```

The PVF then:

1. **Executes the block**: The block produces `requires` commitments based on
   the messages it processed (referencing the `provides` roots it was built
   against)

2. **Processes late block proofs**: For each `requires` commitment where a
   `LateBlockProof` is provided:
   - Verifies the proof connects the old root (block's `requires.expected_root`)
     to the new root (`proof.new_provides_root`)
   - Transforms the commitment to reference the new root

3. **Outputs transformed commitments**: The candidate commitments contain the
   (possibly transformed) `requires` that the relay chain can verify against
   currently available `provides`

```rust
fn process_messaging_commitments(
    block_requires: Vec<RequiresCommitment>,  // From block execution
    proof_inputs: &MessagingProofInputs,      // From POV
) -> Result<Vec<RequiresCommitment>, Error> {
    block_requires.into_iter().map(|req| {
        if let Some(proof) = find_proof_for_source(&proof_inputs, req.source) {
            // Transform: verify proof and update to current root
            process_late_block_requires(&req, proof)
        } else {
            // No transformation needed - block was built against current root
            Ok(req)
        }
    }).collect()
}
```

This follows the same pattern as the scheduling parent header chain in
Low-Latency v2: the PVF verifies proofs and transforms inputs so the relay chain
only sees commitments it can verify against current state.

### Parachain Runtime Changes

1. **MMR maintenance**: Append messages to outgoing MMR, emit provides
2. **Requires generation**: Track incoming message positions, emit requires
3. **Trust domain configuration**: Define trusted peers for speculative messaging
4. **Message processing**: Consume messages based on requires ranges
5. **Authority discovery**: Enable `pallet-authority-discovery` and register collators under `audi` so peers can read the key set from state (see [Trust Model](#trust-model-for-collators))

### Collator Changes

1. **Cross-chain message fetching**: Obtain messages from peer chains
2. **MMR proof generation**: Create extension proofs for late blocks
3. **Extended acknowledgement rules**: Verify message dependencies before acknowledging
4. **Super-block production** (if applicable): Coordinate multi-chain block production
5. **`audi` key + publishing worker**: Run with an `audi` session key and a *publishing* authority-discovery worker on the spec-msg network (today collators run authority-discovery only in `Discover` mode against the relay chain)

### Networking

1. **Message propagation**: Efficient cross-chain message dissemination
2. **Acknowledgement propagation**: Quick distribution of acknowledgement signatures
3. **MMR state sharing**: Allow peers to request MMR proofs
4. **Spec-msg network backend**: Stand up the dedicated Speculative Messaging Network (`/spec-msg/*` protocols) and run authority-discovery publish/lookup over it

> **Implementation status.** This is net-new wiring, not reuse of existing parachain behavior. While the
> `sc-authority-discovery` *verification logic and record schema* are reusable, no parachain runs authority-discovery
> for its own collator set today: a collator only runs an authority-discovery worker inside its embedded relay node, in
> `Role::Discover` mode against the **relay chain** network (`cumulus/client/relay-chain-minimal-node`), to find relay
> validators. The parachain node service crates (`cumulus/client/service`, `cumulus/polkadot-omni-node`, the parachain
> template) do not instantiate `sc-authority-discovery` at all, and stock Aura parachains do not run
> `pallet-authority-discovery`. Speculative messaging therefore needs net-new wiring around the reused
> authority-discovery core — enabling `pallet-authority-discovery` (collators under `audi`), standing up a shared
> spec-msg network, and running publish + cross-para lookup over it. See
> [Discovery Architecture: Two Layers](#discovery-architecture-two-layers) for the precise reuse boundary.

### Discovery Architecture: Two Layers

At its core, collator discovery **reuses authority-discovery**: the `SignedCollatorAuthorityRecord` schema *is* `dht-v3`,
its sign/verify logic is the existing `check_record_signed_with_*` helpers, and self-publishing is driven by the stock
`PublishAndDiscover` worker — no fork of `sc-authority-discovery`. The cross-chain functionality is then cleanly split
into two layers, **decoupled in responsibility** (the Discovery Layer rides on the same `NetworkService` the Network
Layer builds — they are not two separate networks):

**Network Layer (shared DHT).** Different parachains can't share a DHT because Substrate's discovery protocol names are
genesis-prefixed. So we stand up a separate `NetworkService` with a shared `genesis_hash` *domain separator* (see
[Spec-Msg Network Construction](#spec-msg-network-construction)), seed its bootnodes from the extended `/paranode`
response, and point the *reused* worker at it — which self-publishes the node's record under `sha256(audi)` and
discovers its *own* parachain's collators. Pure network construction; **zero AD-crate changes**. One small custom bit
also belongs here: each collator does an `ADD_PROVIDER` under its `para ID || epoch randomness` on this DHT — a
para-ID-keyed index (the stock worker only does records, not providers) that lets a peer find *some* collator of a chain
*before* it knows that chain's `audi` keys. These are the Stage-1 entry points.

**Discovery Layer (cross-parachain task).** Standard AD learns its targets from a local runtime call
(`client.authorities()`); you can't call a *foreign* chain's runtime, and the stock worker will never look up foreign
keys. So an **independent task**, sharing that same `NetworkService`:
1. finds some of Parachain B's collators via `GET_PROVIDERS` under B's `para ID || epoch randomness` entry-point key,
2. fetches from them a relay-anchored **light-client storage proof** of B's `AuthorityDiscovery::Keys` (see
   [Trust Model](#trust-model-for-collators)) — relying on a `/spec-msg/light/2` **server** on B's side, then
3. issues raw `get_value(sha256(b_key))` queries for the verified keys, reusing AD's `dht-v3` verification helpers.

**Hard dependency (both layers).** Participating parachains must run **`pallet-authority-discovery` with collators bound
under `audi`**, so the keys are both *active on the wire* (published to the DHT, for the Discovery Layer's lookup) and
*readable from state* (for its proof). The subsections below detail the Network Layer (construction + reused publish
worker) and the Discovery Layer (cross-para lookup task).

### Spec-Msg Network Construction

The Speculative Messaging Network is a third `NetworkService` standing alongside the collator's parachain network and
its embedded relay network. It is built exactly like any Substrate network — generic over `NetworkBackend` so it
matches the node's chosen backend (libp2p vs litep2p) — mirroring `build_collator_network` in
`cumulus/client/relay-chain-minimal-node`.

**The crux: a shared domain separator in place of the genesis hash.** The original problem (stated at the top of this
section) is that Substrate derives its discovery/notification protocol names from the **genesis hash** — Kademlia is
`/<genesis_hash_hex>/kad` (`substrate/client/network/src/discovery.rs`, `kademlia_protocol_name`) and block-announce is
`/<genesis>/block-announces/1` — so two parachains' names never match and their nodes can't form a common DHT.

The spec-msg network defeats this by constructing the backend with a **fixed, well-known constant as the
`genesis_hash`** (a domain separator, *not* any real chain genesis), identical across all participants. Every
participant then derives the *same* primary names — `/<spec-msg-domain>/kad`, `/<spec-msg-domain>/block-announces/1` —
and joins **one** DHT regardless of which parachain it serves. This shared `genesis_hash` is the actual lever.

A shared `protocol_id = "spec-msg"` additionally yields the *legacy* alias `/spec-msg/kad`
(`legacy_kademlia_protocol_name`), but that is secondary — the primary, genesis-derived name is what must match. Note
the `/spec-msg/*` naming is therefore literal only for the **request/response** protocols (`/spec-msg/exchange/1`,
`/spec-msg/light/2`), whose names are passed verbatim to `request_response_config` and are *not* genesis-prefixed; for
Kademlia, `/spec-msg/kad` is just the legacy alias of the real `/<spec-msg-domain>/kad`.

```rust
/// Build the dedicated Speculative Messaging Network and return its NetworkService.
/// Generic over the backend so it matches the node's choice, exactly like
/// `build_collator_network` in relay-chain-minimal-node.
fn build_spec_msg_network<Network: NetworkBackend<Block, Hash>>(
    node_key: NodeKeyConfig,             // persistent → STABLE PeerId (records bind to it)
    bootnodes: Vec<MultiaddrWithPeerId>, // INITIAL seed from the first relay-DHT `/paranode`
                                         // round; more are injected later (see below)
    spawn_handle: SpawnTaskHandle,
    metrics: Option<Registry>,
) -> Result<(Arc<dyn NetworkService>, SpecMsgChannels), Error> {
    // 1. Isolated, non-syncing DHT network config. `boot_nodes` is seeded here with the
    //    initial set; since we never sync, zero out light-client/sync peer slots
    //    (cf. minimal relay node).
    let mut net_config = FullNetworkConfiguration::<Block, Hash, Network>::new(
        &spec_msg_network_configuration(node_key, bootnodes),
        metrics.clone(),
    );

    // 2. Register the request/response protocols on THIS network
    //    (same helper shape as bootnodes' `bootnode_request_response_config`).
    let (exchange_cfg, exchange_rx) =
        spec_msg_req_resp::<Network>("/spec-msg/exchange/1", MAX_BATCH_SIZE);
    net_config.add_request_response_protocol(exchange_cfg);

    let (light_cfg, light_rx) =
        spec_msg_req_resp::<Network>("/spec-msg/light/2", MAX_PROOF_SIZE);
    net_config.add_request_response_protocol(light_cfg);

    // 3. Kademlia (primary `/<spec-msg-domain>/kad`, legacy alias `/spec-msg/kad`), identify and ping come from the backend's discovery
    //    behaviour, named from `protocol_id` below. Ensure DHT *record* support
    //    (PUT_VALUE/GET_VALUE — for authority-discovery records) and *provider* support
    //    (ADD_PROVIDER/GET_PROVIDERS — for the `para ID || epoch randomness` entry points)
    //    are both enabled; surface results via `network.event_stream(..)` → `DhtEvent`.

    // 4. The backend requires a block-announce notification config even though spec-msg
    //    carries no blocks: provide a minimal placeholder and KEEP its NotificationService
    //    alive (same constraint that bit the minimal relay node, issue #8474).
    let (block_announce_config, notification_service) = minimal_block_announce::<Network>(/* .. */);

    let params = sc_network::config::Params::<Block, Hash, Network> {
        role: Role::Full,
        executor: {
            let h = spawn_handle.clone();
            Box::new(move |f| h.spawn("spec-msg-libp2p", Some("spec-msg"), f))
        },
        fork_id: None,
        network_config: net_config,
        // NOT a real chain genesis — a fixed constant shared by all spec-msg participants,
        // so the derived protocol names collide into one namespace.
        genesis_hash: SPEC_MSG_DOMAIN,
        protocol_id: ProtocolId::from("spec-msg"),
        metrics_registry: metrics,
        block_announce_config,
        bitswap_config: None,
        notification_metrics,
    };

    let worker = Network::new(params)?;
    let service = worker.network_service();
    spawn_handle.spawn_blocking("spec-msg-network-worker", Some("spec-msg"), async move {
        let _keep_alive = notification_service; // keep the block-announce substream alive
        worker.run().await;
    });

    Ok((service, SpecMsgChannels { exchange_rx, light_rx }))
}

/// Request/response config helper — same shape as `bootnode_request_response_config`.
fn spec_msg_req_resp<N: NetworkBackend<Block, Hash>>(
    name: &str,
    max_response: u64,
) -> (N::RequestResponseProtocolConfig, async_channel::Receiver<IncomingRequest>) {
    let (inbound_tx, inbound_rx) = async_channel::bounded(INBOUND_CHANNEL_SIZE);
    let cfg = N::request_response_config(
        name.into(), Vec::new(), MAX_REQUEST_SIZE, max_response, TIMEOUT, Some(inbound_tx),
    );
    (cfg, inbound_rx)
}
```

`exchange_rx` feeds the [`/spec-msg/exchange`](#message-exchange-protocol-spec-msgexchange) responder; `light_rx` feeds
the `/spec-msg/light/2` storage-proof responder (Stage 1 server side). The returned `NetworkService` is what the
publishing worker below (and the cross-para lookup task) operate on.

**Bootnodes are a seed, not a one-shot.** The `bootnodes` argument only populates `boot_nodes` for the *initial* dial.
Relay-DHT discovery is ongoing — the per-parachain provider sets (`para ID || epoch randomness`) that spec-msg bootnodes
are derived from **rotate every epoch** and churn. The discovery task should therefore keep feeding freshly-discovered bootnodes into
the *running* network via `NetworkService::add_known_address(peer_id, addr)`, rather than relying solely on the
construction-time list (which would strand the node if that initial set goes stale). Seed at construction, then top up
continuously:

```rust
// relay-DHT discovery task, running for the life of the node:
for MultiaddrWithPeerId { peer_id, multiaddr } in newly_discovered_each_epoch {
    spec_msg_network.add_known_address(peer_id, multiaddr);
}
```

**Construction gotchas.**
- **Stable PeerId:** use a *persistent* node key for this network. The collator's `SignedCollatorAuthorityRecord` binds to this PeerId via `peer_signature`; a rotating key would invalidate published records.
- **Shared namespace, not per-genesis:** the domain separator and `protocol_id` must be identical across all participants — that is what lets different-genesis parachains share one DHT.
- **Backend must match the node:** instantiate `Network` as the same backend (`NetworkWorker` vs `Litep2pNetworkBackend`) the node is configured with, exactly as the minimal relay node branches on `network_backend`.
- **Block-announce placeholder:** the `Params` API demands a block-announce config and its `NotificationService` must be kept alive, even though no syncing happens.
- **Bootnodes come first, then keep coming:** the *initial* `boot_nodes` are populated from the first relay-DHT `/paranode` discovery, so that step runs before this constructor — but because the provider set rotates per epoch, the discovery task must continue injecting peers via `add_known_address` for the life of the node.

### PoC Wiring: Publishing Worker on the Spec-Msg Network

The publish side reuses `sc-authority-discovery` almost verbatim — modeled on
`cumulus/client/relay-chain-minimal-node`'s `build_authority_discovery_service`, but pointed at the spec-msg network,
in `PublishAndDiscover` mode, with the parachain's `audi` keystore:

```rust
/// Spawn a *publishing* authority-discovery worker on the Speculative Messaging Network.
///
/// - `para_client`:      parachain client; must implement `AuthorityDiscovery<Block>`
///                       (its runtime implements `AuthorityDiscoveryApi`, i.e.
///                       `pallet-authority-discovery` is enabled).
/// - `keystore`:         holds this collator's `audi` key (sr25519, KeyTypeId `audi`).
/// - `spec_msg_network`: the *dedicated* spec-msg `NetworkService` — NOT the parachain's
///                       main network and NOT the relay network. Its Kademlia is `/<spec-msg-domain>/kad`.
fn spawn_spec_msg_authority_discovery<Block, Client>(
    task_manager: &TaskManager,
    para_client: Arc<Client>,
    keystore: KeystorePtr,
    spec_msg_network: Arc<dyn NetworkService>,
    prometheus_registry: Option<Registry>,
) -> sc_authority_discovery::Service
where
    Block: BlockT,
    Client: sc_authority_discovery::AuthorityDiscovery<Block> + 'static,
{
    // DHT events (ValueFound / ValuePut / ...) from the spec-msg network specifically.
    let dht_event_stream = spec_msg_network
        .event_stream("spec-msg-authority-discovery")
        .filter_map(|e| async move {
            match e { Event::Dht(e) => Some(e), _ => None }
        });

    let (worker, service) = sc_authority_discovery::new_worker_and_service_with_config(
        sc_authority_discovery::WorkerConfig {
            // Mandatory for spec-msg: reject records not signed by their PeerId network identity.
            strict_record_validation: true,
            // Live network: don't advertise private IPs.
            publish_non_global_ips: false,
            ..Default::default()
        },
        para_client,                          // OWN collator audi set via AuthorityDiscoveryApi
        Arc::new(spec_msg_network.clone()),   // publish/lookup run on the spec-msg DHT
        Box::pin(dht_event_stream),
        // Publish our collator's record *and* discover same-parachain collators:
        sc_authority_discovery::Role::PublishAndDiscover(keystore),
        prometheus_registry,
        task_manager.spawn_handle(),
    );

    task_manager.spawn_handle().spawn(
        "spec-msg-authority-discovery",
        Some("spec-msg"),
        worker.run(),
    );
    service
}
```

This gets the publish side essentially for free: the stock worker pulls the node's **own** collator set from
`AuthorityDiscoveryApi::authorities()`, signs each `audi` key's record (keystore + libp2p network key), and
`PUT_VALUE`s it under `sha256(audi_pubkey)` on the spec-msg DHT. The record it publishes **is** Substrate's
`SignedAuthorityRecord` (`dht-v3`) — i.e. exactly the `SignedCollatorAuthorityRecord` defined earlier — so no custom
record type is needed. With `PublishAndDiscover` it also discovers **same-parachain** collators on the spec-msg DHT.

**Cross-parachain discovery is NOT covered by this worker.** The stock worker only ever looks up the keys returned by
its **own** `client.authorities()`, and `Service::get_addresses_by_authority_id` only returns already-cached entries.
Resolving *Parachain B's* collators therefore needs separate plumbing on the **same** `spec_msg_network`:

1. **Stage 1 (key set):** the light-client `/spec-msg/light/2` read → B's verified `audi` keys (from the storage proof, not from any runtime call).
2. **Stage 2 (lookup):** drive raw DHT ops for those foreign keys — `spec_msg_network.get_value(&sha256(b_audi_key))`, consume `DhtEvent::ValueFound`, and verify with the same `dht-v3` helpers (`check_record_signed_with_authority_id` / `check_record_signed_with_network_key`).

For a PoC the cleanest split is to **reuse the stock worker above for the node's own record** and add a **small custom
task** for steps 1–2. Because that task shares the `NetworkService`, the records published by every participant's stock
worker are exactly what its cross-para lookups fetch and verify. (Extending `sc-authority-discovery` to accept
externally-supplied lookup keys is the longer-term alternative, not needed for the PoC.)

**Wiring gotchas.**
- Three networks now coexist in a collator — parachain main, embedded relay (already runs discover-only AD), and this new spec-msg network — each with its own `NetworkService` and event stream; don't cross the wires.
- `para_client` must implement `AuthorityDiscovery<Block>`, which requires the **runtime** to implement `AuthorityDiscoveryApi` (the net-new runtime piece).
- The keystore must actually contain the collator's `audi` sr25519 key, or `publish_ext_addresses` finds nothing to publish and silently no-ops.
- `Arc<dyn NetworkService>` satisfies the `NetworkProvider` bound the factory expects (same pattern as the minimal-node call site).

### Network PoC Bring-Up Checklist

A discovery + exchange PoC (no relay-chain commitment verification) comes together in this order. Items marked
**[new]** are net-new code; the rest are reuse/configuration.

1. **Runtime:** enable `pallet-authority-discovery`; register collators under `audi`. *(reuse)*
2. **Keystore:** ensure each collator holds its `audi` sr25519 key. *(config)*
3. **`/paranode` response extension [new]:** extend the `cumulus/client/bootnodes` advertisement task so its `/paranode` response *also* carries the node's spec-msg multiaddresses. No new DHT registration: this reuses the per-parachain bootnode entries every parachain already publishes, so the participant set *is* the bootstrapping seed.
4. **Bootnode discovery — relay side:** for each HRMP channel peer (`HrmpIngressChannelsIndex`/`HrmpEgressChannelsIndex` from relay state), do the standard RFC 08 lookup (providers under `para ID || epoch randomness`, then `/paranode`) and read the spec-msg multiaddresses from the extended response. *(reuse of the existing `/paranode` client path)*
5. **Build the spec-msg network [new]:** `build_spec_msg_network` above, seeded with the bootnodes from step 4 (and kept fed via `add_known_address` as the per-epoch provider sets rotate).
6. **Publish worker:** `spawn_spec_msg_authority_discovery` on that network. *(reuse)*
7. **Entry-point registration [new]:** each collator `ADD_PROVIDER`s under its own `para ID || epoch randomness` on the spec-msg DHT — the para-ID-keyed index used as Stage-1 entry points. The stock worker only publishes records (not providers), so this is custom.
8. **Light-client server — `/spec-msg/light/2` [new]:** run a responder that answers `RemoteReadRequest`s from the `light_rx` channel, generating storage proofs at the *included* head. Restrict it to the `AuthorityDiscovery::Keys` key (or a small allowlist) to bound the DoS surface — it should not be a general state-proof oracle.
9. **Cross-para lookup task [new]:** Stage 1 — `GET_PROVIDERS` under B's `para ID || epoch randomness` to find entry points, then a `/spec-msg/light/2` read for B's `audi` keys; Stage 2 — `get_value(sha256(b_audi_key))` + `dht-v3` verification, as described above.
10. **Exchange responder — `/spec-msg/exchange/1` [new]:** serve `GetMessages` from the `exchange_rx` channel; receivers pull per the [Message Exchange Protocol](#message-exchange-protocol-spec-msgexchange).

Steps 1–9 stand up authenticated cross-parachain discovery (the address book); step 10 adds the message transfer
itself. Everything here is backed by existing Substrate machinery — the **[new]** items are integration glue, not new
protocols — so the network PoC is unblocked.

---

## Security Analysis

### Threat: Fake Provides

**Attack**: Malicious collator claims provides root that doesn't match actual
messages.

**Mitigation**: Receiving chains verify actual message content against the
requires commitment. The MMR root commits to specific message hashes. Any
mismatch is detectable.

### Threat: Invalid Extension Proof

**Attack**: Late block includes a fabricated extension proof.

**Mitigation**: Extension proofs are cryptographically verified by the PVF.
Invalid proofs cause candidate validation to fail.

### Threat: Message Replay/Skip

**Attack**: Receiving chain processes messages out of order or skips messages.

**Mitigation**: The parachain runtime tracks which messages have been processed
and enforces consecutive processing. This is internal to the parachain—the relay
chain only sees the resulting `requires` commitment.

### Threat: Acknowledgement Without Verification

**Attack**: Collator acknowledges a block without verifying message
availability.

**Mitigation**: If the block later fails inclusion due to unmet requires, the
acknowledging collator violated Low-Latency v2 rules and is slashable.

### Threat: Super-Chain Collusion

**Attack**: All collators in a super-chain collude to equivocate across chains.

**Mitigation**: Same as Low-Latency v2—requires at least one honest collator to
submit proofs. For high-value super-chains, ensure diverse collator set.

---

## Conclusion

Speculative Messaging replaces HRMP with a more scalable, lower-latency
alternative that:

- **Eliminates relay chain message storage**: Messages flow off-chain; only
  commitments are verified on-chain
- **Enables parachain-speed messaging**: Within trust domains, messaging latency
  drops to parachain block times
- **Supports super chains**: Tightly coupled chains can exchange messages within
  the same block production cycle
- **Gracefully handles late blocks**: MMR extension proofs allow blocks with
  older requirements to still be included
- **Maintains horizontal scaling**: Even for super chains: Full nodes can still
  be per chain and don't need to keep the entire state or process all sub-chain
  blocks.

Combined with Low-Latency Parachains v2, this positions Polkadot to offer user
experiences competitive with monolithic chains while preserving its core value
propositions of decentralization, security, and horizontal scalability.

---

## Appendix A: Separation of Concerns

Different layers handle different data:

| Layer | Data | Purpose |
|-------|------|---------|
| **Candidate Commitments** | `provides.root`, `requires.{source, expected_root}` | Relay chain verification |
| **Late Block Proofs (POV)** | Merkle proofs, MMR extension proofs | Prove old requires valid under new provides |
| **Parachain Runtime** | MMR structures, message positions, last processed indices | Internal bookkeeping |
| **Off-Chain (Collators)** | Actual messages, inclusion proofs | Message delivery |

The relay chain only sees hashes. It verifies that provides/requires match (or
that a valid proof exists). It never sees message contents, MMR sizes, or
processing positions.

## Appendix B: MMR Extension Proof Details

An MMR extension proof demonstrates that a newer MMR root extends an older one:

```rust
/// MMR extension proof structure
struct MMRExtensionProof {
    /// Peaks of the old MMR
    old_peaks: Vec<Hash>,
    
    /// Peaks of the new MMR  
    new_peaks: Vec<Hash>,
    
    /// Nodes connecting old peaks to new peaks
    /// (proves old peaks are prefix of new structure)
    connecting_nodes: Vec<Hash>,
}

impl MMRExtensionProof {
    fn verify(
        &self,
        old_root: Hash,
        new_root: Hash,
    ) -> bool {
        // 1. Verify old_peaks produce old_root
        let computed_old_root = bag_peaks(&self.old_peaks);
        if computed_old_root != old_root {
            return false;
        }
        
        // 2. Verify new_peaks produce new_root
        let computed_new_root = bag_peaks(&self.new_peaks);
        if computed_new_root != new_root {
            return false;
        }
        
        // 3. Verify old structure is prefix of new structure
        // using connecting_nodes
        verify_prefix_relationship(
            &self.old_peaks,
            &self.new_peaks,
            &self.connecting_nodes,
        )
    }
}
```

## Appendix C: Acknowledgement Rule Summary

| Rule | Description |
|------|-------------|
| Base rules | All rules from Low-Latency v2 |
| Message verification | Don't acknowledge if dependent messages aren't confirmed |
| Same super-chain | Messages from co-authored blocks are immediately trusted |
| Same trust domain | Wait for source block acknowledgement |
| Cross-domain | Wait for source block inclusion on relay chain |
| Cycle prevention | No intra block communication apart from super chains (wait for next block, not inclusion) |

## Appendix D: Commitment Schema Summary

```rust
// === CANDIDATE COMMITMENTS (minimal, verified by relay chain) ===

struct ProvidesCommitment {
    root: Hash,  // Top-level Merkle root over per-destination MMR roots
}

struct RequiresCommitment {
    source: ParaId,
    expected_root: Hash,
}

// === LATE BLOCK PROOF (in POV, not commitments) ===

struct LateBlockProof {
    source: ParaId,
    old_subtree_root: Hash,
    old_subtree_proof: MerkleProof,
    new_provides_root: Hash,
    new_subtree_root: Hash,
    new_subtree_proof: MerkleProof,
    subtree_extension: Option<MMRExtensionProof>,
}

// === PARACHAIN RUNTIME STATE (internal, not on relay chain) ===

// Sender tracks: per_destination MMRs (keyed by (destination, channel instance)),
//                current top-level root
// Receiver tracks: per (source, channel instance) — last_processed position, last seen roots

// === OFF-CHAIN (between collators) ===

// MessageBatch: source, source_block, source_header, messages_proof (light-client),
//               provides_root, subtree_root, subtree_proof, messages
```

## Appendix E: Comparison of Messaging Modes

| Mode (`SpeculationMode`) | Latency | Trust | Use Case |
|------|---------|-------|----------|
| Super-chain (intra-block) | < 1 block | Same collator set | Tightly coupled shards |
| `BestBlock` (acknowledged) | ~1-2 blocks | Source collators (+ llv2 acks) | Fast cross-chain DeFi |
| `BackedBased` | ~2 relay blocks | Relay chain (backed) | Lower latency, no collator trust |
| `InclusionBased` | ~2-3 relay blocks | Relay chain only | Default; cross-domain/untrusted; **MVP** |
| HRMP (legacy) | ~3+ relay blocks | Relay chain only | Deprecated |
