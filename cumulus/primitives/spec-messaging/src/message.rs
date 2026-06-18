// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Parachain-side off-chain message types for speculative messaging.
//!
//! These are the wire/PoV types exchanged between collators and consumed by the
//! receiver runtime: [`MessageBatch`] / [`SpeculativeIngress`] (block body),
//! [`LateBlockProof`] / [`SubtreeExtension`] (PoV scaffolding), and the receiver's
//! per-source [`SourceState`]. The relay chain does not decode any of these — it
//! only matches the flat `(source, subtree_root)` commitments (which live in
//! `polkadot-primitives::v9` as `ProvidesCommitment`/`RequiresCommitment`).
//!
//! This module also fixes the protocol's concrete instantiation: [`SpecHasher`]
//! (the hash function) and [`MaxSpeculativeMessageLen`] (the payload bound), and
//! the concrete [`OutgoingMessage`] alias used in batches.

use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode};
use polkadot_core_primitives::{BlockNumber, Hash};
use polkadot_parachain_primitives::primitives::Id;
use scale_info::TypeInfo;
use sp_core::ConstU32;

use crate::outgoing_message::OutgoingMessage as GenericOutgoingMessage;

/// Maximum size in bytes of a single speculative message payload.
pub const MAX_SPECULATIVE_MESSAGE_LEN: u32 = 102_400;

/// Bound for a single speculative message payload.
pub type MaxSpeculativeMessageLen = ConstU32<MAX_SPECULATIVE_MESSAGE_LEN>;

/// The hash function used throughout speculative messaging (leaf hashing, MMR
/// merges, subtree roots). The crate primitives are generic over the hasher; this
/// alias is the single concrete choice for the protocol, so switching (e.g. to
/// Keccak256) is a one-line change here.
pub type SpecHasher = sp_runtime::traits::BlakeTwo256;

/// A single outbound speculative message with the protocol payload bound.
/// Identified by `(source, destination, position)` and hashed into an MMR leaf via
/// [`GenericOutgoingMessage::hash_leaf`].
pub type OutgoingMessage = GenericOutgoingMessage<MaxSpeculativeMessageLen>;

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
