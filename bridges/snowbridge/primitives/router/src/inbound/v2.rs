// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Converts messages from Ethereum to XCM messages

use codec::{Decode, DecodeLimit, Encode};
use core::marker::PhantomData;
use frame_support::PalletError;
use scale_info::TypeInfo;
use sp_core::{Get, RuntimeDebug, H160, H256};
use sp_std::prelude::*;
use xcm::{
	prelude::{Junction::AccountKey20, *},
	MAX_XCM_DECODE_DEPTH,
};

const LOG_TARGET: &str = "snowbridge-router-primitives";

/// Messages from Ethereum are versioned. This is because in future,
/// we may want to evolve the protocol so that the ethereum side sends XCM messages directly.
/// Instead having BridgeHub transcode the messages into XCM.
#[derive(Clone, Encode, Decode, RuntimeDebug)]
pub enum VersionedMessage {
	V2(Message),
}

/// For V2, the ethereum side sends messages which are transcoded into XCM. These messages are
/// self-contained, in that they can be transcoded using only information in the message.
#[derive(Clone, Encode, Decode, RuntimeDebug)]
pub struct Message {
	/// The origin address
	pub origin: H160,
	/// The assets
	pub assets: Vec<InboundAsset>,
	// The command originating from the Gateway contract
	pub xcm: Vec<u8>,
	// The claimer in the case that funds get trapped.
	pub claimer: Option<Vec<u8>>,
}

#[derive(Clone, Encode, Decode, RuntimeDebug)]
pub enum InboundAsset {
	NativeTokenERC20 {
		/// The native token ID
		token_id: H160,
		/// The monetary value of the asset
		value: u128,
	},
	ForeignTokenERC20 {
		/// The foreign token ID
		token_id: H256,
		/// The monetary value of the asset
		value: u128,
	},
}

/// Reason why a message conversion failed.
#[derive(Copy, Clone, TypeInfo, PalletError, Encode, Decode, RuntimeDebug)]
pub enum ConvertMessageError {
	/// The XCM provided with the message could not be decoded into XCM.
	InvalidXCM,
	/// The XCM provided with the message could not be decoded into versioned XCM.
	InvalidVersionedXCM,
	/// Invalid claimer MultiAddress provided in payload.
	InvalidClaimer,
}

pub trait ConvertMessage {
	fn convert(message: Message) -> Result<Xcm<()>, ConvertMessageError>;
}

pub struct MessageToXcm<EthereumNetwork, InboundQueuePalletInstance>
where
	EthereumNetwork: Get<NetworkId>,
	InboundQueuePalletInstance: Get<u8>,
{
	_phantom: PhantomData<(EthereumNetwork, InboundQueuePalletInstance)>,
}

impl<EthereumNetwork, InboundQueuePalletInstance> ConvertMessage
	for MessageToXcm<EthereumNetwork, InboundQueuePalletInstance>
where
	EthereumNetwork: Get<NetworkId>,
	InboundQueuePalletInstance: Get<u8>,
{
	fn convert(message: Message) -> Result<Xcm<()>, ConvertMessageError> {
		let mut message_xcm : Xcm<()> = Xcm::new();
		if message.xcm.len() > 0{
			// Decode xcm
			let versioned_xcm = VersionedXcm::<()>::decode_with_depth_limit(
				MAX_XCM_DECODE_DEPTH,
				&mut message.xcm.as_ref(),
			)
				.map_err(|_| ConvertMessageError::InvalidVersionedXCM)?;
			message_xcm =
				versioned_xcm.try_into().map_err(|_| ConvertMessageError::InvalidXCM)?;
		}

		log::debug!(target: LOG_TARGET,"xcm decoded as {:?}", message_xcm);

		let network = EthereumNetwork::get();

		let origin_location = Location::new(2, GlobalConsensus(network))
			.push_interior(AccountKey20 { key: message.origin.into(), network: None })
			.map_err(|_| ConvertMessageError::InvalidXCM)?;

		let network = EthereumNetwork::get();

		let fee_asset = Location::new(1, Here);
		let fee_value = 1_000_000_000u128; // TODO needs to be dry-run to get the fee but also
									 // need to add a fee here for the dry run... Chicken/egg problem?
		let fee: Asset = (fee_asset, fee_value).into();
		let mut instructions = vec![
			ReceiveTeleportedAsset(fee.clone().into()),
			BuyExecution { fees: fee, weight_limit: Unlimited },
			DescendOrigin(PalletInstance(InboundQueuePalletInstance::get()).into()),
			UniversalOrigin(GlobalConsensus(network)),
		];

		for asset in &message.assets {
			match asset {
				InboundAsset::NativeTokenERC20 { token_id, value } => {
					let token_location: Location = Location::new(
						2,
						[
							GlobalConsensus(EthereumNetwork::get()),
							AccountKey20 { network: None, key: (*token_id).into() },
						],
					);
					instructions.push(ReserveAssetDeposited((token_location, *value).into()));
				},
				InboundAsset::ForeignTokenERC20 { token_id, value } => {
					// TODO check how token is represented as H256 on AH, assets pallet?
					let token_location: Location =
						Location::new(0, [AccountId32 { network: None, id: (*token_id).into() }]);
					// TODO Is this token always on AH? Would probably need to distinguish between
					// tokens on other parachains eventually
					instructions.push(WithdrawAsset((token_location, *value).into()));
				},
			}
		}

		if let Some(claimer) = message.claimer {
			let claimer = Junction::decode(&mut claimer.as_ref())
				.map_err(|_| ConvertMessageError::InvalidClaimer)?;
			let claimer_location: Location = Location::new(0, [claimer.into()]);
			instructions.push(SetAssetClaimer { location: claimer_location });
		}

		// Set the alias origin to the original sender on Ethereum. Important to be before the
		// arbitrary XCM that is appended to the message on the next line.
		instructions.push(AliasOrigin(origin_location.into()));

		// Add the XCM sent in the message to the end of the xcm instruction
		instructions.extend(message_xcm.0);

		Ok(instructions.into())
	}
}

#[cfg(test)]
mod tests {
	use crate::inbound::{
		v2::{ConvertMessage, Message, MessageToXcm},
		CallIndex, GlobalConsensusEthereumConvertsFor,
	};
	use codec::Decode;
	use frame_support::{assert_ok, parameter_types};
	use hex_literal::hex;
	use sp_runtime::traits::ConstU8;
	use xcm::prelude::*;
	use xcm_executor::traits::ConvertLocation;

	const NETWORK: NetworkId = Ethereum { chain_id: 11155111 };

	parameter_types! {
		pub EthereumNetwork: NetworkId = NETWORK;

		pub const CreateAssetCall: CallIndex = [1, 1];
		pub const CreateAssetExecutionFee: u128 = 123;
		pub const CreateAssetDeposit: u128 = 891;
		pub const SendTokenExecutionFee: u128 = 592;
	}

	#[test]
	fn test_contract_location_with_network_converts_successfully() {
		let expected_account: [u8; 32] =
			hex!("ce796ae65569a670d0c1cc1ac12515a3ce21b5fbf729d63d7b289baad070139d");
		let contract_location = Location::new(2, [GlobalConsensus(NETWORK)]);

		let account =
			GlobalConsensusEthereumConvertsFor::<[u8; 32]>::convert_location(&contract_location)
				.unwrap();

		assert_eq!(account, expected_account);
	}

	#[test]
	fn test_contract_location_with_incorrect_location_fails_convert() {
		let contract_location = Location::new(2, [GlobalConsensus(Polkadot), Parachain(1000)]);

		assert_eq!(
			GlobalConsensusEthereumConvertsFor::<[u8; 32]>::convert_location(&contract_location),
			None,
		);
	}

	#[test]
	fn test_reanchor_all_assets() {
		let ethereum_context: InteriorLocation = [GlobalConsensus(Ethereum { chain_id: 1 })].into();
		let ethereum = Location::new(2, ethereum_context.clone());
		let ah_context: InteriorLocation = [GlobalConsensus(Polkadot), Parachain(1000)].into();
		let global_ah = Location::new(1, ah_context.clone());
		let assets = vec![
			// DOT
			Location::new(1, []),
			// GLMR (Some Polkadot parachain currency)
			Location::new(1, [Parachain(2004)]),
			// AH asset
			Location::new(0, [PalletInstance(50), GeneralIndex(42)]),
			// KSM
			Location::new(2, [GlobalConsensus(Kusama)]),
			// KAR (Some Kusama parachain currency)
			Location::new(2, [GlobalConsensus(Kusama), Parachain(2000)]),
		];
		for asset in assets.iter() {
			// reanchor logic in pallet_xcm on AH
			let mut reanchored_asset = asset.clone();
			assert_ok!(reanchored_asset.reanchor(&ethereum, &ah_context));
			// reanchor back to original location in context of Ethereum
			let mut reanchored_asset_with_ethereum_context = reanchored_asset.clone();
			assert_ok!(
				reanchored_asset_with_ethereum_context.reanchor(&global_ah, &ethereum_context)
			);
			assert_eq!(reanchored_asset_with_ethereum_context, asset.clone());
		}
	}

	#[test]
	fn test_convert_message() {
		let payload = hex!("29e3b139f4393adda86303fcdaa35f60bb7092bf040197874824853fb4ad04794ccfd1cc8d2a7463839cfcbc6a315a1045c60ab85f400000b2d3595bf00600000000000000000000").to_vec();
		let message = Message::decode(&mut payload.as_ref());
		assert_ok!(message.clone());
		let result = MessageToXcm::<EthereumNetwork, ConstU8<80>>::convert(message.unwrap());
		assert_ok!(result);
	}
}
