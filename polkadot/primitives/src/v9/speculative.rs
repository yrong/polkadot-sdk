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

use alloc::vec::Vec;

use codec::{Decode, DecodeWithMemTracking, Encode};
use scale_info::TypeInfo;

use super::{BlockNumber, Hash, Id};

/// The API version at which speculative messaging support was introduced.
/// Collators and runtimes use this to gate speculative field population.
pub const SPECULATIVE_API_VERSION: u32 = 10;

/// Maximum number of source parachains a receiver can consume from in one block.
/// This bounds the size of `requires` in candidate commitments.
pub const MAX_REQUIRES_PER_BLOCK: usize = 32;

/// A commitment that a parachain provides a set of outbound speculative messages.
/// The root is the top-level Merkle root over all per-destination MMR roots.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(Hash))]
pub struct ProvidesCommitment {
	/// Top-level Merkle root over all per-destination MMR roots.
	pub root: Hash,
}

/// A commitment that a parachain requires messages from a source parachain.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(Hash))]
pub struct RequiresCommitment {
	/// The source parachain whose provides root we expect.
	pub source: Id,
	/// The provides root we built against (the source chain's top-level root at the
	/// block from which we received messages).
	pub expected_root: Hash,
}

/// Deterministic ingress payload carried in the parachain block body.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct SpeculativeIngress {
	/// Verified batches selected by the collator for this block.
	pub batches: Vec<MessageBatch>,
}

/// A message batch sent off-chain between collators.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct MessageBatch {
	/// Source parachain.
	pub source: Id,
	/// Source block hash that produced these messages.
	pub source_block: Hash,
	/// Relay-chain block number associated with the source batch.
	pub source_relay_parent_number: BlockNumber,
	/// The top-level provides root for this block.
	pub provides_root: Hash,
	/// The per-destination MMR root for the receiver.
	pub subtree_root: Hash,
	/// Merkle proof that subtree_root is in provides_root.
	/// Length: O(log D) where D = number of destinations.
	pub subtree_inclusion_proof: Vec<Hash>,
	/// Total number of destinations in the top-level Merkle tree
	/// (needed for `binary_merkle_tree::verify_proof`).
	pub number_of_destinations: u32,
	/// The index of this destination's leaf in the top-level Merkle tree
	/// (0-based, sorted by destination ParaId).
	pub leaf_index: u32,
	/// The messages with their positions in the sender's subtree MMR.
	pub messages: Vec<OutgoingMessage>,
}

/// An individual outbound message with its MMR position.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct OutgoingMessage {
	/// Zero-based position in the source's per-destination MMR.
	pub position: u64,
	/// Raw XCM message bytes.
	pub payload: Vec<u8>,
}

/// Included in the receiver candidate's PoV when the block was built against
/// an older source root than what's persisted in ProvidesRoots.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct LateBlockProof {
	/// The source parachain this proof covers.
	pub source: Id,

	/// Number of destinations in the OLD provides root's Merkle tree.
	/// Required to verify old_subtree_proof.
	pub old_number_of_destinations: u32,
	/// Leaf index of the receiver in the OLD provides root's Merkle tree.
	pub old_leaf_index: u32,

	/// Number of destinations in the NEW (current) provides root's Merkle tree.
	/// Required to verify new_subtree_proof.
	pub number_of_destinations: u32,
	/// Leaf index of the receiver in the NEW provides root's Merkle tree.
	pub leaf_index: u32,

	/// The provides root the receiver block was built against (the old root
	/// from the batch).
	pub old_provides_root: Hash,
	/// The subtree root the receiver built against (from the old source block).
	pub old_subtree_root: Hash,
	/// Merkle proof that old_subtree_root was in old_provides_root.
	pub old_subtree_proof: Vec<Hash>,

	/// The current provides root of the source (what's now in ProvidesRoots).
	pub new_provides_root: Hash,
	/// The subtree root under the new provides root.
	pub new_subtree_root: Hash,
	/// Merkle proof that new_subtree_root is in new_provides_root.
	pub new_subtree_proof: Vec<Hash>,

	/// If the source produced additional messages to this receiver since the
	/// block was built, this proof shows the old subtree is a valid prefix of
	/// the new subtree.
	pub subtree_extension: Option<MMRExtensionProof>,
}

/// Proves that an MMR root R_old is an ancestor of R_new, i.e. the MMR was
/// only appended to, not mutated.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub struct MMRExtensionProof {
	/// The peaks of the old MMR.
	pub old_peaks: Vec<Hash>,
	/// Number of leaves in the old MMR. Required to replay appends correctly.
	pub old_leaf_count: u64,
	/// The peaks of the new (larger) MMR.
	pub new_peaks: Vec<Hash>,
	/// Leaf hashes of messages appended between old and new state (in order).
	/// Replaying these appends starting from old_peaks must reproduce new_peaks.
	pub connecting_nodes: Vec<Hash>,
}
