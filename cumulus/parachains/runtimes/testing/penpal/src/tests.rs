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

//! The Speculative Messaging wiring of penpal: the router's place in `XcmRouter` (HRMP wins while
//! it exists, spec-msg only over an open channel), the `Provides` signal it causes, and the message
//! queue's room for consumed payloads.

use crate::{
	xcm_config::XcmRouter, PolkadotXcm, Runtime, RuntimeOrigin, SpecMessaging, SpecMsgMaxMsgLen,
	SpecMsgWindowGrant,
};
use codec::DecodeAll;
use cumulus_pallet_parachain_system::{
	relay_state_snapshot::{MessagingStateSnapshot, RelayDispatchQueueRemainingCapacity},
	RelevantMessagingState,
};
use cumulus_pallet_spec_messaging::{xcm_channel, OutChannels, OutboundMessages, PROTOCOL_VERSION};
use cumulus_primitives_core::AbridgedHrmpChannel;
use cumulus_primitives_spec_messaging::{
	MessagePosition, OutChannelState, ProvideUmpSignals, Register, SpecMsgKind, SPMS_ENGINE_ID,
};
use frame_support::{assert_ok, traits::Get};
use sp_runtime::{generic::DigestItem, BuildStorage};
use xcm::{latest::prelude::*, VersionedXcm};

const SIBLING: u32 = 2001;

fn new_test_ext() -> sp_io::TestExternalities {
	let mut ext: sp_io::TestExternalities = frame_system::GenesisConfig::<Runtime>::default()
		.build_storage()
		.expect("system genesis builds; qed")
		.into();
	ext.execute_with(|| {
		// Both transports wrap the XCM for the destination, which needs a default version.
		assert_ok!(PolkadotXcm::force_default_xcm_version(
			RuntimeOrigin::root(),
			Some(XCM_VERSION)
		));
	});
	ext
}

fn sibling() -> Location {
	Location::new(1, [Parachain(SIBLING)])
}

fn xcm() -> Xcm<()> {
	Xcm(vec![ClearOrigin])
}

/// The outbound spec-msg XCM channel to [`SIBLING`] as an open/accept/register round trip leaves
/// it: `Open`, under the runtime's grant.
fn open_spec_msg_channel() {
	OutChannels::<Runtime>::insert(
		xcm_channel(SIBLING.into()),
		OutChannelState {
			closed_by_us: false,
			announced_version: PROTOCOL_VERSION,
			register: Some(Register {
				version: PROTOCOL_VERSION,
				up_to: MessagePosition(0),
				grant: SpecMsgWindowGrant::get(),
				closed: false,
			}),
		},
	);
}

/// An HRMP egress channel to [`SIBLING`] with room, as `ParachainSystem` stores it from relay
/// state.
fn open_hrmp_channel() {
	RelevantMessagingState::<Runtime>::put(MessagingStateSnapshot {
		dmq_mqc_head: Default::default(),
		relay_dispatch_queue_remaining_capacity: RelayDispatchQueueRemainingCapacity {
			remaining_count: u32::MAX,
			remaining_size: u32::MAX,
		},
		ingress_channels: Vec::new(),
		egress_channels: vec![(
			SIBLING.into(),
			AbridgedHrmpChannel {
				max_capacity: 8,
				max_total_size: 102400,
				max_message_size: 102400,
				msg_count: 0,
				total_size: 0,
				mqc_head: None,
			},
		)],
	});
}

#[test]
fn hrmp_wins_while_a_channel_exists() {
	new_test_ext().execute_with(|| {
		// Even with an open spec-msg channel, sibling XCM keeps going through `XcmpQueue`.
		open_hrmp_channel();
		open_spec_msg_channel();
		assert_ok!(send_xcm::<XcmRouter>(sibling(), xcm()));
		assert!(OutboundMessages::<Runtime>::iter().next().is_none());
	});
}

#[test]
fn without_hrmp_xcm_goes_over_spec_msg_only_on_an_open_channel() {
	new_test_ext().execute_with(|| {
		// No HRMP and no spec-msg channel: the router falls through, and `XcmpQueue` fails the
		// send rather than spec-msg taking it.
		assert!(send_xcm::<XcmRouter>(sibling(), xcm()).is_err());
		assert!(OutboundMessages::<Runtime>::iter().next().is_none());

		// With the channel open, the XCM lands on it as a `Data` leaf holding the
		// `VersionedXcm`. (`WithUniqueTopic` appends a topic, so decode rather than compare.)
		open_spec_msg_channel();
		assert_ok!(send_xcm::<XcmRouter>(sibling(), xcm()));
		let sent = OutboundMessages::<Runtime>::get(SpecMessaging::outbound_stream(&xcm_channel(
			SIBLING.into(),
		)));
		assert_eq!(sent.len(), 1);
		let Ok(SpecMsgKind::Data(data)) = SpecMsgKind::decode_all(&mut &sent[0][..]) else {
			panic!("XCM travels as a `Data` leaf");
		};
		assert!(VersionedXcm::<()>::decode_all(&mut &data[..]).is_ok());

		// The block commits the stream: the `Provides` signal and the `SPMS` digest.
		let root = SpecMessaging::commit_streams_root().expect("a stream was touched; qed");
		assert_eq!(<SpecMessaging as ProvideUmpSignals>::provides_root(), Some(root));
		assert!(frame_system::Pallet::<Runtime>::digest()
			.logs
			.iter()
			.any(|log| matches!(log, DigestItem::Consensus(id, _) if *id == SPMS_ENGINE_ID)));
	});
}

#[test]
fn consumed_payloads_fit_the_message_queue() {
	// `EnqueueToXcmQueue` needs room for every payload the pallet can consume.
	assert!(
		pallet_message_queue::MaxMessageLenOf::<Runtime>::get() >= SpecMsgMaxMsgLen::get(),
		"the message queue's `HeapSize` is too small for `SpecMsgMaxMsgLen`",
	);
}
