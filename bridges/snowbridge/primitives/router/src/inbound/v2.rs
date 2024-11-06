// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Converts messages from Ethereum to XCM messages

use codec::{Decode, Encode};
use core::marker::PhantomData;
use frame_support::PalletError;
use scale_info::TypeInfo;
use sp_core::{Get, RuntimeDebug, H160, H256};
use sp_std::prelude::*;
use xcm::prelude::{Junction::AccountKey20, *};
use xcm::MAX_XCM_DECODE_DEPTH;
use codec::DecodeLimit;

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
		value: u128
	},
	ForeignTokenERC20 {
		/// The foreign token ID
		token_id: H256,
		/// The monetary value of the asset
		value: u128
	}
}

/// Reason why a message conversion failed.
#[derive(Copy, Clone, TypeInfo, PalletError, Encode, Decode, RuntimeDebug)]
pub enum ConvertMessageError {
	/// The XCM provided with the message could not be decoded into XCM.
	InvalidXCM,
	/// Invalid claimer MultiAddress provided in payload.
	InvalidClaimer,
}

pub trait ConvertMessage {
	fn convert(
		message: Message,
	) -> Result<Xcm<()>, ConvertMessageError>;
}

pub struct MessageToXcm<
	EthereumNetwork,
	AssetHubLocation,
	InboundQueuePalletInstance,
> where
	EthereumNetwork: Get<NetworkId>,
	AssetHubLocation: Get<InteriorLocation>,
	InboundQueuePalletInstance: Get<u8>,
{
	_phantom: PhantomData<(
		EthereumNetwork,
		AssetHubLocation,
		InboundQueuePalletInstance,
	)>,
}

impl<
	EthereumNetwork,
	AssetHubLocation,
	InboundQueuePalletInstance,
> ConvertMessage
for MessageToXcm<
	EthereumNetwork,
	AssetHubLocation,
	InboundQueuePalletInstance,
>
	where
		EthereumNetwork: Get<NetworkId>,
		AssetHubLocation: Get<InteriorLocation>,
		InboundQueuePalletInstance: Get<u8>,
{
	fn convert(message: Message) -> Result<Xcm<()>, ConvertMessageError> {
		// Decode xcm
		let versioned_xcm = VersionedXcm::<()>::decode_with_depth_limit(
			MAX_XCM_DECODE_DEPTH,
			&mut message.xcm.as_ref(),
		).map_err(|_| ConvertMessageError::InvalidXCM)?;
		let message_xcm: Xcm<()> = versioned_xcm.try_into().map_err(|_| ConvertMessageError::InvalidXCM)?;

		log::debug!(target: LOG_TARGET,"xcm decoded as {:?}", message_xcm);

		let network = EthereumNetwork::get();

		let mut origin_location = Location::new(2, GlobalConsensus(network)).push_interior(AccountKey20 {
			key: message.origin.into(), network: None
		}).map_err(|_| ConvertMessageError::InvalidXCM)?;

		let network = EthereumNetwork::get();

		let fee_asset = Location::new(1, Here);
		let fee_value = 1_000_000_000u128; // TODO configure
		let fee: Asset = (fee_asset, fee_value).into();
		let mut instructions = vec![
			ReceiveTeleportedAsset(fee.clone().into()),
		  	BuyExecution{fees: fee, weight_limit: Unlimited},
			DescendOrigin(PalletInstance(InboundQueuePalletInstance::get()).into()),
		  	UniversalOrigin(GlobalConsensus(network)),
			AliasOrigin(origin_location.into()),
		];

		for asset in &message.assets {
			match asset {
				InboundAsset::NativeTokenERC20 { token_id, value } => {
					let token_location: Location = Location::new(2, [GlobalConsensus(EthereumNetwork::get()), AccountKey20{network: None, key: (*token_id).into()}]);
					instructions.push(ReserveAssetDeposited((token_location, *value).into()));
				}
				InboundAsset::ForeignTokenERC20 { token_id, value } => {
					// TODO check how token is represented as H256 on AH, assets pallet?
					let token_location: Location = Location::new(0, [AccountId32 {network: None, id: (*token_id).into()}]);
					// TODO Is this token always on AH? Would probably need to distinguish between tokens on other parachains eventually
					instructions.push(WithdrawAsset((token_location, *value).into()));
				}
			}
		}

		if let Some(claimer) = message.claimer {
			let claimer = Junction::decode(&mut claimer.as_ref()).map_err(|_| ConvertMessageError::InvalidClaimer)?;
			let claimer_location: Location = Location::new(0, [claimer.into()]);
			instructions.push(SetAssetClaimer { location: claimer_location });
		}

		// TODO not sure this is correct, should the junction be prefixed with GlobalConsensus(EthereumNetwork::get()?
		instructions.push(DescendOrigin(AccountKey20 {
			key: message.origin.into(), network: None
		}.into()));

		// Add the XCM the user specified to the end of the XCM
		instructions.extend(message_xcm.0);

		Ok(instructions.into())
	}
}

#[cfg(test)]
mod tests {
	use crate::inbound::{CallIndex, GlobalConsensusEthereumConvertsFor};
	use frame_support::{assert_ok, parameter_types};
	use hex_literal::hex;
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
}
