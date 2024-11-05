// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;

use frame_support::{
	derive_impl, parameter_types,
	traits::{Everything, Hooks},
	weights::IdentityFee,
	BoundedVec,
};

use hex_literal::hex;
use snowbridge_core::{
	gwei,
	inbound::{Log, Proof, VerificationError, Verifier},
	meth,
	outbound::v2::*,
	pricing::{PricingParameters, Rewards},
	ParaId,
};
use sp_core::{ConstU32, H160, H256};
use sp_runtime::{
	traits::{BlakeTwo256, IdentityLookup, Keccak256},
	AccountId32, BuildStorage, FixedU128,
};
use sp_std::marker::PhantomData;

type Block = frame_system::mocking::MockBlock<Test>;
type AccountId = AccountId32;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system::{Pallet, Call, Storage, Event<T>},
		MessageQueue: pallet_message_queue::{Pallet, Call, Storage, Event<T>},
		OutboundQueue: crate::{Pallet, Storage, Event<T>},
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type BaseCallFilter = Everything;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	type RuntimeTask = RuntimeTask;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type RuntimeEvent = RuntimeEvent;
	type PalletInfo = PalletInfo;
	type Nonce = u64;
	type Block = Block;
}

parameter_types! {
	pub const HeapSize: u32 = 32 * 1024;
	pub const MaxStale: u32 = 32;
	pub static ServiceWeight: Option<Weight> = Some(Weight::from_parts(100, 100));
}

impl pallet_message_queue::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type MessageProcessor = OutboundQueue;
	type Size = u32;
	type QueueChangeHandler = ();
	type HeapSize = HeapSize;
	type MaxStale = MaxStale;
	type ServiceWeight = ServiceWeight;
	type IdleMaxServiceWeight = ();
	type QueuePausedQuery = ();
}

// Mock verifier
pub struct MockVerifier;

impl Verifier for MockVerifier {
	fn verify(_: &Log, _: &Proof) -> Result<(), VerificationError> {
		Ok(())
	}
}

const GATEWAY_ADDRESS: [u8; 20] = hex!["eda338e4dc46038493b885327842fd3e301cab39"];

parameter_types! {
	pub const OwnParaId: ParaId = ParaId::new(1013);
	pub Parameters: PricingParameters<u128> = PricingParameters {
		exchange_rate: FixedU128::from_rational(1, 400),
		fee_per_gas: gwei(20),
		rewards: Rewards { local: DOT, remote: meth(1) },
		multiplier: FixedU128::from_rational(4, 3),
	};
	pub const GatewayAddress: H160 = H160(GATEWAY_ADDRESS);
	pub EthereumNetwork: NetworkId = NetworkId::Ethereum { chain_id: 11155111 };

}

pub const DOT: u128 = 10_000_000_000;
impl crate::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Verifier = MockVerifier;
	type GatewayAddress = GatewayAddress;
	type Hashing = Keccak256;
	type MessageQueue = MessageQueue;
	type MaxMessagePayloadSize = ConstU32<1024>;
	type MaxMessagesPerBlock = ConstU32<20>;
	type GasMeter = ConstantGasMeter;
	type Balance = u128;
	type WeightToFee = IdentityFee<u128>;
	type WeightInfo = ();
	type RewardLedger = ();
	type ConvertAssetId = ();
	type EthereumNetwork = EthereumNetwork;
}

fn setup() {
	System::set_block_number(1);
}

pub fn new_tester() -> sp_io::TestExternalities {
	let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext: sp_io::TestExternalities = storage.into();
	ext.execute_with(setup);
	ext
}

pub fn run_to_end_of_next_block() {
	// finish current block
	MessageQueue::on_finalize(System::block_number());
	OutboundQueue::on_finalize(System::block_number());
	System::on_finalize(System::block_number());
	// start next block
	System::set_block_number(System::block_number() + 1);
	System::on_initialize(System::block_number());
	OutboundQueue::on_initialize(System::block_number());
	MessageQueue::on_initialize(System::block_number());
	// finish next block
	MessageQueue::on_finalize(System::block_number());
	OutboundQueue::on_finalize(System::block_number());
	System::on_finalize(System::block_number());
}

pub fn mock_governance_message<T>() -> Message
where
	T: Config,
{
	let _marker = PhantomData::<T>; // for clippy

	Message {
		origin: primary_governance_origin(),
		id: Default::default(),
		fee: 0,
		commands: BoundedVec::try_from(vec![Command::Upgrade {
			impl_address: Default::default(),
			impl_code_hash: Default::default(),
			initializer: None,
		}])
		.unwrap(),
	}
}

// Message should fail validation as it is too large
pub fn mock_invalid_governance_message<T>() -> Message
where
	T: Config,
{
	let _marker = PhantomData::<T>; // for clippy

	Message {
		origin: Default::default(),
		id: Default::default(),
		fee: 0,
		commands: BoundedVec::try_from(vec![Command::Upgrade {
			impl_address: H160::zero(),
			impl_code_hash: H256::zero(),
			initializer: Some(Initializer {
				params: (0..1000).map(|_| 1u8).collect::<Vec<u8>>(),
				maximum_required_gas: 0,
			}),
		}])
		.unwrap(),
	}
}

pub fn mock_message(sibling_para_id: u32) -> Message {
	Message {
		origin: H256::from_low_u64_be(sibling_para_id as u64),
		id: Default::default(),
		fee: 0,
		commands: BoundedVec::try_from(vec![Command::UnlockNativeToken {
			agent_id: Default::default(),
			token: Default::default(),
			recipient: Default::default(),
			amount: 0,
		}])
		.unwrap(),
	}
}
