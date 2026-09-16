#[cfg(not(feature = "runtime-benchmarks"))]
use crate::XcmRouter;
use crate::{
	xcm_config, xcm_config::UniversalLocation, Balances, EthereumBeaconClient,
	EthereumInboundQueue, EthereumOutboundQueue, EthereumSystem, MessageQueue, Runtime,
	RuntimeEvent, TransactionByteFee, TreasuryAccount,
};
use parachains_common::{AccountId, Balance};
use snowbridge_beacon_primitives::{Fork, ForkVersions};
use snowbridge_core::{gwei, meth, AllowSiblingsOnly, PricingParameters, Rewards};
use snowbridge_inbound_queue_primitives::{
	v1::MessageToXcm as MessageToXcmV1,
	v2::{CreateAssetCallInfo, MessageToXcm, XcmMessageProcessor as InboundXcmMessageProcessor},
};
use snowbridge_outbound_queue_primitives::v1::EthereumBlobExporter;

use sp_core::H160;
use testnet_parachains_constants::rococo::{
	currency::*,
	fee::WeightToFee,
	snowbridge::{
		AssetHubParaId, EthereumLocation, EthereumNetwork, INBOUND_QUEUE_PALLET_INDEX,
		INBOUND_QUEUE_PALLET_INDEX_V2,
	},
};

use crate::xcm_config::RelayNetwork;
#[cfg(feature = "runtime-benchmarks")]
use benchmark_helpers::DoNothingRouter;
use bp_asset_hub_rococo::CreateForeignAssetDeposit;
use bp_relayers::RewardLedger;
use frame_support::{parameter_types, weights::ConstantMultiplier};
use hex_literal::hex;
use pallet_xcm::EnsureXcm;
use sp_runtime::{
	traits::{ConstU32, ConstU8, Keccak256},
	FixedU128,
};
use xcm::prelude::{GlobalConsensus, InteriorLocation, Location, PalletInstance, Parachain};
use xcm_executor::XcmExecutor;

/// Exports message to the Ethereum Gateway contract.
pub type SnowbridgeExporter = EthereumBlobExporter<
	UniversalLocation,
	EthereumNetwork,
	snowbridge_pallet_outbound_queue::Pallet<Runtime>,
	snowbridge_core::AgentIdOf,
	EthereumSystem,
>;

// Ethereum Bridge
parameter_types! {
	pub storage EthereumGatewayAddress: H160 = H160(hex!("EDa338E4dC46038493b885327842fD3E301CaB39"));
}

parameter_types! {
	pub const CreateAssetCall: [u8;2] = [53, 0];
	pub const CreateAssetCallIndex: [u8;2] = [53, 0];
	pub const SetReservesCallIndex: [u8;2] = [53, 33];
	pub Parameters: PricingParameters<u128> = PricingParameters {
		exchange_rate: FixedU128::from_rational(1, 400),
		fee_per_gas: gwei(20),
		rewards: Rewards { local: 1 * UNITS, remote: meth(1) },
		multiplier: FixedU128::from_rational(1, 1),
	};
	pub AssetHubFromEthereum: Location = Location::new(1,[GlobalConsensus(RelayNetwork::get()),Parachain(rococo_runtime_constants::system_parachain::ASSET_HUB_ID)]);
	pub EthereumUniversalLocation: InteriorLocation = [GlobalConsensus(EthereumNetwork::get())].into();
	pub InboundQueueV2Location: InteriorLocation = [PalletInstance(INBOUND_QUEUE_PALLET_INDEX_V2)].into();
	pub CreateAssetCallV2: CreateAssetCallInfo = CreateAssetCallInfo {
		create_call: CreateAssetCallIndex::get(),
		deposit: CreateForeignAssetDeposit::get(),
		min_balance: 1,
		set_reserves_call: SetReservesCallIndex::get(),
	};
	pub TargetLocation: Location = Location::new(1, [Parachain(AssetHubParaId::get().into())]);
	pub const DefaultSnowbridgeRewardKind: u8 = 0;
}

impl snowbridge_pallet_inbound_queue::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Verifier = snowbridge_pallet_ethereum_client::Pallet<Runtime>;
	type Token = Balances;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type XcmSender = XcmRouter;
	#[cfg(feature = "runtime-benchmarks")]
	type XcmSender = DoNothingRouter;
	type ChannelLookup = EthereumSystem;
	type GatewayAddress = EthereumGatewayAddress;
	#[cfg(feature = "runtime-benchmarks")]
	type Helper = Runtime;
	type MessageConverter = MessageToXcmV1<
		CreateAssetCall,
		CreateForeignAssetDeposit,
		ConstU8<INBOUND_QUEUE_PALLET_INDEX>,
		AccountId,
		Balance,
		EthereumSystem,
		EthereumUniversalLocation,
		AssetHubFromEthereum,
	>;
	type WeightToFee = WeightToFee;
	type LengthToFee = ConstantMultiplier<Balance, TransactionByteFee>;
	type MaxMessageSize = ConstU32<2048>;
	type WeightInfo = crate::weights::snowbridge_pallet_inbound_queue::WeightInfo<Runtime>;
	type PricingParameters = EthereumSystem;
	type AssetTransactor = <xcm_config::XcmConfig as xcm_executor::Config>::AssetTransactor;
}

pub type XcmMessageProcessorV2 = InboundXcmMessageProcessor<
	Runtime,
	crate::XcmRouter,
	XcmExecutor<xcm_config::XcmConfig>,
	MessageToXcm<
		CreateAssetCallV2,
		EthereumNetwork,
		RelayNetwork,
		EthereumGatewayAddress,
		InboundQueueV2Location,
		AssetHubParaId,
		EthereumSystem,
		AccountId,
	>,
	xcm_builder::AliasesIntoAccountId32<
		xcm_config::RelayNetwork,
		<Runtime as frame_system::Config>::AccountId,
	>,
	TargetLocation,
>;

/// Local E2E noop reward ledger. Rococo BH does not yet use westend's BridgeReward::Snowbridge
/// payment path; inbound verification still works without claiming rewards.
pub struct NoopRewardLedger;
impl RewardLedger<AccountId, u8, u128> for NoopRewardLedger {
	fn register_reward(_relayer: &AccountId, _reward: u8, _reward_balance: u128) {}
}

impl snowbridge_pallet_inbound_queue_v2::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Verifier = EthereumBeaconClient;
	type GatewayAddress = EthereumGatewayAddress;
	type WeightInfo = crate::weights::snowbridge_pallet_inbound_queue_v2::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type MessageProcessor = benchmark_helpers::DoNothingMessageProcessor;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type MessageProcessor = XcmMessageProcessorV2;
	type RewardKind = u8;
	type DefaultRewardKind = DefaultSnowbridgeRewardKind;
	type RewardPayment = NoopRewardLedger;
	#[cfg(feature = "runtime-benchmarks")]
	type Helper = Runtime;
}

impl snowbridge_pallet_outbound_queue::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Hashing = Keccak256;
	type MessageQueue = MessageQueue;
	type Decimals = ConstU8<12>;
	type MaxMessagePayloadSize = ConstU32<2048>;
	type MaxMessagesPerBlock = ConstU32<32>;
	type GasMeter = crate::ConstantGasMeter;
	type Balance = Balance;
	type WeightToFee = WeightToFee;
	type WeightInfo = crate::weights::snowbridge_pallet_outbound_queue::WeightInfo<Runtime>;
	type PricingParameters = EthereumSystem;
	type Channels = EthereumSystem;
}

#[cfg(any(feature = "std", feature = "fast-runtime", feature = "runtime-benchmarks", test))]
parameter_types! {
	// Local E2E (lodestar --params.*): mainnet-style fork versions, Fulu@0, Gloas@$GLOAS_FORK_EPOCH.
	// Must stay aligned with web/packages/test lodestar + beacon-relay forkVersions.gloas.
	pub const ChainForkVersions: ForkVersions = ForkVersions {
		genesis: Fork {
			version: hex!("00000000"),
			epoch: 0,
		},
		altair: Fork {
			version: hex!("01000000"),
			epoch: 0,
		},
		bellatrix: Fork {
			version: hex!("02000000"),
			epoch: 0,
		},
		capella: Fork {
			version: hex!("03000000"),
			epoch: 0,
		},
		deneb: Fork {
			version: hex!("04000000"),
			epoch: 0,
		},
		electra: Fork {
			version: hex!("05000000"),
			epoch: 0,
		},
		fulu: Fork {
			version: hex!("06000000"),
			epoch: 0,
		},
		gloas: Fork {
			version: hex!("07000000"),
			epoch: 40,
		}
	};
}

#[cfg(not(any(feature = "std", feature = "fast-runtime", feature = "runtime-benchmarks", test)))]
parameter_types! {
	pub const ChainForkVersions: ForkVersions = ForkVersions {
		genesis: Fork {
			version: hex!("90000069"),
			epoch: 0,
		},
		altair: Fork {
			version: hex!("90000070"),
			epoch: 50,
		},
		bellatrix: Fork {
			version: hex!("90000071"),
			epoch: 100,
		},
		capella: Fork {
			version: hex!("90000072"),
			epoch: 56832,
		},
		deneb: Fork {
			version: hex!("90000073"),
			epoch: 132608,
		},
		electra: Fork {
			version: hex!("90000074"),
			epoch: 222464,
		},
		fulu: Fork {
			version: hex!("90000075"),
			epoch: 272640, // https://notes.ethereum.org/@bbusa/fusaka-bpo-timeline
		},
		gloas: Fork {
			version: hex!("90000076"),
			epoch: 351232,
		},
	};
}

pub const SLOTS_PER_EPOCH: u32 = snowbridge_pallet_ethereum_client::config::SLOTS_PER_EPOCH as u32;

impl snowbridge_pallet_ethereum_client::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ForkVersions = ChainForkVersions;
	// Free consensus update every epoch. Works out to be 225 updates per day.
	type FreeHeadersInterval = ConstU32<SLOTS_PER_EPOCH>;
	type WeightInfo = crate::weights::snowbridge_pallet_ethereum_client::WeightInfo<Runtime>;
}

impl snowbridge_pallet_system::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OutboundQueue = EthereumOutboundQueue;
	type SiblingOrigin = EnsureXcm<AllowSiblingsOnly>;
	type AgentIdOf = snowbridge_core::AgentIdOf;
	type TreasuryAccount = TreasuryAccount;
	type Token = Balances;
	type WeightInfo = crate::weights::snowbridge_pallet_system::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type Helper = ();
	type DefaultPricingParameters = Parameters;
	type InboundDeliveryCost = EthereumInboundQueue;
	type UniversalLocation = UniversalLocation;
	type EthereumLocation = EthereumLocation;
}

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmark_helpers {
	use crate::{EthereumBeaconClient, Runtime, RuntimeOrigin};
	use codec::Encode;
	use parachains_common::AccountId;
	use snowbridge_inbound_queue_primitives::{
		v2::{Message, MessageProcessor, MessageProcessorError},
		EventFixture,
	};
	use snowbridge_pallet_inbound_queue::BenchmarkHelper;
	use snowbridge_pallet_inbound_queue_fixtures::register_token::make_register_token_message;
	use xcm::latest::{Assets, Location, SendError, SendResult, SendXcm, Xcm, XcmHash};

	impl<T: snowbridge_pallet_ethereum_client::Config> BenchmarkHelper<T> for Runtime {
		fn initialize_storage() -> EventFixture {
			let message = make_register_token_message();
			EthereumBeaconClient::store_finalized_header(
				message.finalized_header,
				message.block_roots_root,
			)
			.unwrap();
			message
		}
	}

	impl<T: snowbridge_pallet_inbound_queue_v2::Config>
		snowbridge_pallet_inbound_queue_v2::BenchmarkHelper<T> for Runtime
	{
		fn initialize_storage() -> EventFixture {
			let message = make_register_token_message();
			EthereumBeaconClient::store_finalized_header(
				message.finalized_header,
				message.block_roots_root,
			)
			.unwrap();
			message
		}
	}

	pub struct DoNothingRouter;
	impl SendXcm for DoNothingRouter {
		type Ticket = Xcm<()>;

		fn validate(
			_dest: &mut Option<Location>,
			xcm: &mut Option<Xcm<()>>,
		) -> SendResult<Self::Ticket> {
			Ok((xcm.clone().unwrap(), Assets::new()))
		}
		fn deliver(xcm: Xcm<()>) -> Result<XcmHash, SendError> {
			let hash = xcm.using_encoded(sp_io::hashing::blake2_256);
			Ok(hash)
		}
	}

	pub struct DoNothingMessageProcessor;
	impl MessageProcessor<AccountId> for DoNothingMessageProcessor {
		fn can_process_message(_relayer: &AccountId, _message: &Message) -> bool {
			true
		}
		fn process_message(
			_relayer: AccountId,
			_message: Message,
		) -> Result<[u8; 32], MessageProcessorError> {
			Ok([0u8; 32])
		}
	}

	impl snowbridge_pallet_system::BenchmarkHelper<RuntimeOrigin> for () {
		fn make_xcm_origin(location: Location) -> RuntimeOrigin {
			RuntimeOrigin::from(pallet_xcm::Origin::Xcm(location))
		}
	}
}
