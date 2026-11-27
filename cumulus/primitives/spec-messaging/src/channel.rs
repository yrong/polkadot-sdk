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

//! The channel layer: the formats both ends exchange, and each side's local channel state. The
//! pallet owns the lifecycle and flow-control logic.
//!
//! # Formats (consensus surface)
//!
//! - [`ChannelMessage`] is the SCALE payload of every channel-stream MMR leaf, inside
//! [`crate::mmr::leaf_hash`]'s framing.
//! - [`Register`] is the SCALE payload of every ack-stream leaf: the receiver's complete voice in
//! the channel, lossy and latest-wins, read out-of-band by the sender.
//!
//! The domain identifies the channel protocol. A closed lane number cannot be reused.
//!
//! # Local state
//!
//! Distinct ID types per direction and kind ([`OutChannelId`], [`InChannelId`], [`BroadcastId`])
//! prevent an outbound channel ID from indexing inbound state, and let the runtime API and
//! inherent maps fix each entry's discipline by type.
//!
//! Helper convention: a bare [`StreamId`] is one of this chain's own streams (a key in its own
//! commitment tree and sender-side storage). A foreign stream is returned with its owner,
//! `(ParaId, StreamId)`.

use alloc::vec::Vec;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use scale_info::TypeInfo;

use crate::{
	mmr::{MessagePosition, MmrFrontier},
	stream::StreamId,
};

/// The payload of every channel-stream MMR leaf: userspace data or the terminal close marker.
#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, Debug, TypeInfo)]
pub enum ChannelMessage {
	/// Size limits apply to the whole encoded leaf payload, including the channel envelope.
	#[codec(index = 0)]
	Data(Vec<u8>),
	#[codec(index = 1)]
	Close,
}

/// The receiver's complete voice in a channel, published as a leaf on its `Ack` stream.
/// Latest-wins: every register is self-contained.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
)]
pub struct Register {
	/// Cumulative watermark: all positions `< up_to` of the channel's data stream (data and
	/// signals alike) have been consumed.
	pub up_to: MessagePosition,
	/// Current lane status.
	pub status: RegisterStatus,
}

/// Receiver-side lane status carried in a [`Register`].
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
)]
pub enum RegisterStatus {
	/// The receiver accepts data up to the advertised size limit.
	#[codec(index = 0)]
	Open { max_message_size: u32 },
	/// The receiver is draining the lane and requests a [`ChannelMessage::Close`] marker.
	#[codec(index = 1)]
	Closing,
	/// The receiver has stopped consuming this lane.
	#[codec(index = 2)]
	Closed,
}

/// A channel this chain sends on, named by its recipient.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Debug,
	TypeInfo,
)]
pub struct OutChannelId {
	pub recipient: ParaId,
	pub domain: u8,
	pub num: u16,
}

impl OutChannelId {
	/// Our data stream: `Channel { recipient, domain, num }`.
	pub fn data_stream(&self) -> StreamId {
		StreamId::Channel { recipient: self.recipient, domain: self.domain, num: self.num }
	}

	/// The recipient's `Ack { us, domain, num }`, read for its register.
	pub fn peer_ack_stream(&self, us: ParaId) -> (ParaId, StreamId) {
		(self.recipient, StreamId::Ack { recipient: us, domain: self.domain, num: self.num })
	}
}

/// A channel this chain receives on, named by its sender.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Debug,
	TypeInfo,
)]
pub struct InChannelId {
	pub sender: ParaId,
	pub domain: u8,
	pub num: u16,
}

impl InChannelId {
	/// Our `Ack { sender, domain, num }`, where we publish the register.
	pub fn ack_stream(&self) -> StreamId {
		StreamId::Ack { recipient: self.sender, domain: self.domain, num: self.num }
	}

	/// The sender's `Channel { us, domain, num }`, consumed in order.
	pub fn sender_data_stream(&self, us: ParaId) -> (ParaId, StreamId) {
		(self.sender, StreamId::Channel { recipient: us, domain: self.domain, num: self.num })
	}
}

/// A broadcast stream: its sender plus the discriminator. Broadcast has no recipient, so this is
/// the stream's global identity.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Debug,
	TypeInfo,
)]
pub struct BroadcastId {
	pub sender: ParaId,
	pub domain: u16,
	pub subdomain: u8,
	pub num: u32,
}

impl BroadcastId {
	/// The sender's `Broadcast { domain, subdomain, num }`, with its owner.
	pub fn stream(&self) -> (ParaId, StreamId) {
		(
			self.sender,
			StreamId::Broadcast { domain: self.domain, subdomain: self.subdomain, num: self.num },
		)
	}
}

/// A register applied by the sender, with its position on the recipient's `Ack` stream (the ack
/// highwater).
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
)]
pub struct AppliedRegister {
	pub position: MessagePosition,
	pub register: Register,
}

/// Sender side, per outbound channel.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	Debug,
	Default,
	TypeInfo,
)]
pub struct OutChannelState {
	pub want_open: bool,
	pub register: Option<AppliedRegister>,
}

/// Local close state distinguishes an orderly drain from governance abandoning the tail.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
)]
pub enum InboundStatus {
	Open,
	Closing,
	Closed,
	ForceClosed,
}

/// Receiver side, per inbound channel.
#[derive(
	Clone, Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, PartialEq, Eq, Debug, TypeInfo,
)]
pub struct InChannelState {
	/// Our copy of the sender's channel stream: incoming messages are appended to it; its leaf
	/// count is the position of the next message to consume.
	pub frontier: MmrFrontier,
	pub status: InboundStatus,
	/// The advertised limit remains in force while closing.
	pub max_message_size: u32,
	pub published: Option<Register>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn typed_ids_map_to_own_and_foreign_streams() {
		let us = ParaId::from(1000);
		let peer = ParaId::from(2000);

		let out_id = OutChannelId { recipient: peer, domain: 1, num: 2 };
		assert_eq!(out_id.data_stream(), StreamId::Channel { recipient: peer, domain: 1, num: 2 });
		assert_eq!(
			out_id.peer_ack_stream(us),
			(peer, StreamId::Ack { recipient: us, domain: 1, num: 2 }),
		);

		let in_id = InChannelId { sender: peer, domain: 1, num: 2 };
		assert_eq!(in_id.ack_stream(), StreamId::Ack { recipient: peer, domain: 1, num: 2 });
		assert_eq!(
			in_id.sender_data_stream(us),
			(peer, StreamId::Channel { recipient: us, domain: 1, num: 2 }),
		);

		let bcast_id = BroadcastId { sender: peer, domain: 3, subdomain: 4, num: 5 };
		assert_eq!(
			bcast_id.stream(),
			(peer, StreamId::Broadcast { domain: 3, subdomain: 4, num: 5 }),
		);
	}

	#[test]
	fn id_ordering_is_by_peer_then_discriminator() {
		let a = OutChannelId { recipient: ParaId::from(1), domain: 1, num: 9 };
		let b = OutChannelId { recipient: ParaId::from(2), domain: 0, num: 0 };
		let c = OutChannelId { recipient: ParaId::from(2), domain: 0, num: 1 };
		assert!(a < b && b < c);
	}

	#[test]
	fn channel_message_encoding() {
		assert_eq!(ChannelMessage::Close.encode(), vec![1]);
		assert_eq!(ChannelMessage::Data(vec![0xaa]).encode(), vec![0, 4, 0xaa]);
	}

	#[test]
	fn data_at_the_protocol_bound_takes_five_bytes_of_framing() {
		// The bound is on the encoded leaf. At this size the compact length takes 4 bytes, so 5
		// bytes less data fill it exactly and one more overflows it.
		let max = crate::fetch::MAX_SPECULATIVE_MESSAGE_LEN as usize;
		assert_eq!(ChannelMessage::Data(vec![0; max - 5]).encoded_size(), max);
		assert_eq!(ChannelMessage::Data(vec![0; max - 4]).encoded_size(), max + 1);
	}

	#[test]
	fn register_encoding() {
		for (status, tail) in [
			(RegisterStatus::Open { max_message_size: 1024 }, vec![0, 0, 4, 0, 0]),
			(RegisterStatus::Closing, vec![1]),
			(RegisterStatus::Closed, vec![2]),
		] {
			let register = Register { up_to: MessagePosition(5), status };
			let mut expected = 5u64.to_le_bytes().to_vec();
			expected.extend(tail);
			assert_eq!(register.encode(), expected);
			assert_eq!(Register::decode(&mut &expected[..]).unwrap(), register);
		}
	}
}
