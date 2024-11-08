// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Helpers for implementing runtime api

use crate::{Config, Error};
use snowbridge_core::inbound::Proof;
use snowbridge_router_primitives::inbound::v2::{ConvertMessage, Message};
use xcm::{
	latest::Xcm,
	prelude::{Junction::*, Location, SendError as XcmpSendError, SendXcm},
};
pub fn dry_run<T>(message: Message, proof: Proof) -> Result<(Xcm<()>, u128), Error<T>>
where
	T: Config,
{
	Ok((Xcm::<()>::new(), 0))
}
