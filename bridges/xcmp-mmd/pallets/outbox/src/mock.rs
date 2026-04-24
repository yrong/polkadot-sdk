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

//! Mock runtime for testing XCMP MMD outbox pallet.

use crate as xcmp_mmd_outbox;
use frame_support::{derive_impl, parameter_types};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::BuildStorage;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		XcmpMmdOutbox: xcmp_mmd_outbox,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

parameter_types! {
	pub const MaxPendingOutboxLeaves: u32 = 100;
}

/// Mock XCMP message source that returns test messages.
pub struct MockXcmpMessageSource;

impl cumulus_primitives_core::XcmpMessageSource for MockXcmpMessageSource {
	fn take_outbound_messages(
		_maximum_channels: usize,
		_excluded_recipients: &[ParaId],
	) -> alloc::vec::Vec<(ParaId, alloc::vec::Vec<u8>)> {
		// Return empty by default - tests will call note_outbound directly
		alloc::vec::Vec::new()
	}
}

impl xcmp_mmd_outbox::Config for Test {
	type OutboundXcmpMessageSource = MockXcmpMessageSource;
	type MaxPendingOutboxLeaves = MaxPendingOutboxLeaves;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	t.into()
}
