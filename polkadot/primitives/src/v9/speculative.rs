// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Relay-visible speculative-messaging commitments (Phase 1).
//!
//! The relay chain only sees the **commitments** carried in the
//! `UMPSignal::ProvidesRoots`/`RequiresRoots` signals (inside
//! `CandidateCommitments.upward_messages`) — a sender's flat `provides` set and a
//! receiver's `requires` set, both canonical sorted `CommitmentSet`s of
//! `(ParaId, Hash)`. The aliases for those are defined here.
//!
//! Everything else — the low-level primitives (`CommitmentSet`, `OutgoingMessage`,
//! `hash_leaf`, the MMR `SpecMerge`) and the parachain-side off-chain types
//! (`MessageBatch`, `LateBlockProof`, `SubtreeExtension`, `SpeculativeIngress`,
//! `SourceState`, `SpecHasher`, …) — lives in `cumulus-primitives-spec-messaging`,
//! since the relay chain never decodes them. See
//! `docs/speculative-messaging-impl-design.md`.

use super::commitment_set::CommitmentSet;

/// The API version at which speculative messaging support was introduced.
/// Collators and runtimes use this to gate speculative field population.
pub const SPECULATIVE_API_VERSION: u32 = 10;

/// Maximum number of destination parachains a sender can commit to in one block.
/// Bounds the size of the `provides` commitment.
pub const MAX_DESTINATIONS_PER_BLOCK: u32 = 128;

/// Maximum number of source parachains a receiver can consume from in one block.
/// Bounds the size of the `requires` commitment.
pub const MAX_SOURCES_PER_BLOCK: u32 = 128;

/// A sender's outbound commitment for one block: a canonical, sorted set of
/// `(destination, subtree_root)` entries. This flat set **is** the top-level
/// commitment — there is no Merkle root over the subtree roots.
pub type ProvidesCommitment = CommitmentSet<MAX_DESTINATIONS_PER_BLOCK>;

/// A receiver's inbound dependencies for one block: a canonical, sorted set of
/// `(source, expected_subtree_root)` entries, where `expected_subtree_root` is
/// the source's per-destination subtree root *for this receiver*. The relay chain
/// matches each entry against `ProvidesRoots[source].get(receiver)`.
pub type RequiresCommitment = CommitmentSet<MAX_SOURCES_PER_BLOCK>;
