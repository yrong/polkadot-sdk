# Speculative Messaging Follow-Up Roadmap

This document captures the work that sits **after** the minimal happy-path POC.
It is intentionally separate from `speculative-messaging-impl-design.md` so the
POC stays narrow and implementation-ready.

## 1. Late Block Proofs

- Handle lagging destinations and core-on-demand chains robustly.
- Transform source `provides` into receiver-ready `requires` via proof-bearing validation.
- Define proof size bounds and failure behavior.

## 2. Delivery Bounds and Pruning

- Define what "eventual delivery" means operationally.
- Bound maximum message age and maximum catch-up per block.
- Define message retention windows.
- Add pruning triggers and grace periods.
- Bound catch-up work per block.

## 3. Rate Limiting and DoS Protection

- Add per-channel message and byte limits.
- Enforce limits on outbox and inbox paths.
- Benchmark weight impact of limit checks.

## 4. Proof and Storage Bounds

- Bound late-block-proof and MMR-extension-proof sizes.
- Define fallback behavior when proofs are too large to include.
- Confirm relay-chain storage remains bounded to latest-per-para data only.

## 5. Trust Domains and Acknowledgements

- Define when speculative mode is allowed.
- Clarify unilateral trust, revocation, and fallback behavior.
- Integrate acknowledgements when Low-Latency v2 is available.

## 6. Migration and Coexistence

- Define how HRMP and speculative messaging run in parallel.
- Clarify per-channel or per-parachain enablement.
- Add rollback and upgrade sequencing guidance.

## 7. Production Hardening

- Formalize PoV / validation ABI extensions.
- Tighten proof size and storage growth guarantees.
- Expand adversarial testing and security review scope.

## 8. Optional Future Directions

- Super-chain / intra-block messaging.
- Relaxed or unordered delivery semantics.
- Enhanced pruning and garbage collection strategies.
