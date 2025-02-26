// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(feature = "std", feature = "runtime-benchmarks"))]
pub mod benchmark_helpers;

#[cfg(any(feature = "std", test))]
pub mod mock_origin;

#[cfg(any(feature = "std", test))]
pub mod mock_outbound_queue;

#[cfg(any(feature = "std", test))]
pub mod mock_xcm;
