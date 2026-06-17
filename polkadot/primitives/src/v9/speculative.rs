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

//! Speculative messaging primitives (Phase 1 — inclusion-based messaging).
//!
//! The canonical primitives — `CommitmentSet`, `OutgoingMessage::hash_leaf`, and
//! the `SpecMerge` MMR merge — live in `cumulus-primitives-spec-messaging`. This
//! module re-exports the relay-visible commitment types as concrete aliases and
//! defines the off-chain wire types (`MessageBatch`, `LateBlockProof`,
//! `SpeculativeIngress`) used by collators and the receiver runtime.
//!
//! Design: the top-level `provides`/`requires` commitments are **flat**
//! `CommitmentSet`s of `(ParaId, Hash)` (no two-level Merkle tree), and the
//! per-destination subtree MMR uses `mmr_lib` with `SpecMerge` (`blake2_256` +
//! domain tags). See `docs/speculative-messaging-impl-design.md`.

use alloc::vec::Vec;

use codec::{Decode, DecodeWithMemTracking, Encode};
use scale_info::TypeInfo;
use sp_core::ConstU32;

use cumulus_primitives_spec_messaging::{
	commitment_set::CommitmentSet, outgoing_message::OutgoingMessage as SpecOutgoingMessage,
};

use super::{BlockNumber, Hash, Id};

/// The API version at which speculative messaging support was introduced.
/// Collators and runtimes use this to gate speculative field population.
pub const SPECULATIVE_API_VERSION: u32 = 10;

/// Maximum number of destination parachains a sender can commit to in one block.
/// Bounds the size of the `provides` commitment.
pub const MAX_DESTINATIONS_PER_BLOCK: u32 = 128;

/// Maximum number of source parachains a receiver can consume from in one block.
/// Bounds the size of the `requires` commitment.
pub const MAX_SOURCES_PER_BLOCK: u32 = 128;

/// Maximum size in bytes of a single speculative message payload.
pub const MAX_SPECULATIVE_MESSAGE_LEN: u32 = 102_400;

/// Bound for a single speculative message payload.
pub type MaxSpeculativeMessageLen = ConstU32<MAX_SPECULATIVE_MESSAGE_LEN>;

/// A single outbound speculative message (the canonical crate type, with a
/// concrete payload bound). Identified by `(source, destination, position)` and
/// hashed into an MMR leaf via [`OutgoingMessage::hash_leaf`].
pub type OutgoingMessage = SpecOutgoingMessage<MaxSpeculativeMessageLen>;

/// A sender's outbound commitment for one block: a canonical, sorted set of
/// `(destination, subtree_root)` entries. This flat set **is** the top-level
/// commitment — there is no Merkle root over the subtree roots.
pub type ProvidesCommitment = CommitmentSet<MAX_DESTINATIONS_PER_BLOCK>;

/// A receiver's inbound dependencies for one block: a canonical, sorted set of
/// `(source, expected_subtree_root)` entries, where `expected_subtree_root` is
/// the source's per-destination subtree root *for this receiver*. The relay chain
/// matches each entry against `ProvidesRoots[source].get(receiver)`.
pub type RequiresCommitment = CommitmentSet<MAX_SOURCES_PER_BLOCK>;

/// Deterministic ingress payload carried in the parachain block body.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct SpeculativeIngress {
	/// Verified batches selected by the collator for this block.
	pub batches: Vec<MessageBatch>,
}

/// A message batch sent off-chain between collators.
///
/// With the flat commitment there is no top-level inclusion proof: the receiver's
/// `subtree_root` is matched directly by the relay chain against the source's
/// committed `(receiver, subtree_root)` entry.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct MessageBatch {
	/// Source parachain.
	pub source: Id,
	/// Relay-chain block number associated with the source batch.
	pub source_relay_parent_number: BlockNumber,
	/// The per-destination MMR root for the receiver. Recorded by the receiver in
	/// `RequiresCommitment` and matched by the relay chain.
	pub subtree_root: Hash,
	/// Number of MMR nodes in the per-destination subtree when the proof was
	/// generated. Required to reconstruct the `mmr_lib::MerkleProof`.
	pub subtree_mmr_size: u64,
	/// Combined `mmr_lib` inclusion proof (with `SpecMerge`) over every leaf in
	/// `messages`, against `subtree_root`. Verified by
	/// `MerkleProof::<Hash, SpecMerge>::new(subtree_mmr_size, messages_proof)
	/// .verify(subtree_root, leaves)` where each leaf is
	/// `(leaf_index_to_pos(position), msg.hash_leaf())`.
	pub messages_proof: Vec<Hash>,
	/// The messages, sorted by `position` ascending. Verified collectively by
	/// `messages_proof` against `subtree_root`.
	pub messages: Vec<OutgoingMessage>,
}

/// Included in the receiver candidate's PoV when the block was built against an
/// older source subtree root than what's persisted in `ProvidesRoots`.
///
/// With the flat commitment both subtree roots are directly observable as
/// `(receiver, root)` entries in the source's old/new `ProvidesCommitment`, so
/// there is no top-level inclusion proof — only the append-only MMR extension.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct LateBlockProof {
	/// The source parachain this proof covers.
	pub source: Id,
	/// The per-destination subtree root the receiver block was built against.
	pub old_subtree_root: Hash,
	/// The source's current per-destination subtree root for this receiver;
	/// must match `ProvidesRoots[source].get(receiver)` at enactment.
	pub new_subtree_root: Hash,
	/// Append-only extension proof from `old_subtree_root` to `new_subtree_root`.
	/// `None` only when the two roots are equal (no new messages for this
	/// receiver, just other-destination churn).
	pub subtree_extension: Option<SubtreeExtension>,
}

/// A codec-able `mmr_lib` incremental (append-only) extension proof.
///
/// Verified via `MerkleProof::<Hash, SpecMerge>::new(new_mmr_size, proof)
/// .verify_incremental(new_subtree_root, old_subtree_root, incremental)`.
/// Generated by `mmr_lib::MMR::gen_proof` over the positions of the appended
/// leaves on the new MMR.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct SubtreeExtension {
	/// MMR size (node count) of the new, extended subtree.
	pub new_mmr_size: u64,
	/// `MerkleProof` items over the appended leaf positions.
	pub proof: Vec<Hash>,
	/// The appended leaf hashes (`hash_leaf` values), in append order.
	pub incremental: Vec<Hash>,
}

/// Per-source tracking for incoming speculative messages.
///
/// Only `last_processed` is required: subtree authentication flows through the
/// relay chain matching `batch.subtree_root` against the source's committed
/// `ProvidesRoots[source].get(receiver)` entry, and message authentication flows
/// through the per-batch MMR inclusion proof against `batch.subtree_root`.
/// `last_processed` is the only cross-block state required to enforce continuity
/// (`msg.position == last_processed + 1`) and prevent replay.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo, Default)]
pub struct SourceState {
	/// Last processed message leaf index in the source's per-destination subtree.
	pub last_processed: u64,
}
