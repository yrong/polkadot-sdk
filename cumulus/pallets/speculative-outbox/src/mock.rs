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

use crate as speculative_outbox;
use cumulus_primitives_core::ParaId;
use frame_support::{derive_impl, parameter_types, traits::Everything};
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
	}
);

parameter_types! {
	/// Sender / outbox para id — bound into each message's `hash_leaf` preimage.
	pub const OutboxParaId: ParaId = ParaId::new(1000);
}

/// No-op inner source; the rotation tests drive `record_outbound_messages` directly.
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

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	sp_io::TestExternalities::new(t)
}
