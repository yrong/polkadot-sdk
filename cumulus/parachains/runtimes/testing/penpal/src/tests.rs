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

//! Penpal runtime tests for speculative messaging.

use crate::{xcm_config::XcmRouter, PolkadotXcm, Runtime, RuntimeOrigin, SpecMessaging};
use codec::DecodeAll;
use cumulus_pallet_parachain_system::{
	relay_state_snapshot::{MessagingStateSnapshot, RelayDispatchQueueRemainingCapacity},
	RelevantMessagingState,
};
use cumulus_pallet_spec_messaging::{xcm_channel, OutChannels, OutboundMessages};
use cumulus_primitives_core::{AbridgedHrmpChannel, AggregateMessageOrigin, InChannelId, ParaId};
use cumulus_primitives_spec_messaging::{
	AppliedRegister, ChannelMessage, MessagePosition, OpenSession, OutChannelState, Register,
	RegisterStatus, SessionParams, SPMS_ENGINE_ID,
};
use frame_support::{
	assert_ok,
	traits::{EnqueueMessage, Get, TransformOrigin},
	BoundedSlice,
};
use parachains_common::message_queue::ChannelToSpecMsg;
use sp_runtime::{generic::DigestItem, BuildStorage};
use xcm::{latest::prelude::*, VersionedXcm};

const SIBLING: u32 = 2001;

fn new_test_ext() -> sp_io::TestExternalities {
	let mut ext: sp_io::TestExternalities = frame_system::GenesisConfig::<Runtime>::default()
		.build_storage()
		.expect("system genesis builds; qed")
		.into();
	ext.execute_with(|| {
		frame_system::Pallet::<Runtime>::initialize(&1, &Default::default(), &Default::default());
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

/// Seed the state after a completed opening handshake.
fn open_spec_msg_channel() {
	let max_message_size = 102400;
	OutChannels::<Runtime>::insert(
		xcm_channel(SIBLING.into()),
		OutChannelState {
			want_open: true,
			session: 1,
			register: Some(AppliedRegister {
				position: MessagePosition(0),
				register: Register { up_to: MessagePosition(0), status: RegisterStatus::Opened },
			}),
			open: Some(OpenSession {
				session: 1,
				params: SessionParams { version: 1, max_message_size },
				opened: true,
			}),
			closed_session: None,
		},
	);
}

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
		open_hrmp_channel();
		open_spec_msg_channel();
		assert_ok!(send_xcm::<XcmRouter>(sibling(), xcm()));
		assert!(OutboundMessages::<Runtime>::iter().next().is_none());
	});
}

#[test]
fn without_hrmp_xcm_goes_over_spec_msg_only_on_an_open_channel() {
	new_test_ext().execute_with(|| {
		assert!(send_xcm::<XcmRouter>(sibling(), xcm()).is_err());
		assert!(OutboundMessages::<Runtime>::iter().next().is_none());

		// `WithUniqueTopic` appends a topic, so decode rather than compare.
		open_spec_msg_channel();
		assert_ok!(send_xcm::<XcmRouter>(sibling(), xcm()));
		let sent = OutboundMessages::<Runtime>::get(xcm_channel(SIBLING.into()).data_stream());
		assert_eq!(sent.len(), 1);
		let Ok(ChannelMessage::Data(data)) = ChannelMessage::decode_all(&mut &sent[0][..]) else {
			panic!("XCM travels as a `Data` leaf");
		};
		assert!(VersionedXcm::<()>::decode_all(&mut &data[..]).is_ok());

		let root = SpecMessaging::commit_streams_root().expect("a stream was touched; qed");
		assert_eq!(SpecMessaging::current_streams_root(), Some(root));
		assert!(frame_system::Pallet::<Runtime>::digest()
			.logs
			.iter()
			.any(|log| matches!(log, DigestItem::Consensus(id, _) if *id == SPMS_ENGINE_ID)));
	});
}

#[test]
fn a_closing_hrmp_channel_diverts_new_xcm_while_it_drains() {
	new_test_ext().execute_with(|| {
		open_hrmp_channel();
		open_spec_msg_channel();
		assert_ok!(SpecMessaging::set_hrmp_closing(RuntimeOrigin::root(), SIBLING.into()));
		assert_ok!(send_xcm::<XcmRouter>(sibling(), xcm()));
		let stream = xcm_channel(SIBLING.into()).data_stream();
		assert_eq!(OutboundMessages::<Runtime>::get(stream).len(), 1);
	});
}

#[test]
fn consumed_payloads_fit_the_message_queue() {
	assert!(
		pallet_message_queue::MaxMessageLenOf::<Runtime>::get() >=
			<Runtime as cumulus_pallet_spec_messaging::Config>::MaxMsgLen::get(),
		"the message queue's `HeapSize` is too small for the pallet's `MaxMsgLen`",
	);
}

#[test]
fn a_full_delivery_queue_stops_consuming_its_channel() {
	new_test_ext().execute_with(|| {
		let channel = InChannelId { sender: SIBLING.into(), domain: 0, num: 0 };
		assert_ok!(SpecMessaging::accept_open_channel(
			RuntimeOrigin::root(),
			channel.sender,
			channel.domain,
			channel.num
		));
		assert!(SpecMessaging::consumed_streams().channels.contains_key(&channel));

		// A message of nearly a page each, so every one takes its own ready page.
		let body = vec![0u8; 100 * 1024];
		let message = BoundedSlice::try_from(&body[..]).expect("fits the queue; qed");
		type Queue = TransformOrigin<
			crate::MessageQueue,
			AggregateMessageOrigin,
			InChannelId,
			ChannelToSpecMsg,
		>;
		let limit = <Runtime as cumulus_pallet_spec_messaging::Config>::MaxQueueFootprint::get();
		for _ in 0..limit {
			Queue::enqueue_message(message, channel);
		}
		assert!(!SpecMessaging::consumed_streams().channels.contains_key(&channel));
	});
}

#[test]
fn an_xcm_one_framing_short_of_the_bound_is_the_largest_that_fits() {
	new_test_ext().execute_with(|| {
		open_spec_msg_channel();
		// Channel framing counts toward the protocol's message bound.
		assert_ok!(SpecMessaging::can_send(xcm_channel(SIBLING.into()), 102_395));
		assert!(SpecMessaging::can_send(xcm_channel(SIBLING.into()), 102_396).is_err());
	});
}

fn consume(payloads: &[Vec<u8>]) {
	use cumulus_pallet_spec_messaging::OnSpecMsgData;
	let stream = cumulus_primitives_spec_messaging::StreamId::Channel {
		recipient: crate::ParachainInfo::parachain_id(),
		domain: 0,
		num: 0,
	};
	for (i, payload) in payloads.iter().enumerate() {
		<Runtime as cumulus_pallet_spec_messaging::Config>::DataHandler::on_data(
			SIBLING.into(),
			stream,
			MessagePosition(i as u64),
			payload.clone(),
		);
	}
}

fn queue_events() -> Vec<pallet_message_queue::Event<Runtime>> {
	frame_system::Pallet::<Runtime>::events()
		.into_iter()
		.filter_map(|record| match record.event {
			crate::RuntimeEvent::MessageQueue(event) => Some(event),
			_ => None,
		})
		.collect()
}

#[test]
fn junk_and_overweight_xcm_do_not_hold_up_the_channel() {
	use codec::Encode;
	use frame_support::traits::ServiceQueues;
	use pallet_message_queue::Event;
	new_test_ext().execute_with(|| {
		let set_code = crate::RuntimeCall::System(frame_system::Call::set_code { code: vec![] });
		let transact = Transact {
			origin_kind: OriginKind::Superuser,
			call: set_code.encode().into(),
			fallback_max_weight: None,
		};
		// Weighed past what the queue may spend on one message.
		let overweight = Xcm::<()>(vec![transact.clone(), transact]);
		let payloads = [
			VersionedXcm::from(xcm()).encode(),
			vec![0xff; 64],
			VersionedXcm::from(overweight).encode(),
			VersionedXcm::from(Xcm::<()>(vec![ClearOrigin, ClearOrigin])).encode(),
		];
		consume(&payloads);
		<crate::MessageQueue as ServiceQueues>::service_queues(
			crate::MessageQueueServiceWeight::get(),
		);

		let index = |id: sp_core::H256| {
			payloads.iter().position(|p| sp_io::hashing::blake2_256(p) == id.0).unwrap()
		};
		let settled: Vec<_> = queue_events()
			.into_iter()
			.map(|event| match event {
				Event::Processed { id, .. } => (index(id), "processed"),
				Event::ProcessingFailed { id, .. } => (index(id), "failed"),
				Event::OverweightEnqueued { id, .. } => (index(id.into()), "overweight"),
				other => panic!("unexpected {other:?}"),
			})
			.collect();
		assert_eq!(settled, [(0, "processed"), (1, "failed"), (2, "overweight"), (3, "processed")]);
	});
}

fn next_block() {
	use frame_support::traits::Hooks;
	let number = frame_system::Pallet::<Runtime>::block_number();
	<SpecMessaging as Hooks<_>>::on_finalize(number);
	frame_system::Pallet::<Runtime>::initialize(
		&(number + 1),
		&Default::default(),
		&Default::default(),
	);
	<SpecMessaging as Hooks<_>>::on_initialize(number + 1);
	frame_system::Pallet::<Runtime>::note_finished_initialize();
}

fn sent_topics() -> Vec<u8> {
	let stream = xcm_channel(SIBLING.into()).data_stream();
	OutboundMessages::<Runtime>::get(stream)
		.iter()
		.map(|leaf| {
			let Ok(ChannelMessage::Data(data)) = ChannelMessage::decode_all(&mut &leaf[..]) else {
				panic!("XCM travels as a `Data` leaf");
			};
			let message: Xcm<()> =
				VersionedXcm::<()>::decode_all(&mut &data[..]).unwrap().try_into().unwrap();
			match message.0.last() {
				Some(SetTopic(topic)) => topic[0],
				other => panic!("no topic: {other:?}"),
			}
		})
		.collect()
}

#[test]
fn a_send_flood_stops_at_the_in_flight_window_in_order_and_raises_the_fee() {
	use cumulus_pallet_spec_messaging::DeliveryFeeFactor;
	new_test_ext().execute_with(|| {
		open_spec_msg_channel();
		frame_system::Pallet::<Runtime>::note_finished_initialize();
		let window: u32 = <Runtime as cumulus_pallet_spec_messaging::Config>::MaxInFlight::get();
		let topic = |n: u8| Xcm::<()>(vec![ClearOrigin, SetTopic([n; 32])]);
		let mut sent = Vec::new();
		for n in 0..200u8 {
			match send_xcm::<XcmRouter>(sibling(), topic(n)) {
				Ok(_) => {},
				Err(SendError::Transport("block has no room for the spec-msg send")) => {
					sent.extend(sent_topics());
					next_block();
					assert_ok!(send_xcm::<XcmRouter>(sibling(), topic(n)));
				},
				Err(e) => {
					assert_eq!(e, SendError::Transport("spec-msg XCM channel has no capacity"));
					assert_eq!(sent.len() + sent_topics().len(), window as usize, "at send {n}");
				},
			}
		}
		sent.extend(sent_topics());
		assert_eq!(sent, (0..window as u8).collect::<Vec<_>>(), "every accepted send, in order");
		assert!(DeliveryFeeFactor::<Runtime>::get(ParaId::from(SIBLING)) > 1.into());
	});
}
