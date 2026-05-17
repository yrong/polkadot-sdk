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

//! `V10` Primitives.
//!
//! Extends v9 with speculative messaging types (Phase 1 — inclusion-based messaging).
//! Unchanged types are re-exported from v9.

use alloc::vec::Vec;

use codec::{Decode, DecodeWithMemTracking, Encode};
use scale_info::TypeInfo;

// ── Re-export unchanged types from v9 ──

pub use crate::v9::{
	AccountId, AccountIndex, AccountPublic, Balance, BlakeTwo256, Block, BlockId, BlockNumber,
	CandidateHash, ChainId, CollatorId, CollatorSignature, CoreIndex, DownwardMessage, Hash, HashT,
	HeadData, Header, HorizontalMessages, HrmpChannelId, Id, Id as ParaId, InboundDownwardMessage,
	InboundHrmpMessage, Moment, Nonce, OutboundHrmpMessage, ProvidesCommitment, Remark,
	RequiresCommitment, SessionIndex, Signature, Slot, UncheckedExtrinsic, UpwardMessage,
	UpwardMessages, ValidationCode, ValidationCodeHash, ValidatorId, ValidatorSignature,
	LOWEST_PUBLIC_ID,
};

// ── Speculative Messaging Types (v10 additions) ──

/// The API version at which speculative messaging support was introduced.
/// Collators and runtimes use this to gate speculative field population.
pub const SPECULATIVE_API_VERSION: u32 = 10;

/// Maximum number of source parachains a receiver can consume from in one block.
/// This bounds the size of `requires` in candidate commitments.
pub const MAX_REQUIRES_PER_BLOCK: usize = 32;

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
	pub source: ParaId,
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
	pub source: ParaId,

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

/// Commitments made in a v10 candidate receipt.
/// Re-uses v9 `CandidateCommitments` directly since v9 now has the speculative fields.
pub use crate::v9::CandidateCommitments;
