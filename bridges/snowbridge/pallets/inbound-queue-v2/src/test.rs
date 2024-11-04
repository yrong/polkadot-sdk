// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;

use frame_support::{assert_noop, assert_ok};
use hex_literal::hex;
use snowbridge_core::{inbound::Proof, ChannelId};
use sp_keyring::AccountKeyring as Keyring;
use sp_runtime::DispatchError;

use crate::{mock::*, Error, Event as InboundQueueEvent};
use snowbridge_router_primitives::inbound::v2::Asset;

#[test]
fn test_submit_happy_path() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let message = Message {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::submit(origin.clone(), message.clone()));
		expect_events(vec![InboundQueueEvent::MessageReceived {
			nonce: 1,
			message_id: [
				183, 243, 1, 130, 170, 254, 104, 45, 116, 181, 146, 237, 14, 139, 138, 89, 43, 166,
				182, 24, 163, 222, 112, 238, 215, 83, 21, 160, 24, 88, 112, 9,
			],
		}
		.into()]);
	});
}

#[test]
fn test_submit_xcm_invalid_channel() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Submit message
		let message = Message {
			event_log: mock_event_log_invalid_channel(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_noop!(
			InboundQueue::submit(origin.clone(), message.clone()),
			Error::<Test>::InvalidChannel,
		);
	});
}

#[test]
fn test_submit_with_invalid_gateway() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Submit message
		let message = Message {
			event_log: mock_event_log_invalid_gateway(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_noop!(
			InboundQueue::submit(origin.clone(), message.clone()),
			Error::<Test>::InvalidGateway
		);
	});
}

#[test]
fn test_submit_with_invalid_nonce() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Submit message
		let message = Message {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_ok!(InboundQueue::submit(origin.clone(), message.clone()));

		// Submit the same again
		assert_noop!(
			InboundQueue::submit(origin.clone(), message.clone()),
			Error::<Test>::InvalidNonce
		);
	});
}

#[test]
fn test_set_operating_mode() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);
		let message = Message {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::set_operating_mode(
			RuntimeOrigin::root(),
			snowbridge_core::BasicOperatingMode::Halted
		));

		assert_noop!(InboundQueue::submit(origin, message), Error::<Test>::Halted);
	});
}

#[test]
fn test_set_operating_mode_root_only() {
	new_tester().execute_with(|| {
		assert_noop!(
			InboundQueue::set_operating_mode(
				RuntimeOrigin::signed(Keyring::Bob.into()),
				snowbridge_core::BasicOperatingMode::Halted
			),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn test_send_native_erc20_token_payload() {
	new_tester().execute_with(|| {
		// To generate test data: forge test --match-test testSendEther  -vvvv
		let payload = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf04005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000b2d3595bf00600000000000000000000").to_vec();
		let message = MessageV2::decode(&mut payload.as_ref());
		assert_ok!(message.clone());

		let inbound_message = message.unwrap();

		let expected_origin: H160 = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf").into();
		let expected_token_id: H160 = hex!("5615deb798bb3e4dfa0139dfa1b3d433cc23b72f").into();
		let expected_value = 500000000000000000u128;
		let expected_xcm: Vec<u8> = vec![];
		let expected_claimer: Option<Vec<u8>> = None;

		assert_eq!(expected_origin, inbound_message.origin);
		assert_eq!(1, inbound_message.assets.len());
		if let Asset::NativeTokenERC20 { token_id, value } = &inbound_message.assets[0] {
			assert_eq!(expected_token_id, *token_id);
			assert_eq!(expected_value, *value);
		} else {
			panic!("Expected NativeTokenERC20 asset");
		}
		assert_eq!(expected_xcm, inbound_message.xcm);
		assert_eq!(expected_claimer, inbound_message.claimer);
	});
}
