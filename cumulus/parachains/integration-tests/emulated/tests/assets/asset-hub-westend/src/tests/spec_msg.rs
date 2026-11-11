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

/// PenpalA's sovereign account on PenpalB.
fn sovereign_of_a_on_b() -> AccountId {
	PenpalB::sovereign_account_id_of(Location::new(1, [Parachain(PARA_ID_A)]))
}

/// Send `xcm` from PenpalA to PenpalB as root.
fn send_a_to_b(xcm: Xcm<()>) {
	PenpalA::execute_with(|| {
		let dest = VersionedLocation::from(Location::new(1, [Parachain(PARA_ID_B)]));
		assert_ok!(penpal_runtime::PolkadotXcm::send(
			penpal_runtime::RuntimeOrigin::root(),
			bx!(dest),
			bx!(VersionedXcm::from(xcm)),
		));
	});
}

/// PenpalB's events since its last block, read in one block: the emulator clears them after
/// each.
fn events_on_b() -> Vec<penpal_runtime::RuntimeEvent> {
	PenpalB::execute_with(|| PenpalB::events())
}

/// The XCMs `events` show PenpalB's message queue settled: origin transport and success.
fn settled(events: &[penpal_runtime::RuntimeEvent]) -> Vec<(&'static str, bool)> {
	events
		.iter()
		.filter_map(|event| match event {
			penpal_runtime::RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { origin, success, .. },
			) => Some((
				match origin {
					cumulus_primitives_core::AggregateMessageOrigin::SpecMsg(_) => "SpecMsg",
					cumulus_primitives_core::AggregateMessageOrigin::Sibling(_) => "Sibling",
					_ => "other",
				},
				*success,
			)),
			_ => None,
		})
		.collect()
}

/// Pay for a `System::remark_with_event` on PenpalB from PenpalA's sovereign account there, and
/// return to it what the execution did not use.
fn paid_remark() -> Xcm<()> {
	let fees: Asset = (Location::here(), penpal_runtime::UNIT).into();
	let remark = penpal_runtime::RuntimeCall::System(frame_system::Call::remark_with_event {
		remark: b"spec-msg parity".to_vec(),
	});
	Xcm(vec![
		WithdrawAsset(fees.clone().into()),
		BuyExecution { fees, weight_limit: Unlimited },
		Transact {
			origin_kind: OriginKind::SovereignAccount,
			fallback_max_weight: None,
			call: remark.encode().into(),
		},
		RefundSurplus,
		DepositAsset {
			assets: Wild(AllCounted(1)),
			beneficiary: Location::new(1, [Parachain(PARA_ID_A)]),
		},
	])
}

/// A paid `Transact` from PenpalA executes on PenpalB as its sovereign account, at the same cost
/// over HRMP and over spec-msg.
#[test]
fn a_paid_transact_is_the_same_over_hrmp_and_spec_msg() {
	let sovereign = sovereign_of_a_on_b();
	PenpalB::fund_accounts(vec![(sovereign.clone(), 10 * penpal_runtime::UNIT)]);
	let balance = || {
		PenpalB::execute_with(|| {
			<penpal_runtime::Balances as frame_support::traits::fungible::Inspect<_>>::balance(
				&sovereign_of_a_on_b(),
			)
		})
	};
	let mut runs = Vec::new();
	for transport in ["Sibling", "SpecMsg"] {
		if transport == "SpecMsg" {
			open_xcm_channel_a_to_b();
		}
		let before = balance();
		send_a_to_b(paid_remark());
		let events = events_on_b();
		assert_eq!(settled(&events), vec![(transport, true)], "over {transport}");
		let remarked = events.iter().any(|event| {
			matches!(
				event,
				penpal_runtime::RuntimeEvent::System(frame_system::Event::Remarked { sender, .. })
					if *sender == sovereign
			)
		});
		assert!(remarked, "no remark from PenpalA's sovereign account over {transport}");
		runs.push(before - balance());
	}
	assert_eq!(runs[0], runs[1], "the fee differs between HRMP and spec-msg");
	assert!(runs[0] > 0);
}

/// Assets an XCM trapped over HRMP are claimed over spec-msg: PenpalB sees the same origin
/// location for both transports, so the claim matches the trap.
#[test]
fn assets_trapped_over_hrmp_are_claimed_over_spec_msg() {
	let sovereign = sovereign_of_a_on_b();
	let sibling_a = Location::new(1, [Parachain(PARA_ID_A)]);
	PenpalB::fund_accounts(vec![(sovereign.clone(), 10 * penpal_runtime::UNIT)]);

	// Over HRMP: pay, then fail, so what the execution did not use is trapped.
	let fees: Asset = (Location::here(), penpal_runtime::UNIT).into();
	send_a_to_b(Xcm(vec![
		WithdrawAsset(fees.clone().into()),
		BuyExecution { fees, weight_limit: Unlimited },
		Trap(7),
	]));
	let events = events_on_b();
	assert_eq!(settled(&events), vec![("Sibling", false)]);
	let trapped = events
		.iter()
		.find_map(|event| match event {
			penpal_runtime::RuntimeEvent::PolkadotXcm(pallet_xcm::Event::AssetsTrapped {
				origin,
				assets,
				..
			}) => Some((origin.clone(), assets.clone())),
			_ => None,
		})
		.expect("the failed XCM trapped its assets");
	assert_eq!(trapped.0, sibling_a);
	let assets: Assets = trapped.1.try_into().expect("a current version");
	let claimed: Asset = assets.inner()[0].clone();

	// Over spec-msg: claim them and pay with them.
	open_xcm_channel_a_to_b();
	let before = PenpalB::execute_with(|| {
		<penpal_runtime::Balances as frame_support::traits::fungible::Inspect<_>>::balance(
			&sovereign,
		)
	});
	send_a_to_b(Xcm(vec![
		ClaimAsset { assets: assets.clone(), ticket: Location::new(0, [GeneralIndex(5)]) },
		BuyExecution { fees: claimed, weight_limit: Unlimited },
		DepositAsset { assets: Wild(AllCounted(1)), beneficiary: sibling_a.clone() },
	]));
	let events = events_on_b();
	assert_eq!(settled(&events), vec![("SpecMsg", true)]);
	assert!(events.iter().any(|event| matches!(
		event,
		penpal_runtime::RuntimeEvent::PolkadotXcm(pallet_xcm::Event::AssetsClaimed { origin, .. })
			if *origin == sibling_a
	)));
	let after = PenpalB::execute_with(|| {
		<penpal_runtime::Balances as frame_support::traits::fungible::Inspect<_>>::balance(
			&sovereign,
		)
	});
	assert!(after > before, "the claim deposited nothing back");
}

/// What a dry run of PenpalA's send to PenpalB forwards there, without the unique `SetTopic` each
/// send gets, and the delivery fee PenpalA quotes for it in its native asset.
fn dry_run_send_a_to_b() -> (Vec<Xcm<()>>, u128) {
	use xcm_runtime_apis::{
		dry_run::runtime_decl_for_dry_run_api::DryRunApiV2,
		fees::runtime_decl_for_xcm_payment_api::XcmPaymentApiV2,
	};
	PenpalA::execute_with(|| {
		type Runtime = <PenpalA as Chain>::Runtime;
		type OriginCaller = <PenpalA as Chain>::OriginCaller;
		let dest = VersionedLocation::from(Location::new(1, [Parachain(PARA_ID_B)]));
		let call = penpal_runtime::RuntimeCall::PolkadotXcm(pallet_xcm::Call::send {
			dest: bx!(dest.clone()),
			message: bx!(VersionedXcm::from(paid_remark())),
		});
		let origin = OriginCaller::system(frame_support::dispatch::RawOrigin::Root);
		let effects = Runtime::dry_run_call(origin, call, XCM_VERSION).expect("dry run");
		let forwarded = effects
			.forwarded_xcms
			.into_iter()
			.find_map(|(to, messages)| (to == dest).then_some(messages))
			.expect("the dry run forwards to PenpalB");
		let fee = Runtime::query_delivery_fees(
			dest,
			forwarded[0].clone(),
			VersionedAssetId::from(Location::here()),
		)
		.expect("a delivery fee quote");
		let forwarded = forwarded
			.into_iter()
			.map(|message| {
				let mut message: Xcm<()> = message.try_into().expect("a current version");
				assert!(matches!(message.0.pop(), Some(SetTopic(_))));
				message
			})
			.collect();
		(forwarded, get_amount_from_versioned_assets(fee))
	})
}

/// A dry run of PenpalA's send forwards the same XCM to PenpalB, and the same delivery fee is
/// quoted, before and after the cutover to spec-msg.
#[test]
fn a_dry_run_forwards_the_same_xcm_and_fee_over_hrmp_and_spec_msg() {
	let over_hrmp = dry_run_send_a_to_b();
	open_xcm_channel_a_to_b();
	let over_spec_msg = dry_run_send_a_to_b();
	assert_eq!(over_hrmp.0.len(), 1);
	assert_eq!(over_hrmp, over_spec_msg);
	assert!(over_hrmp.1 > 0);
}

/// PenpalB's native asset, held on PenpalA as a foreign asset, goes back to PenpalB with
/// `transfer_assets` (PenpalB is the reserve): the beneficiary receives the same amount over HRMP
/// and over spec-msg.
#[test]
fn a_reserve_transfer_back_home_is_the_same_over_hrmp_and_spec_msg() {
	let b_native = Location::new(1, [Parachain(PARA_ID_B)]);
	let sender = PenpalASender::get();
	let beneficiary = PenpalBReceiver::get();
	let amount = 10 * penpal_runtime::UNIT;
	PenpalA::force_create_foreign_asset(
		b_native.clone(),
		PenpalAssetOwner::get(),
		true,
		penpal_runtime::EXISTENTIAL_DEPOSIT,
		vec![(sender.clone(), 4 * amount)],
	);
	PenpalB::fund_accounts(vec![(sovereign_of_a_on_b(), 4 * amount)]);
	let balance = || {
		PenpalB::execute_with(|| {
			<penpal_runtime::Balances as frame_support::traits::fungible::Inspect<_>>::balance(
				&beneficiary,
			)
		})
	};

	let mut received = Vec::new();
	for transport in ["Sibling", "SpecMsg"] {
		if transport == "SpecMsg" {
			open_xcm_channel_a_to_b();
		}
		let before = balance();
		PenpalA::execute_with(|| {
			let dest = Location::new(1, [Parachain(PARA_ID_B)]);
			let to: Location = AccountId32 { network: None, id: beneficiary.clone().into() }.into();
			assert_ok!(penpal_runtime::PolkadotXcm::transfer_assets(
				penpal_runtime::RuntimeOrigin::signed(sender.clone()),
				bx!(dest.into()),
				bx!(to.into()),
				bx!(vec![(b_native.clone(), amount).into()].into()),
				0,
				Unlimited,
			));
		});
		let events = events_on_b();
		assert_eq!(settled(&events), vec![(transport, true)], "over {transport}");
		received.push(balance() - before);
	}
	assert_eq!(received[0], received[1], "the amount received differs between HRMP and spec-msg");
	assert!(received[0] > 0 && received[0] < amount);
}

/// The messages waiting in PenpalB's queues, by origin transport.
fn queued_on_b() -> Vec<(&'static str, u64)> {
	PenpalB::execute_with(|| {
		use frame_support::traits::QueueFootprintQuery;
		pallet_message_queue::BookStateFor::<penpal_runtime::Runtime>::iter_keys()
			.map(|origin| {
				let count = penpal_runtime::MessageQueue::footprint(origin.clone()).storage.count;
				(origin, count)
			})
			.filter(|(_, count)| *count > 0)
			.map(|(origin, count)| {
				let transport = match origin {
					cumulus_primitives_core::AggregateMessageOrigin::SpecMsg(_) => "SpecMsg",
					cumulus_primitives_core::AggregateMessageOrigin::Sibling(_) => "Sibling",
					_ => "other",
				};
				(transport, count)
			})
			.collect()
	})
}

/// Suspending XCM execution on PenpalB holds an XCM from PenpalA in its queue until it resumes,
/// over HRMP and over spec-msg.
#[test]
fn suspended_xcm_execution_holds_hrmp_and_spec_msg_alike() {
	let root = || penpal_runtime::RuntimeOrigin::root();
	let sovereign = sovereign_of_a_on_b();
	PenpalB::fund_accounts(vec![(sovereign.clone(), 10 * penpal_runtime::UNIT)]);
	let balance = || {
		PenpalB::execute_with(|| {
			<penpal_runtime::Balances as frame_support::traits::fungible::Inspect<_>>::balance(
				&sovereign,
			)
		})
	};
	for transport in ["Sibling", "SpecMsg"] {
		if transport == "SpecMsg" {
			open_xcm_channel_a_to_b();
		}
		PenpalB::execute_with(|| {
			assert_ok!(penpal_runtime::XcmpQueue::suspend_xcm_execution(root()));
		});
		let before = balance();
		send_a_to_b(paid_remark());
		assert_eq!(settled(&events_on_b()), vec![], "executed while suspended over {transport}");
		assert_eq!(queued_on_b(), vec![(transport, 1)], "over {transport}");
		assert_eq!(balance(), before);
		PenpalB::execute_with(|| {
			assert_ok!(penpal_runtime::XcmpQueue::resume_xcm_execution(root()));
		});
		assert_eq!(queued_on_b(), vec![], "still queued after resuming over {transport}");
		assert!(balance() < before, "no fee paid after resuming over {transport}");
	}
}

/// PenpalA's version subscription reaches PenpalB over HRMP and over spec-msg alike: PenpalB starts
/// notifying, and PenpalA learns its version from the reply, which comes back over HRMP.
#[test]
fn a_version_subscription_is_the_same_over_hrmp_and_spec_msg() {
	let root = || penpal_runtime::RuntimeOrigin::root();
	let b = Location::new(1, [Parachain(PARA_ID_B)]);
	let a = Location::new(1, [Parachain(PARA_ID_A)]);
	for transport in ["Sibling", "SpecMsg"] {
		if transport == "SpecMsg" {
			open_xcm_channel_a_to_b();
		}
		PenpalA::execute_with(|| {
			assert_ok!(penpal_runtime::PolkadotXcm::force_subscribe_version_notify(
				root(),
				bx!(b.clone().into()),
			));
		});
		let events = events_on_b();
		assert_eq!(settled(&events), vec![(transport, true)], "over {transport}");
		assert!(
			events.iter().any(|event| matches!(
				event,
				penpal_runtime::RuntimeEvent::PolkadotXcm(
					pallet_xcm::Event::VersionNotifyStarted { destination, .. }
				) if *destination == a
			)),
			"PenpalB did not start notifying PenpalA over {transport}"
		);
		let learned = PenpalA::execute_with(|| {
			PenpalA::events().iter().any(|event| {
				matches!(
					event,
					penpal_runtime::RuntimeEvent::PolkadotXcm(
						pallet_xcm::Event::SupportedVersionChanged { location, version }
					) if *location == b && *version == XCM_VERSION
				)
			})
		});
		assert!(learned, "PenpalA did not learn PenpalB's version over {transport}");
		PenpalA::execute_with(|| {
			assert_ok!(penpal_runtime::PolkadotXcm::force_unsubscribe_version_notify(
				root(),
				bx!(b.clone().into()),
			));
		});
		// PenpalA's reply to PenpalB's own version discovery may land in the same block.
		let settled = settled(&events_on_b());
		assert!(!settled.is_empty(), "no unsubscribe over {transport}");
		assert!(settled.iter().all(|s| *s == (transport, true)), "over {transport}: {settled:?}");
	}
}
