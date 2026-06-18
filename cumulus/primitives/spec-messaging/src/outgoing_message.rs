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

use alloc::vec::Vec;
use codec::Encode;
use polkadot_core_primitives::Hash;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_core::Get;
use sp_runtime::{traits::Hash as HashT, BoundedVec};

use crate::{LEAF_TAG, LEAF_VERSION};

/// A single message sent from one parachain to another.
///
/// Each message is uniquely identified by `(source, destination, position)`.
/// `position` is a monotonically increasing sequence number scoped to the
/// `(source, destination)` pair; it lets the receiver detect gaps or replays
/// without inspecting the payload.
/// `MaxMsgLen` is a `Get<u32>` marker (e.g. `ConstU32<N>`) that does not itself
/// implement `Clone`/`PartialEq`/`Eq`/`TypeInfo`/etc. The standard `derive`s
/// would wrongly bound `MaxMsgLen` by those traits, so the SCALE derives use
/// explicit empty bounds + `skip_type_params`, and the `std`/`core` traits are
/// implemented by hand (bound-free) below. This lets `OutgoingMessage<ConstU32<N>>`
/// be embedded in `PartialEq`/`Eq`/`TypeInfo` containers like `MessageBatch`.
#[derive(codec::Encode, codec::MaxEncodedLen, codec::Decode, scale_info::TypeInfo)]
#[codec(encode_bound())]
#[codec(decode_bound())]
#[codec(mel_bound())]
#[scale_info(skip_type_params(MaxMsgLen))]
pub struct OutgoingMessage<MaxMsgLen: Get<u32>> {
	/// The parachain that sent this message.
	pub source: ParaId,
	/// The parachain this message is addressed to.
	pub destination: ParaId,
	/// Sequence number within the `(source, destination)` channel, starting at 0.
	pub position: u64,
	/// Opaque message body, bounded to `MaxMsgLen` bytes.
	pub payload: BoundedVec<u8, MaxMsgLen>,
}

// `DecodeWithMemTracking` is a marker trait; this type only ever holds plain
// `Copy`/`Vec` data, so the bound is trivially satisfied.
impl<MaxMsgLen: Get<u32>> codec::DecodeWithMemTracking for OutgoingMessage<MaxMsgLen> {}

impl<MaxMsgLen: Get<u32>> Clone for OutgoingMessage<MaxMsgLen> {
	fn clone(&self) -> Self {
		Self {
			source: self.source,
			destination: self.destination,
			position: self.position,
			payload: self.payload.clone(),
		}
	}
}

impl<MaxMsgLen: Get<u32>> PartialEq for OutgoingMessage<MaxMsgLen> {
	fn eq(&self, other: &Self) -> bool {
		self.source == other.source &&
			self.destination == other.destination &&
			self.position == other.position &&
			self.payload == other.payload
	}
}

impl<MaxMsgLen: Get<u32>> Eq for OutgoingMessage<MaxMsgLen> {}

impl<MaxMsgLen: Get<u32>> core::fmt::Debug for OutgoingMessage<MaxMsgLen> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		f.debug_struct("OutgoingMessage")
			.field("source", &self.source)
			.field("destination", &self.destination)
			.field("position", &self.position)
			.field("payload", &self.payload)
			.finish()
	}
}

impl<MaxMsgLen: Get<u32>> Default for OutgoingMessage<MaxMsgLen> {
	fn default() -> Self {
		Self {
			source: Default::default(),
			destination: Default::default(),
			position: 0,
			payload: Default::default(),
		}
	}
}

#[cfg(feature = "std")]
impl<MaxMsgLen: Get<u32>> core::hash::Hash for OutgoingMessage<MaxMsgLen> {
	fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
		self.source.hash(state);
		self.destination.hash(state);
		self.position.hash(state);
		self.payload.hash(state);
	}
}

impl<MaxMsgLen: Get<u32>> OutgoingMessage<MaxMsgLen> {
	/// Constructs a new [`OutgoingMessage`].
	pub fn new(
		source: ParaId,
		destination: ParaId,
		position: u64,
		payload: BoundedVec<u8, MaxMsgLen>,
	) -> Self {
		Self { source, destination, position, payload }
	}

	/// Hashes this message into an MMR leaf with hash function `H`.
	///
	/// The preimage is:
	/// ```text
	/// LEAF_TAG | LEAF_VERSION | source | destination | position (le64) | len (le32) | payload
	/// ```
	/// Domain tags and a version prefix prevent collisions with inner/peak nodes
	/// and allow the leaf format to evolve without breaking existing commitments.
	pub fn hash_leaf<H: HashT<Output = Hash>>(&self) -> Hash {
		let mut preimage = Vec::new();
		preimage.extend_from_slice(&LEAF_TAG.to_le_bytes());
		preimage.extend_from_slice(&LEAF_VERSION.to_le_bytes());
		preimage.extend_from_slice(&self.source.encode());
		preimage.extend_from_slice(&self.destination.encode());
		preimage.extend_from_slice(&self.position.to_le_bytes());
		preimage.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
		preimage.extend_from_slice(&self.payload);
		<H as HashT>::hash(&preimage)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::ConstU32;
	use sp_runtime::traits::BlakeTwo256;

	type MaxLen = ConstU32<32>;

	fn message(
		source: u32,
		destination: u32,
		position: u64,
		payload: Vec<u8>,
	) -> OutgoingMessage<MaxLen> {
		OutgoingMessage::new(
			ParaId::from(source),
			ParaId::from(destination),
			position,
			BoundedVec::try_from(payload).unwrap(),
		)
	}

	#[test]
	fn new_sets_fields() {
		let msg = message(1, 2, 3, vec![1, 2, 3]);

		assert_eq!(msg.source, ParaId::from(1));
		assert_eq!(msg.destination, ParaId::from(2));
		assert_eq!(msg.position, 3);
		assert_eq!(msg.payload.to_vec(), vec![1, 2, 3]);
	}

	#[test]
	fn hash_leaf_is_deterministic() {
		let msg = message(1, 2, 3, vec![1, 2, 3]);

		assert_eq!(msg.hash_leaf::<BlakeTwo256>(), msg.hash_leaf::<BlakeTwo256>());
	}

	#[test]
	fn hash_leaf_matches_expected_preimage() {
		let msg = message(1, 2, 3, vec![1, 2, 3]);

		let mut expected_preimage = Vec::new();
		expected_preimage.extend_from_slice(&LEAF_TAG.to_le_bytes());
		expected_preimage.extend_from_slice(&LEAF_VERSION.to_le_bytes());
		expected_preimage.extend_from_slice(&ParaId::from(1).encode());
		expected_preimage.extend_from_slice(&ParaId::from(2).encode());
		expected_preimage.extend_from_slice(&3u64.to_le_bytes());
		expected_preimage.extend_from_slice(&3u32.to_le_bytes());
		expected_preimage.extend_from_slice(&[1, 2, 3]);

		assert_eq!(msg.hash_leaf::<BlakeTwo256>(), BlakeTwo256::hash(&expected_preimage));
	}

	#[test]
	fn hash_leaf_differs_for_different_source() {
		let a = message(1, 2, 3, vec![1, 2, 3]);
		let b = message(9, 2, 3, vec![1, 2, 3]);

		assert_ne!(a.hash_leaf::<BlakeTwo256>(), b.hash_leaf::<BlakeTwo256>());
	}

	#[test]
	fn hash_leaf_differs_for_different_destination() {
		let a = message(1, 2, 3, vec![1, 2, 3]);
		let b = message(1, 9, 3, vec![1, 2, 3]);

		assert_ne!(a.hash_leaf::<BlakeTwo256>(), b.hash_leaf::<BlakeTwo256>());
	}

	#[test]
	fn hash_leaf_differs_for_different_position() {
		let a = message(1, 2, 3, vec![1, 2, 3]);
		let b = message(1, 2, 9, vec![1, 2, 3]);

		assert_ne!(a.hash_leaf::<BlakeTwo256>(), b.hash_leaf::<BlakeTwo256>());
	}

	#[test]
	fn hash_leaf_differs_for_different_payload() {
		let a = message(1, 2, 3, vec![1, 2, 3]);
		let b = message(1, 2, 3, vec![9, 9, 9]);

		assert_ne!(a.hash_leaf::<BlakeTwo256>(), b.hash_leaf::<BlakeTwo256>());
	}
}
