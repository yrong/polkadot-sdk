// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! # Location
//!
//! Location helpers for dealing with Polkadot locations

use crate::v2::Vec;
use codec::Encode;
use core::marker::PhantomData;
use frame_support::traits::Get;
use xcm::prelude::{GlobalConsensus, InteriorLocation, Location, Reanchorable};
use xcm_builder::DescribeLocation;

/// Resolves Polkadot locations (as seen by Ethereum) to an unique 32 bytes identifiers.
pub struct DescribeForEthereum<EthereumLocation, UniversalLocation, Suffix>(
	PhantomData<(EthereumLocation, UniversalLocation, Suffix)>,
);
impl<
		EthereumLocation: Get<Location>,
		UniversalLocation: Get<InteriorLocation>,
		Suffix: DescribeLocation,
	> DescribeLocation for DescribeForEthereum<EthereumLocation, UniversalLocation, Suffix>
{
	fn describe_location(l: &Location) -> Option<Vec<u8>> {
		let location = l.clone().reanchored(&EthereumLocation::get(), &UniversalLocation::get());
		let location = match location {
			Ok(l) => l,
			_ => return None,
		};
		match (location.parent_count(), location.first_interior()) {
			(n, Some(GlobalConsensus(network))) => {
				let mut tail = location.clone().split_first_interior().0;
				tail.dec_parent();
				let interior = Suffix::describe_location(&tail)?;
				Some((b"Parent", n, b"GlobalConsensus", network, b"Interior", interior).encode())
			},
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::v2::location::DescribeForEthereum;
	use frame_support::parameter_types;
	use xcm::prelude::{
		GeneralIndex, GeneralKey, InteriorLocation, Junction::*, Kusama, Location, NetworkId,
		PalletInstance, Parachain,
	};
	use xcm_builder::{DescribeAllTerminal, DescribeFamily, DescribeLocation, DescribeTerminus};

	parameter_types! {
		pub EthereumNetwork: NetworkId = NetworkId::Ethereum { chain_id: 1 };
		pub EthereumLocation: Location = Location::new(2, EthereumNetwork::get());
		pub const RelayNetwork: NetworkId = NetworkId::Polkadot;
		pub UniversalLocation: InteriorLocation =
		[GlobalConsensus(RelayNetwork::get()), Parachain(1002)].into();
	}

	#[test]
	fn test_describe_location() {
		let locations = [
			// Relay Chain
			Location::new(1, []),
			// Parachain
			Location::new(1, [Parachain(2000)]),
			// Parachain general index
			Location::new(1, [Parachain(2000), GeneralIndex(1)]),
			// Parachain general key
			Location::new(1, [Parachain(2000), GeneralKey { length: 32, data: [0; 32] }]),
			// Parachain account key 20
			Location::new(1, [Parachain(2000), AccountKey20 { network: None, key: [0; 20] }]),
			// Parachain account id 32
			Location::new(1, [Parachain(2000), AccountId32 { network: None, id: [0; 32] }]),
			// Parachain pallet instance
			Location::new(1, [Parachain(2000), PalletInstance(8)]),
			// Parachain Pallet general index
			Location::new(1, [Parachain(2000), PalletInstance(8), GeneralIndex(1)]),
			// Parachain Pallet general key
			Location::new(
				1,
				[Parachain(2000), PalletInstance(8), GeneralKey { length: 32, data: [0; 32] }],
			),
			// Parachain Pallet account key 20
			Location::new(
				1,
				[Parachain(2000), PalletInstance(8), AccountKey20 { network: None, key: [0; 20] }],
			),
			// Parachain Pallet account id 32
			Location::new(
				1,
				[Parachain(2000), PalletInstance(8), AccountId32 { network: None, id: [0; 32] }],
			),
			// KSM from Kusama
			Location::new(2, [GlobalConsensus(Kusama)]),
			// KAR from Acala on Kusama
			Location::new(2, [GlobalConsensus(Kusama), Parachain(2000)]),
		];

		for location in locations {
			assert!(
				DescribeForEthereum::<
					EthereumLocation,
					UniversalLocation,
					(DescribeTerminus, DescribeFamily<DescribeAllTerminal>),
				>::describe_location(&location)
				.is_some(),
				"Valid location = {location:?} yields no ID."
			);
		}
	}
}
