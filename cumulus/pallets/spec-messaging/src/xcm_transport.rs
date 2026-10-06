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

//! XCM over Speculative Messaging.
//!
//! XCM uses one well-known channel per peer, `(domain 0, num 0)`. On it, a `Data` payload is
//! exactly the SCALE-encoded `VersionedXcm`, with no extra framing. Other channels are free for
//! non-XCM protocols; demultiplexing is by channel, never in-band.
//!
//! Inbound, [`EnqueueToXcmQueue`] hands the XCM channel's payloads to the message queue for
//! execution under `AggregateMessageOrigin::SpecMsg(source)`, which converts to the same
//! `Location` as HRMP's `Sibling(source)`.

use crate::OnSpecMsgData;
use alloc::vec::Vec;
use cumulus_primitives_spec_messaging::{ChannelId, MessagePosition, StreamId};
use frame_support::{traits::EnqueueMessage, BoundedSlice};
use polkadot_parachain_primitives::primitives::Id as ParaId;

/// The channel `domain` XCM uses.
pub const XCM_CHANNEL_DOMAIN: u8 = 0;

/// The channel `num` XCM uses.
pub const XCM_CHANNEL_NUM: u16 = 0;

/// The XCM channel with `peer`.
pub fn xcm_channel(peer: ParaId) -> ChannelId {
	ChannelId { peer, domain: XCM_CHANNEL_DOMAIN, num: XCM_CHANNEL_NUM }
}

/// Whether `stream` is an XCM channel's data stream.
fn is_xcm_channel(stream: &StreamId) -> bool {
	matches!(stream, StreamId::Channel { domain: XCM_CHANNEL_DOMAIN, num: XCM_CHANNEL_NUM, .. })
}

/// [`OnSpecMsgData`] that forwards the XCM channel's consumed payloads, verbatim and in order, to
/// the runtime's message queue, mirroring the XCMP enqueue path. Wire `Queue` so the source maps
/// to its spec-msg queue book:
///
/// ```ignore
/// type DataHandler = EnqueueToXcmQueue<
/// 	TransformOrigin<MessageQueue, AggregateMessageOrigin, ParaId, ParaIdToSpecMsg>,
/// >;
/// ```
///
/// Only the XCM channel is forwarded. Inbound XCM executes with the source chain's sibling
/// origin, so bytes from any other channel, which a chain may hand to an application, must never
/// be executed as XCM. They are consumed and dropped until such a protocol brings its own handler.
///
/// The queue's `MaxMessageLen` must be at least the pallet's `MaxMsgLen`, so every consumed
/// payload fits. A payload that does not fit is dropped defensively.
pub struct EnqueueToXcmQueue<Queue>(core::marker::PhantomData<Queue>);

impl<Queue: EnqueueMessage<ParaId>> OnSpecMsgData for EnqueueToXcmQueue<Queue> {
	fn on_data(source: ParaId, stream: StreamId, _position: MessagePosition, data: Vec<u8>) {
		if !is_xcm_channel(&stream) {
			return;
		}
		let Ok(message) = BoundedSlice::try_from(&data[..]) else {
			frame_support::defensive!(
				"spec-msg XCM payload exceeds the queue's `MaxMessageLen`; dropped \
				 (`MaxMessageLen` must be at least `MaxMsgLen`)"
			);
			return;
		};
		Queue::enqueue_message(message, source);
	}
}
