// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use frame_support::traits::OriginTrait;
use xcm::prelude::Location;
pub trait BenchmarkHelper<O>
where
	O: OriginTrait,
{
	fn make_xcm_origin(location: Location) -> O;
}
