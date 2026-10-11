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

//! XCM over Speculative Messaging, on one channel per peer, `(domain 0, num 0)`, whose `Data`
//! payload is the SCALE-encoded `VersionedXcm`. Inbound, [`EnqueueToXcmQueue`] queues it under
//! `SpecMsg(source)`; outbound, [`SpecMsgRouter`] sends to a sibling with no HRMP channel.

use crate::{Config, Error, HrmpClosing, OnSpecMsgData, OutChannels, OutboundMessages, Pallet};
use alloc::vec::Vec;
use codec::{DecodeAll, DecodeLimit, Encode};
use core::marker::PhantomData;
use cumulus_primitives_core::{ChannelStatus, GetChannelInfo};
use cumulus_primitives_spec_messaging::{
	ChannelId, ChannelPhase, MessagePosition, SpecMsgKind, StreamId,
};
use frame_support::{
	traits::{EnqueueMessage, Get},
	BoundedSlice,
};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use polkadot_runtime_common::xcm_sender::PriceForMessageDelivery;
use xcm::{latest::prelude::*, VersionedLocation, VersionedXcm, WrapVersion, MAX_XCM_DECODE_DEPTH};
use xcm_builder::InspectMessageQueues;

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

/// [`OnSpecMsgData`] that forwards the XCM channel's payloads, in order, to the message queue:
///
/// ```ignore
/// type DataHandler = EnqueueToXcmQueue<
/// 	TransformOrigin<MessageQueue, AggregateMessageOrigin, ParaId, ParaIdToSpecMsg>,
/// >;
/// ```
///
/// Other channels are never executed as XCM, since XCM runs with the sibling origin; they are
/// dropped. The pallet's `integrity_test` keeps `MaxMsgLen` within the queue's `MaxMessageLen`.
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

	fn max_len() -> u32 {
		Queue::MaxMessageLen::get()
	}
}

/// XCM sender to sibling parachains over Speculative Messaging. Place it **before** `XcmpQueue`,
/// which accepts any sibling.
///
/// - HRMP wins while a channel exists (`Full` included), unless the sibling is [`HrmpClosing`].
/// - Otherwise the XCM goes over the spec-msg XCM channel if it is `Open`, else falls through.
/// - An `Open` channel without capacity is a hard [`SendError::Transport`]: falling through would
///   lose the XCM in `XcmpQueue`.
///
/// `ChannelInfo` is usually `ParachainSystem`, `VersionWrapper` usually `PolkadotXcm` (same
/// destination `Location` as HRMP, so negotiated versions carry over).
pub struct SpecMsgRouter<T, ChannelInfo, VersionWrapper, Price>(
	PhantomData<(T, ChannelInfo, VersionWrapper, Price)>,
);

impl<T, ChannelInfo, VersionWrapper, Price> SendXcm
	for SpecMsgRouter<T, ChannelInfo, VersionWrapper, Price>
where
	T: Config,
	ChannelInfo: GetChannelInfo,
	VersionWrapper: WrapVersion,
	Price: PriceForMessageDelivery<Id = ParaId>,
{
	type Ticket = (ChannelId, Vec<u8>);

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		let d = dest.take().ok_or(SendError::MissingArgument)?;
		let id = match d.unpack() {
			(1, [Parachain(id)]) => ParaId::from(*id),
			_ => {
				*dest = Some(d);
				return Err(SendError::NotApplicable);
			},
		};
		let channel = xcm_channel(id);
		let hrmp_open = !HrmpClosing::<T>::contains_key(id) &&
			!matches!(ChannelInfo::get_channel_status(id), ChannelStatus::Closed);
		let spec_msg_open =
			OutChannels::<T>::get(channel).is_some_and(|state| state.phase() == ChannelPhase::Open);
		if hrmp_open || !spec_msg_open {
			*dest = Some(d);
			return Err(SendError::NotApplicable);
		}

		let xcm = msg.take().ok_or(SendError::MissingArgument)?;
		let price = Price::price_for_delivery(id, &xcm);
		let versioned = VersionWrapper::wrap_version(&d, xcm)
			.map_err(|()| SendError::DestinationUnsupported)?;
		versioned.check_is_decodable().map_err(|()| SendError::ExceedsMaxMessageSize)?;
		let encoded = versioned.encode();
		Pallet::<T>::can_send(&channel, encoded.len()).map_err(|error| {
			if error == Error::<T>::MessageTooBig {
				SendError::ExceedsMaxMessageSize
			} else {
				SendError::Transport("spec-msg XCM channel has no capacity")
			}
		})?;
		Ok(((channel, encoded), price))
	}

	fn deliver((channel, encoded): Self::Ticket) -> Result<XcmHash, SendError> {
		let hash = sp_io::hashing::blake2_256(&encoded);
		Pallet::<T>::send(channel, encoded)
			.map_err(|_| SendError::Transport("spec-msg XCM send failed"))?;
		Ok(hash)
	}
}

/// The dry-run APIs' view of this block's XCM sends: the XCM channels' `Data` leaves, decoded back
/// to the `VersionedXcm`s the router sent, keyed by destination. Mirrors `XcmpQueue`.
impl<T: Config, ChannelInfo, VersionWrapper, Price> InspectMessageQueues
	for SpecMsgRouter<T, ChannelInfo, VersionWrapper, Price>
{
	fn clear_messages() {
		// `OutboundMessages` holds only this block's sends, so this isolates a dry run's output.
		let _ = OutboundMessages::<T>::clear(u32::MAX, None);
	}

	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		OutboundMessages::<T>::iter()
			.filter(|(stream, _)| is_xcm_channel(stream))
			.filter_map(|(stream, leaves)| {
				let StreamId::Channel { recipient, .. } = stream else { return None };
				let messages: Vec<_> = leaves
					.iter()
					.filter_map(|leaf| match SpecMsgKind::decode_all(&mut &leaf[..]).ok()? {
						SpecMsgKind::Data(data) => VersionedXcm::<()>::decode_all_with_depth_limit(
							MAX_XCM_DECODE_DEPTH,
							&mut &data[..],
						)
						.ok(),
						SpecMsgKind::Signal(_) => None,
					})
					.collect();
				(!messages.is_empty()).then(|| {
					(
						VersionedLocation::from(Location::new(1, Parachain(recipient.into()))),
						messages,
					)
				})
			})
			.collect()
	}
}
