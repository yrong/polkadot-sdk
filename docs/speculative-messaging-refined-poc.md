# Speculative Messaging — Refined POC Implementation

This document details the refined, "network-ready" implementation of the Speculative Messaging POC. It supersedes the theoretical gaps in [speculative-messaging-impl-design.md](speculative-messaging-impl-design.md) with the actual, verified logic now present in the codebase.

## 1. Core Workflow Overview

The system enables parachains to send messages "speculatively" (before inclusion in the relay chain) while maintaining cryptographic safety through the relay chain's inclusion process.

### A. Sender Workflow (Outbox)
1.  **Message Recording:** Messages are intercepted from XCMP and appended to per-destination MMRs (Merkle Mountain Ranges).
2.  **Historical Tracking:** Every block, the top-level `provides` root and all subtree states are recorded in storage, retaining a 256-block window.
3.  **Proof Generation:** When a receiver requests a batch but has an older view of the sender, the sender generates a **Late Block Proof (LBP)**. This connects the receiver's "old" root to the sender's "new" root via MMR extension peaks.

### B. Collator/Relayer Workflow (Networking)
1.  **Direct Fetching:** The receiver's collator queries the sender's Runtime API (`SpeculativeOutboxApi`) directly.
2.  **Batch & Proof Retrieval:** It retrieves the message batch and, if necessary, the LBP required to promote its local requirements to the sender's current state.
3.  **PoV Wrapping:** The collator wraps the block data and LBPs into a `ParachainBlockDataV4` envelope for the relay chain validators.

### C. PVF/Relay Chain Workflow (Verification)
1.  **LBP Transformation:** During PVF execution, `apply_messaging_proofs` verifies the LBPs. If valid, it "promotes" the candidate's requirements to match the sender's latest on-chain root.
2.  **Inclusion Matching:** The relay chain's `inclusion` pallet matches the (possibly promoted) `requires` commitments against the tracked `ProvidesRoots` to ensure message availability.

---

## 2. Key Component Changes

### Pallet Speculative Outbox
- **Storage:** Migrated from naive `Vec<H256>` to structured `MMRNodes` and `MMRSize` for O(log N) efficiency.
- **History:** Added `HistoricalProvidesRoots` and `HistoricalSubtreeState`.
- **API:** Fully implemented `generate_late_block_proof` and `mmr_extension_proof`.

### Pallet Speculative Inbox
- **Inherent:** Implemented `ingest_verified_messages` which reconstructs source subtrees and dispatches payloads to the local XCMP handler.
- **State:** Tracks `last_processed` indices per source to ensure no gaps or reorders.

### Parachain System (PVF)
- **Decoding:** Updated `validate_block` to support `ParachainBlockDataV4`.
- **Verification:** Implemented `verify_mmr_extension` using peak-bagging logic to confirm source chain progress trustlessly.

### Polkadot Primitives (v10)
- **Types:** Added `CandidateReceiptV4`, `LateBlockProof`, and `MMRExtensionProof`.
- **Versioning:** Integrated `SpeculativeCommitments` into `CandidateCommitments` using `TrailingOption` for backward compatibility.
