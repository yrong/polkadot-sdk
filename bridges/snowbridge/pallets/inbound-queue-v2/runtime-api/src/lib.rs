// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::traits::tokens::Balance as BalanceT;
use snowbridge_core::{
	inbound::v2::InboundMessage,
	PricingParameters,
};

sp_api::decl_runtime_apis! {
	pub trait InboundQueueApi<Balance> where Balance: BalanceT
	{
		/// Dry runs the provided message on AH to provide the XCM payload and execution cost.
		fn dry_run(message: InboundMessage, proof: ) -> (Xcm, u128);
	}
}
