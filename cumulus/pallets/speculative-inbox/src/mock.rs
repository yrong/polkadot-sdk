// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use crate as speculative_inbox;
use cumulus_pallet_speculative_outbox as speculative_outbox;
use cumulus_primitives_core::ParaId;
use frame_support::{derive_impl, parameter_types, traits::Everything, weights::Weight};
use polkadot_parachain_primitives::primitives::XcmpMessageHandler;
use sp_core::H256;
use sp_runtime::{
	traits::{BlakeTwo256, IdentityLookup},
	BuildStorage,
};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		SpeculativeOutbox: speculative_outbox,
		SpeculativeInbox: speculative_inbox,
	}
);

parameter_types! {
	/// Receiver / inbox para id.
	pub const SelfParaId: ParaId = ParaId::new(2000);
	/// Sender / outbox para id — distinct so the `hash_leaf` source binding is
	/// meaningful (the outbox binds messages to *its* para id).
	pub const OutboxParaId: ParaId = ParaId::new(1000);
	pub ReservedXcmpWeight: Weight = Weight::from_parts(1_000_000, 0);
}

pub struct NoopXcmpHandler;
impl XcmpMessageHandler for NoopXcmpHandler {
	fn handle_xcmp_messages<'a, I: Iterator<Item = (ParaId, u32, &'a [u8])>>(
		_iter: I,
		_max_weight: Weight,
	) -> Weight {
		Weight::zero()
	}
}

pub struct NoopXcmpSource;
impl cumulus_primitives_core::XcmpMessageSource for NoopXcmpSource {
	fn take_outbound_messages(
		_maximum_channels: usize,
		_excluded_recipients: &[ParaId],
	) -> alloc::vec::Vec<(ParaId, alloc::vec::Vec<u8>)> {
		alloc::vec::Vec::new()
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type BaseCallFilter = Everything;
}

impl speculative_outbox::Config for Test {
	type InnerXcmpMessageSource = NoopXcmpSource;
	type SelfParaId = OutboxParaId;
}

impl speculative_inbox::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SelfParaId = SelfParaId;
	type XcmpMessageHandler = NoopXcmpHandler;
	type ReservedXcmpWeight = ReservedXcmpWeight;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	sp_io::TestExternalities::new(t)
}
