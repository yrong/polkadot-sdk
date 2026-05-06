# Speculative Messaging: Proposal vs POC Design — Issues & Diffs

This document captures differences between the
[Implementation Proposal](Speculative%20Messaging_%20Implementation%20Proposal.original.md)
and the consolidated [POC Implementation Design](speculative-messaging-impl-design.md)
that emerged from detailed technical review. The proposal covers the full
three-mode vision; the POC design narrows to inclusion-based messaging only,
correcting several technical details.

## 1. On-Chain Payload Storage (Proposal §3.1)

**Proposal:** Stores full message payloads in runtime storage:

```rust
pub type OutgoingMessages<T>: DoubleMap<ParaId, u64, Bytes>; // (dest, leaf_index) → payload
```

**POC design:** Payloads are kept off-chain. The runtime stores only MMR state
(`leaf_count`, `root`, `nodes` — hashes only). The relayer/provider reads payload
bytes from finalized block events and constructs `MessageBatch` structs.

**Impact:** This is the core value proposition. Storing payloads on-chain
(defeats the goal of removing message data from chain state — even if the
storage moves from the relay chain to the sender parachain, the bytes are still
in state. The MMR only needs the 32-byte payload hash for proof verification.

## 2. LateBlockProof Location (Proposal §3.2)

**Proposal:** Puts `late_block_proofs` inside `MessagingInherent` (block body),
alongside batches.

**POC design:** `LateBlockProof` goes in the PoV (appended after block data).
The collator prechecks proofs locally and uses the transformed root in candidate
commitments. The PVF independently reads proofs from the PoV during
`validate_block` and confirms the transformation — commitments hash mismatch if
the collator's precheck and the PVF's verification disagree. Matches the
high-level design's PVF transformation model.

## 3. Custom `MessagingInherent` vs Standard `ProvideInherent` (Proposal §3.2)

**Proposal:** Defines a custom `MessagingInherent` struct bundling batches and
proofs, implying a bespoke inherent extraction and injection path:

```rust
struct MessagingInherent {
    batches: Vec<MessageBatch>,
    late_block_proofs: Vec<LateBlockProof>,
}
```

**POC design:** Uses the standard `ProvideInherent` pattern already established
by `ParachainSystem::set_validation_data`. The pallet declares an
`INHERENT_IDENTIFIER`, the collator puts `SpeculativeIngress` under that key via
the existing inherent-data pipeline, and `create_inherent` decodes it into
`ingest_verified_messages`. No custom inherent struct, no new injection path.

**Why simpler:** Cumulus already has the full `ProvideInherent` lifecycle —
`create_inherent_data_with_rp_offset` assembles inherents, the proposer includes
them, and `validate_block` replays them. Our pallet plugs into this directly
rather than defining a parallel mechanism. The `SpeculativeIngress` struct is
just the typed payload under a standard inherent key.

## 4. Relay Chain Enforcement (Proposal §1)

**Proposal:** Matches against "currently backed/included" provides roots.

**POC design:** Matches against same-block enacted provides roots or the latest
persisted `ProvidesRoots` entry. A candidate that is merely backed but not yet
enacted does not satisfy a dependency — its provides root hasn't been committed
to relay state yet.

## 5. Collator: Proof Fetching vs Generation (Proposal §3.5 #3)

**Proposal:** "generate the extension proof."

**POC design:** The receiver collator fetches the proof from the source side or
provider. It does not generate it — only the source side has the full MMR peaks
needed for extension proofs.

## 6. Networking Model (Proposal §3.6)

**Proposal:** Native collator-to-collator P2P protocols for message and
acknowledgement propagation.

**POC design:** Relayer/provider model with HTTP + static config for the POC.
Native P2P is a future optimization. The acknowledgement propagation protocol is
additionally gated on LLv2. See also issue #7 (relayer framing).

## 7. Relayer Framing (Proposal §3.8)

**Proposal:** Frames the relayer as a "fallback" for eventual-delivery
guarantees. Proposes an embedded relayer subservice inside the node binary.

**POC design:** The relayer/provider is the primary transport in the POC — a
separate process serving both `MessageBatch` data and `LateBlockProof` data via
HTTP. The embedded-relayer-in-node model is a valid production hardening path
(Snowbridge-proven pattern), but the POC starts simpler: a standalone process
avoids coupling to node-side service wiring and allows independent
testing/debugging.

## 8. PVF Logic (Proposal §3.3)

**Proposal:** `LateBlockProof` in PoV, separate PVF entry point for proof
handling following LLv2's scheduling-parent pattern.

**POC design:** Agrees on the PoV approach but uses inline parsing instead of a
separate entry point. Proofs are appended to the PoV after block data
(length-prefixed); `validate_block` reads and verifies them as a post-execution
step before returning the validation result.

**Why not a separate entry point.** LLv2's separate-entry-point pattern exists
for inputs that are independent of block execution (scheduling parent headers,
core assignments) — they determine *whether* the block is valid. Late Block
Proofs are different: they transform an output the block execution already
produced (the `requires` root). Processing that transformation inside
`validate_block` after execution is the natural place — it's a post-processing
step on the execution result, not an independent validation gate. A separate
entry point would need to receive the execution output and couple two entry
points, which adds complexity without benefit. This isn't a POC shortcut to fix
later; it's the right architecture for this specific problem.

## 9. Collator Responsibilities (Proposal §3.5)

**Proposal:** #1 peer connections, #3 "generate" extension proof into PoV,
#4 LLv2 acknowledgement rules, #5 super-block production.

**POC design:**
- #1: fetches from relayer/provider HTTP, not direct peer connections
- #3: obtains proofs from the source/provider, carries them in the PoV
  (appended after block data), not generated by the receiver collator
- #4: deferred (gated on LLv2)
- #5: deferred (gated on shared collator set infrastructure)

## 10. Gated Delivery Modes (Proposal §2)

The proposal lists all three delivery modes as in-scope. Two are gated on
infrastructure that doesn't exist yet:

| Mode | Gate |
|---|---|
| Speculative (acknowledged) | Requires Low-Latency v2 collator acknowledgement signatures, not yet implemented |
| Super-chain (intra-block) | Requires shared collator set infrastructure, not yet existing |

The POC implements inclusion-based messaging only — the one mode that works with
today's codebase. Collator responsibilities #4 (acknowledgement rules) and #5
(super-block production) in proposal §3.5 are similarly gated.

## 11. LLv2 Implementation Status

The Low-Latency v2 design (`docs/low-latency-v2-design.md`) is a pure design
document. No LLv2 code exists in the codebase: no `scheduling_parent` field, no
acknowledgement signature types, no decoupled candidate types. The `vstaging`
primitives module is empty. The speculative (acknowledged) delivery mode and all
acknowledgement-rule extensions are therefore blocked until LLv2 ships.

## Summary

| # | Issue | Severity | Fix in POC |
|---|---|---|---|
| 1 | Payloads stored on-chain | **Must fix** — contradicts core goal | Off-chain via relayer |
| 2 | LateBlockProof location | Aligned — both use PoV | PoV, appended after block data |
| 3 | Custom MessagingInherent vs standard ProvideInherent | Simplified — reuse existing pattern | Standard `ProvideInherent` with `SpeculativeIngress` |
| 4 | "backed" vs "enacted" in relay enforcement | Precision fix | Same-block enacted or persisted |
| 5 | "generate" vs "obtain" extension proof | Precision fix | Fetch from source/provider |
| 6 | Networking: P2P vs relayer/provider | Scope simplification | Relayer HTTP + static config for POC |
| 7 | Relayer as "fallback" vs primary | Framing / production path | Separate process for POC; embedded subservice for production |
| 8 | PVF Logic: separate entry point | Inline parsing is architecturally correct | Append to PoV, verify in validate_block post-execution |
| 9 | Collator: P2P connections + PoV reference | Scope simplification | Relayer HTTP; proofs in PoV, fetched not generated |
| 10 | Gated delivery modes listed as in-scope | Scope clarification | Inclusion-based only for POC; two modes deferred |
| 11 | LLv2 assumed available | Factual | LLv2 not yet implemented |
