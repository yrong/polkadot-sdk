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

//! Primitives for the Speculative Messaging protocol.
//!
//! Speculative Messaging lets parachains exchange messages without waiting for
//! full relay-chain confirmation. Each sender parachain accumulates its outgoing
//! messages per destination into a Merkle Mountain Range (MMR) and commits the
//! per-destination roots into a commitment set (`polkadot_primitives::v9::
//! CommitmentSet` — relay-visible, so it lives in `polkadot-primitives`). The relay
//! chain then matches sender commitments against receiver expectations, allowing
//! both sides to process messages speculatively and confirm them after the fact.
//!
//! This crate holds the **parachain-side** primitives that build those
//! commitments and the off-chain wire types.
//!
//! # Key types
//!
//! - [`outgoing_message::OutgoingMessage`] — a single outgoing message; call
//!   [`outgoing_message::OutgoingMessage::hash_leaf`] to obtain the MMR leaf hash.
//! - [`mmr::SpecMerge`] — the domain-tagged `mmr_lib::Merge` that backs the per-destination MMR;
//!   [`mmr::root_from_peaks`] derives a subtree root from peaks-only state. Inclusion and ancestry
//!   proofs come from `mmr_lib` itself.
//! - [`message`] — the off-chain wire types (`MessageBatch`, `LateBlockProof`, …).

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod message;
pub mod mmr;
pub mod outgoing_message;

pub use message::{
	apply_late_block_proofs, LateBlockProof, MaxSpeculativeMessageLen, MessageBatch,
	OutgoingMessage, SourceState, SpecHasher, SpeculativeIngress, SubtreeExtension,
	MAX_SPECULATIVE_MESSAGE_LEN,
};

// Domain Tags to ensure that the same message structure used in different
// contexts (e.g. leaf vs inner node) do not collide on the same hash. Tag values
// are part of the hash preimages, so they are kept stable (the now-removed
// `EMPTY_TAG = 0x1` slot is intentionally left unused rather than renumbering).

/// Tag for a leaf node.
pub const LEAF_TAG: u8 = 0x2;

/// Tag for an inner node.
pub const INNER_TAG: u8 = 0x3;

/// Tag for a peak.
pub const PEAK_TAG: u8 = 0x4;

// Leaf versioning to allow for future changes to the leaf structure without
// breaking compatibility with old messages.

/// Leaf Version.
pub const LEAF_VERSION: u8 = 0x0;
