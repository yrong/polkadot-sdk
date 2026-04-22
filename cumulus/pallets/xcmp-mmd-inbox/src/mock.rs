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

//! Mock runtime for testing XCMP MMD inbox pallet.

use crate as xcmp_mmd_inbox;
use cumulus_pallet_parachain_system::RelayNumberMonotonicallyIncreases;
use frame_support::{derive_impl, parameter_types};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::{BuildStorage, traits::IdentityLookup};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		ParachainSystem: cumulus_pallet_parachain_system,
		XcmpMmdInbox: xcmp_mmd_inbox,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type OnSetCode = cumulus_pallet_parachain_system::ParachainSetCode<Self>;
}

parameter_types! {
	pub const SelfParaId: ParaId = ParaId::new(2000);
	pub const MaxRelayMmrProofItems: u32 = 128;
	pub const MaxParaHeadsProofItems: u32 = 32;
	pub const MaxOutboxMmrProofItems: u32 = 64;
	pub const MaxPayloadBytes: u32 = 256 * 1024;
}

impl cumulus_pallet_parachain_system::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type OnSystemEvent = ();
	type SelfParaId = SelfParaId;
	type ReservedDmpWeight = ();
	type OutboundXcmpMessageSource = ();
	type XcmpMessageHandler = MockXcmpMessageHandler;
	type ReservedXcmpWeight = ();
	type CheckAssociatedRelayNumber = RelayNumberMonotonicallyIncreases;
	type ConsensusHook = cumulus_pallet_parachain_system::consensus_hook::ExpectParentIncluded;
	type WeightInfo = ();
	type DmpQueue = ();
	type RelayParentOffset = ();
}

/// Mock XCMP message handler
pub struct MockXcmpMessageHandler;

impl cumulus_primitives_core::XcmpMessageHandler for MockXcmpMessageHandler {
	fn handle_xcmp_messages<'a, I: Iterator<Item = (ParaId, u32, &'a [u8])>>(
		_iter: I,
		_max_weight: frame_support::weights::Weight,
	) -> frame_support::weights::Weight {
		frame_support::weights::Weight::zero()
	}
}

impl xcmp_mmd_inbox::Config for Test {
	type XcmpMessageHandler = MockXcmpMessageHandler;
	type SelfParaId = SelfParaId;
	type MaxRelayMmrProofItems = MaxRelayMmrProofItems;
	type MaxParaHeadsProofItems = MaxParaHeadsProofItems;
	type MaxOutboxMmrProofItems = MaxOutboxMmrProofItems;
	type MaxPayloadBytes = MaxPayloadBytes;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	t.into()
}
