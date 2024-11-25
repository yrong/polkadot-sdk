// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;

use crate::{self as inbound_queue_v2};
use frame_support::{
	derive_impl, parameter_types,
	traits::{ConstU128, ConstU32},
};
use hex_literal::hex;
use snowbridge_beacon_primitives::{
	types::deneb, BeaconHeader, ExecutionProof, Fork, ForkVersions, VersionedExecutionPayloadHeader,
};
use snowbridge_core::{
	inbound::{Log, Proof, VerificationError},
	TokenId,
};
use snowbridge_router_primitives::inbound::v2::MessageToXcm;
use sp_core::H160;
use sp_runtime::{
	traits::{IdentifyAccount, IdentityLookup, MaybeEquivalence, Verify},
	BuildStorage, MultiSignature,
};
use sp_std::{convert::From, default::Default};
use xcm::prelude::*;
use xcm_executor::{traits::TransactAsset, AssetsInHolding};
use xcm_builder::SendControllerWeightInfo;
use sp_runtime::DispatchError;
use sp_core::H256;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system::{Pallet, Call, Storage, Event<T>},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		EthereumBeaconClient: snowbridge_pallet_ethereum_client::{Pallet, Call, Storage, Event<T>},
		InboundQueue: inbound_queue_v2::{Pallet, Call, Storage, Event<T>},
	}
);

pub type Signature = MultiSignature;
pub type AccountId = <<Signature as Verify>::Signer as IdentifyAccount>::AccountId;

type Balance = u128;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type AccountData = pallet_balances::AccountData<u128>;
	type Block = Block;
}

parameter_types! {
	pub const ExistentialDeposit: u128 = 1;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

parameter_types! {
	pub const ChainForkVersions: ForkVersions = ForkVersions{
		genesis: Fork {
			version: [0, 0, 0, 1], // 0x00000001
			epoch: 0,
		},
		altair: Fork {
			version: [1, 0, 0, 1], // 0x01000001
			epoch: 0,
		},
		bellatrix: Fork {
			version: [2, 0, 0, 1], // 0x02000001
			epoch: 0,
		},
		capella: Fork {
			version: [3, 0, 0, 1], // 0x03000001
			epoch: 0,
		},
		deneb: Fork {
			version: [4, 0, 0, 1], // 0x04000001
			epoch: 4294967295,
		}
	};
}

impl snowbridge_pallet_ethereum_client::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type ForkVersions = ChainForkVersions;
	type FreeHeadersInterval = ConstU32<32>;
	type WeightInfo = ();
}

// Mock verifier
pub struct MockVerifier;

impl Verifier for MockVerifier {
	fn verify(_: &Log, _: &Proof) -> Result<(), VerificationError> {
		Ok(())
	}
}

const GATEWAY_ADDRESS: [u8; 20] = hex!["eda338e4dc46038493b885327842fd3e301cab39"];

#[cfg(feature = "runtime-benchmarks")]
impl<T: snowbridge_pallet_ethereum_client::Config> BenchmarkHelper<T> for Test {
	// not implemented since the MockVerifier is used for tests
	fn initialize_storage(_: BeaconHeader, _: H256) {}
}


pub struct MockXcmSenderWeights;

impl SendControllerWeightInfo for MockXcmSenderWeights {
	fn send() -> Weight {
		return Weight::default();
	}
}

// Mock XCM sender that always succeeds
pub struct MockXcmSender;

impl SendController<mock::RuntimeOrigin> for MockXcmSender {
	type WeightInfo = MockXcmSenderWeights;
	fn send(
		_origin: mock::RuntimeOrigin,
		_dest: Box<VersionedLocation>,
		_message: Box<VersionedXcm<()>>,
	) -> Result<XcmHash, DispatchError> {
		Ok(H256::random().into())
	}
}

pub struct MockTokenIdConvert;
impl MaybeEquivalence<TokenId, Location> for MockTokenIdConvert {
	fn convert(_id: &TokenId) -> Option<Location> {
		Some(Location::parent())
	}
	fn convert_back(_loc: &Location) -> Option<TokenId> {
		None
	}
}

parameter_types! {
	pub const EthereumNetwork: xcm::v5::NetworkId = xcm::v5::NetworkId::Ethereum { chain_id: 11155111 };
	pub const GatewayAddress: H160 = H160(GATEWAY_ADDRESS);
	pub const InboundQueuePalletInstance: u8 = 80;
	pub AssetHubLocation: InteriorLocation = Parachain(1000).into();
}

impl inbound_queue_v2::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Verifier = MockVerifier;
	type XcmSender = MockXcmSender;
	type WeightInfo = ();
	type GatewayAddress = GatewayAddress;
	type AssetHubParaId = ConstU32<1000>;
	type MessageConverter =
		MessageToXcm<EthereumNetwork, InboundQueuePalletInstance, MockTokenIdConvert>;
	type Token = Balances;
	type XcmPrologueFee = ConstU128<1_000_000_000>;
	type AssetTransactor = SuccessfulTransactor;
	#[cfg(feature = "runtime-benchmarks")]
	type Helper = Test;
}

pub struct SuccessfulTransactor;
impl TransactAsset for SuccessfulTransactor {
	fn can_check_in(_origin: &Location, _what: &Asset, _context: &XcmContext) -> XcmResult {
		Ok(())
	}

	fn can_check_out(_dest: &Location, _what: &Asset, _context: &XcmContext) -> XcmResult {
		Ok(())
	}

	fn deposit_asset(_what: &Asset, _who: &Location, _context: Option<&XcmContext>) -> XcmResult {
		Ok(())
	}

	fn withdraw_asset(
		_what: &Asset,
		_who: &Location,
		_context: Option<&XcmContext>,
	) -> Result<AssetsInHolding, XcmError> {
		Ok(AssetsInHolding::default())
	}

	fn internal_transfer_asset(
		_what: &Asset,
		_from: &Location,
		_to: &Location,
		_context: &XcmContext,
	) -> Result<AssetsInHolding, XcmError> {
		Ok(AssetsInHolding::default())
	}
}

pub fn last_events(n: usize) -> Vec<RuntimeEvent> {
	frame_system::Pallet::<Test>::events()
		.into_iter()
		.rev()
		.take(n)
		.rev()
		.map(|e| e.event)
		.collect()
}

pub fn expect_events(e: Vec<RuntimeEvent>) {
	assert_eq!(last_events(e.len()), e);
}

pub fn setup() {
	System::set_block_number(1);
}

pub fn new_tester() -> sp_io::TestExternalities {
	let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext: sp_io::TestExternalities = storage.into();
	ext.execute_with(setup);
	ext
}

// Generated from smoketests:
//   cd smoketests
//   ./make-bindings
//   cargo test --test register_token -- --nocapture
pub fn mock_event_log() -> Log {
	Log {
        // gateway address
        address: hex!("eda338e4dc46038493b885327842fd3e301cab39").into(),
        topics: vec![
            hex!("7153f9357c8ea496bba60bf82e67143e27b64462b49041f8e689e1b05728f84f").into(),
            // channel id
            hex!("c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539").into(),
            // message id
            hex!("5f7060e971b0dc81e63f0aa41831091847d97c1a4693ac450cc128c7214e65e0").into(),
        ],
        // Nonce + Payload
        data: hex!("00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000002e000f000000000000000087d1f7fdfee7f651fabc8bfcb6e086c278b77a7d00e40b54020000000000000000000000000000000000000000000000000000000000").into(),
    }
}

pub fn mock_event_log_invalid_channel() -> Log {
	Log {
        address: hex!("eda338e4dc46038493b885327842fd3e301cab39").into(),
        topics: vec![
            hex!("7153f9357c8ea496bba60bf82e67143e27b64462b49041f8e689e1b05728f84f").into(),
            // invalid channel id
            hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
            hex!("5f7060e971b0dc81e63f0aa41831091847d97c1a4693ac450cc128c7214e65e0").into(),
        ],
        data: hex!("00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000001e000f000000000000000087d1f7fdfee7f651fabc8bfcb6e086c278b77a7d0000").into(),
    }
}

pub fn mock_event_log_invalid_gateway() -> Log {
	Log {
        // gateway address
        address: H160::zero(),
        topics: vec![
            hex!("7153f9357c8ea496bba60bf82e67143e27b64462b49041f8e689e1b05728f84f").into(),
            // channel id
            hex!("c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539").into(),
            // message id
            hex!("5f7060e971b0dc81e63f0aa41831091847d97c1a4693ac450cc128c7214e65e0").into(),
        ],
        // Nonce + Payload
        data: hex!("00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000001e000f000000000000000087d1f7fdfee7f651fabc8bfcb6e086c278b77a7d0000").into(),
    }
}

pub fn mock_execution_proof() -> ExecutionProof {
	ExecutionProof {
		header: BeaconHeader::default(),
		ancestry_proof: None,
		execution_header: VersionedExecutionPayloadHeader::Deneb(deneb::ExecutionPayloadHeader {
			parent_hash: Default::default(),
			fee_recipient: Default::default(),
			state_root: Default::default(),
			receipts_root: Default::default(),
			logs_bloom: vec![],
			prev_randao: Default::default(),
			block_number: 0,
			gas_limit: 0,
			gas_used: 0,
			timestamp: 0,
			extra_data: vec![],
			base_fee_per_gas: Default::default(),
			block_hash: Default::default(),
			transactions_root: Default::default(),
			withdrawals_root: Default::default(),
			blob_gas_used: 0,
			excess_blob_gas: 0,
		}),
		execution_branch: vec![],
	}
}
