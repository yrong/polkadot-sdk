// Copyright (C) Parity Technologies (UK) Ltd.
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

//! XCM between PenpalA and PenpalB over Speculative Messaging instead of HRMP.

use crate::imports::*;
use cumulus_pallet_spec_messaging::{xcm_channel, OutChannels};
use westend_system_emulated_network::penpal_emulated_chain::{
	penpal_runtime, PARA_ID_A, PARA_ID_B,
};

/// Whether `from`'s XCM channel to `to` has an open session, read on `from`.
fn xcm_channel_open(to: u32) -> bool {
	OutChannels::<penpal_runtime::Runtime>::get(xcm_channel(to.into()))
		.is_some_and(|state| state.open.is_some_and(|session| session.opened))
}

/// Open the spec-msg XCM channel PenpalA -> PenpalB and flag PenpalA's HRMP to PenpalB as
/// closing, so new XCM to PenpalB goes over spec-msg. The open takes a register round trip each
/// way, so the chains build blocks until PenpalA reads `Opened`.
fn open_xcm_channel_a_to_b() {
	let root = || penpal_runtime::RuntimeOrigin::root();
	PenpalB::execute_with(|| {
		assert_ok!(penpal_runtime::SpecMessaging::accept_open_channel(
			root(),
			PARA_ID_A.into(),
			0,
			0
		));
	});
	PenpalA::execute_with(|| {
		assert_ok!(penpal_runtime::SpecMessaging::open_channel(root(), PARA_ID_B.into(), 0, 0));
	});
	for _ in 0..10 {
		if PenpalA::execute_with(|| xcm_channel_open(PARA_ID_B)) {
			PenpalA::execute_with(|| {
				assert_ok!(penpal_runtime::SpecMessaging::set_hrmp_closing(
					root(),
					PARA_ID_B.into()
				));
			});
			return;
		}
		PenpalB::execute_with(|| {});
	}
	panic!("the spec-msg XCM channel PenpalA -> PenpalB did not open");
}

#[test]
fn xcm_goes_from_penpal_a_to_penpal_b_over_spec_msg() {
	open_xcm_channel_a_to_b();

	PenpalA::execute_with(|| {
		let dest = VersionedLocation::from(Location::new(1, [Parachain(PARA_ID_B)]));
		let message = VersionedXcm::from(Xcm::<()>(vec![UnsubscribeVersion]));
		assert_ok!(penpal_runtime::PolkadotXcm::send(
			penpal_runtime::RuntimeOrigin::root(),
			bx!(dest),
			bx!(message),
		));
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
		assert_expected_events!(
			PenpalB,
			vec![
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed {
					origin: cumulus_primitives_core::AggregateMessageOrigin::SpecMsg(channel),
					success: true,
					..
				}) => { channel: channel.sender == PARA_ID_A.into(), },
			]
		);
	});
}
