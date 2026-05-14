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
	InboundHrmpMessage, Moment, Nonce, OutboundHrmpMessage, Remark,
	SessionIndex, Signature, Slot, UncheckedExtrinsic, UpwardMessage, UpwardMessages,
	ValidationCode, ValidationCodeHash, ValidatorId, ValidatorSignature, LOWEST_PUBLIC_ID,
};

// ── Speculative Messaging Types (v10 additions) ──

/// The API version at which speculative messaging support was introduced.
/// Collators and runtimes use this to gate speculative field population.
pub const SPECULATIVE_API_VERSION: u32 = 10;

/// Maximum number of source parachains a receiver can consume from in one block.
/// This bounds the size of `requires` in candidate commitments.
pub const MAX_REQUIRES_PER_BLOCK: usize = 32;

/// A commitment that a parachain provides a set of outbound messages.
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
	pub source: ParaId,
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
#[derive(Clone, Encode, Decode, Debug)]
pub struct LateBlockProof {
	/// The source parachain this proof covers.
	pub source: ParaId,

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
#[derive(Clone, Encode, Decode, Debug)]
pub struct MMRExtensionProof {
	/// The peaks of the old MMR.
	pub old_peaks: Vec<Hash>,
	/// The peaks of the new (larger) MMR.
	pub new_peaks: Vec<Hash>,
	/// Nodes connecting old peaks to new peaks to prove prefix relationship.
	pub connecting_nodes: Vec<Hash>,
}

// ── Extended Candidate Types for v10 ──

/// Commitments made in a v10 candidate receipt.
/// Extends v9 commitments with speculative messaging fields.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(Default, Hash))]
pub struct CandidateCommitments<N = BlockNumber> {
	/// Messages destined to be interpreted by the Relay chain itself.
	pub upward_messages: UpwardMessages,
	/// Horizontal messages sent by the parachain (HRMP, legacy).
	pub horizontal_messages: HorizontalMessages,
	/// New validation code.
	pub new_validation_code: Option<ValidationCode>,
	/// The head-data produced as a result of execution.
	pub head_data: HeadData,
	/// The number of messages processed from the DMQ.
	pub processed_downward_messages: u32,
	/// The mark which specifies the block number up to which all inbound HRMP messages are
	/// processed.
	pub hrmp_watermark: N,

	// ── Speculative messaging fields ──
	/// The provides commitment (None if no outbox state exists yet).
	pub provides: Option<ProvidesCommitment>,
	/// The requires commitments, sorted by source: ParaId, at most one per source.
	pub requires: Vec<RequiresCommitment>,
}

impl CandidateCommitments {
	/// Compute the blake2-256 hash of the commitments.
	pub fn hash(&self) -> Hash {
		BlakeTwo256::hash_of(self)
	}
}

/// The candidate descriptor version for speculative messaging.
#[derive(PartialEq, Eq, Clone, TypeInfo, Debug)]
pub enum CandidateDescriptorVersion {
	/// Legacy v1 descriptor.
	V1,
	/// Legacy v2 descriptor.
	V2,
	/// Legacy v3 descriptor (reserved).
	V3,
	/// Speculative-messaging-capable v4 descriptor.
	V4,
	/// An unknown version.
	Unknown,
}

/// A unique descriptor of a v10 candidate receipt (V4).
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub struct CandidateDescriptorV4<H = Hash> {
	/// The ID of the para this is a candidate for.
	pub para_id: ParaId,
	/// The hash of the relay-chain block this is executed in the context of.
	pub relay_parent: H,
	/// Phase 1 speculative messaging does not require LLv2 fields.
	/// Included as optional for future shared descriptor upgrade path.
	pub scheduling_parent: Option<H>,
	/// Scheduling session index (LLv2 future field).
	pub scheduling_session_index: Option<SessionIndex>,
	/// The collator's public key.
	pub collator: CollatorId,
	/// The blake2-256 hash of the persisted validation data.
	pub persisted_validation_data_hash: Hash,
	/// The blake2-256 hash of the PoV.
	pub pov_hash: Hash,
	/// The root of a block's erasure encoding Merkle tree.
	pub erasure_root: Hash,
	/// Hash of the para header that is being generated by this candidate.
	pub para_head: Hash,
	/// The blake2-256 hash of the validation code bytes.
	pub validation_code_hash: ValidationCodeHash,
	/// The collator's signature.
	pub signature: CollatorSignature,
	/// The core index where the candidate is backed.
	pub core_index: CoreIndex,
	/// The session index of the candidate relay parent.
	pub session_index: SessionIndex,
}

impl<H: Copy> CandidateDescriptorV4<H> {
	/// Returns the descriptor version.
	pub fn version(&self) -> CandidateDescriptorVersion {
		CandidateDescriptorVersion::V4
	}

	/// The relay parent.
	pub fn relay_parent(&self) -> H {
		self.relay_parent
	}

	/// The para id.
	pub fn para_id(&self) -> ParaId {
		self.para_id
	}

	/// The persisted validation data hash.
	pub fn persisted_validation_data_hash(&self) -> Hash {
		self.persisted_validation_data_hash
	}

	/// The PoV hash.
	pub fn pov_hash(&self) -> Hash {
		self.pov_hash
	}

	/// The erasure root.
	pub fn erasure_root(&self) -> Hash {
		self.erasure_root
	}

	/// The para head hash.
	pub fn para_head(&self) -> Hash {
		self.para_head
	}

	/// The validation code hash.
	pub fn validation_code_hash(&self) -> ValidationCodeHash {
		self.validation_code_hash
	}
}

/// A candidate receipt at version 4 (v10).
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub struct CandidateReceiptV4<H = Hash> {
	/// The descriptor of the candidate.
	pub descriptor: CandidateDescriptorV4<H>,
	/// The hash of the encoded commitments made as a result of candidate execution.
	pub commitments_hash: Hash,
}

/// A v10 candidate receipt with commitments directly included.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub struct CommittedCandidateReceiptV4<H = Hash> {
	/// The descriptor of the candidate.
	pub descriptor: CandidateDescriptorV4<H>,
	/// The commitments of the candidate receipt.
	pub commitments: CandidateCommitments,
}

// Note: `ParachainBlockDataV4` is defined in the cumulus primitives layer
// (cumulus/primitives/core/) since it depends on `ParachainBlockData` which
// is a cumulus type. See step 6 of the implementation plan.
