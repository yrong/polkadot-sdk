# XCMP MMD POC Documentation

This directory contains documentation for the XCMP MMD (Merkle Mountain Range based cross-chain messaging) Proof of Concept implementation.

## 📚 Documentation Files

### [XCMP-MMD-POC.md](./XCMP-MMD-POC.md)
**Main implementation guide** - Start here for a comprehensive overview.

Contains:
- Implementation status and progress
- Design decisions (Option B: well-known key approach)
- Data structures (MessageWithProof, OutboxLeaf, XcmpMmdDigest)
- 8-step verification flow
- Key implementation details
- Hard bounds and constants
- File locations in codebase
- Known limitations
- Success criteria
- Next steps for production

### [blog-spec.md](./blog-spec.md)
**Original specification** - The blog post that defines the protocol.

Contains:
- Problem and motivation
- POC design and must-haves
- "Matryoshka" proof stack structure
- Implementation details (low level)
- Source and destination pallet specifications
- Appendix A: Relay MMR root access (Option A vs B)

### [spec-comparison.md](./spec-comparison.md)
**Spec vs implementation comparison** - Detailed verification of consistency.

Contains:
- Core design consistency verification
- Data structure comparison
- 8-step verification detailed comparison
- Known differences (intentional)
- POC simplifications documented
- Overall assessment

### [xcmp-mmd-todo.md](./xcmp-mmd-todo.md)
**Task tracker** - Detailed checklist with progress tracking.

Contains:
- Phase-by-phase breakdown (Phases 0-7)
- Checkboxes for completed tasks
- Progress summary
- POC completion status
- Estimated remaining time

## 🛠️ Tools

Development utilities are located in the top-level tools directory:
- **Location:** `/tools/xcmp-mmd/`
- **Contents:** MMR storage key calculator and related utilities
- See [tools/xcmp-mmd/README.md](../../../tools/xcmp-mmd/README.md) for details

## 🎯 Quick Start

1. **Understanding the Protocol:** Read [blog-spec.md](./blog-spec.md) for the original specification
2. **Implementation Overview:** Read [XCMP-MMD-POC.md](./XCMP-MMD-POC.md) for implementation details
3. **Verify Consistency:** Check [spec-comparison.md](./spec-comparison.md) to see how implementation matches spec
4. **Track Progress:** Review [xcmp-mmd-todo.md](./xcmp-mmd-todo.md) for completion status

## 📦 Implementation Locations

### Pallets
- **Outbox:** `cumulus/pallets/xcmp-mmd-outbox/`
- **Inbox:** `cumulus/pallets/xcmp-mmd-inbox/`
- **Integration Tests:** `cumulus/pallets/xcmp-mmd-integration-tests/`

### Primitives
- **Core Types:** `cumulus/primitives/xcmp-mmd/`

### Infrastructure
- **Well-Known Keys:** `polkadot/primitives/src/v9/mod.rs`
- **Collator Client:** `cumulus/client/parachain-inherent/src/lib.rs`

## ✅ POC Status

**Core implementation is complete:**
- ✅ Phase 0: MMR root in well-known keys
- ✅ Phase 1: XcmpMmdOutbox pallet (9 tests passing)
- ✅ Phase 2: XcmpMmdInbox pallet (4 tests passing)
- ✅ Phase 3: Primitives crate
- ✅ Phase 4: Integration tests (7 tests passing)

**Remaining for production:**
- ⏳ Phase 5: Relayer tool
- ⏳ Phase 6: Zombienet testing
- ⏳ Phase 7: Documentation (partial)

## 🔗 References

- **Polkadot SDK:** https://github.com/paritytech/polkadot-sdk
- **Forum Discussion:** [XCMP Design Discussion](https://forum.polkadot.network/t/xcmp-design-discussion/7328)
- **Original Blog Post:** Included as [blog-spec.md](./blog-spec.md)

## 📝 Notes

This is a **Proof of Concept** implementation demonstrating the feasibility of MMR-based cross-chain messaging. Known limitations are documented in [XCMP-MMD-POC.md](./XCMP-MMD-POC.md#-known-limitations-poc-scope).
