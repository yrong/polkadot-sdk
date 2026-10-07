// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Weights for `cumulus_pallet_spec_messaging`.
//!
//! TODO: replace the `()` estimates with benchmark output (`benchmarking.rs`).

use frame_support::weights::Weight;

/// Weight functions of the spec-messaging pallet.
pub trait WeightInfo {
	fn open_channel() -> Weight;
	fn accept_open_channel() -> Weight;
	fn close_channel() -> Weight;
	fn close_inbound_channel() -> Weight;
	fn suspend_inbound_channel() -> Weight;
	fn resume_inbound_channel() -> Weight;
	fn set_hrmp_closing() -> Weight;
	fn clear_hrmp_closing() -> Weight;
	/// `c` channel items, `n` payloads beyond one per item, `b` payload bytes in total, `r`
	/// register reads.
	fn enact_messages(c: u32, n: u32, b: u32, r: u32) -> Weight;
	/// One send of a `b`-byte payload: the leaf hash and the queue writes.
	fn send(b: u32) -> Weight;
	/// The `on_initialize` drain of `h` leaf hashes, each in its own stream (the worst case).
	fn drain(h: u32) -> Weight;
	/// The end-of-block `StreamsRoot` fold over `s` streams with `h` new leaf hashes, each in its
	/// own stream (the worst case).
	fn commit_streams_root(s: u32, h: u32) -> Weight;
}

/// Estimates from storage access counts alone, until benchmarked.
impl WeightInfo for () {
	fn open_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn accept_open_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn close_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn close_inbound_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn suspend_inbound_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn resume_inbound_channel() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(6, 6)
	}
	fn set_hrmp_closing() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(1, 1)
	}
	fn clear_hrmp_closing() -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().writes(1)
	}
	fn enact_messages(c: u32, n: u32, _b: u32, r: u32) -> Weight {
		let db = frame_support::weights::constants::RocksDbWeight::get();
		db.reads_writes(1, 1)
			.saturating_add(
				db.reads_writes(6, 6).saturating_mul((c as u64).saturating_add(n as u64)),
			)
			.saturating_add(db.reads_writes(3, 3).saturating_mul(r as u64))
	}
	fn send(_b: u32) -> Weight {
		frame_support::weights::constants::RocksDbWeight::get().reads_writes(5, 4)
	}
	fn drain(h: u32) -> Weight {
		frame_support::weights::constants::RocksDbWeight::get()
			.reads_writes(3, 3)
			.saturating_add(
				frame_support::weights::constants::RocksDbWeight::get()
					.reads_writes(2, 2)
					.saturating_mul(h as u64),
			)
	}
	fn commit_streams_root(s: u32, h: u32) -> Weight {
		frame_support::weights::constants::RocksDbWeight::get()
			.reads_writes(2, 2)
			.saturating_add(
				frame_support::weights::constants::RocksDbWeight::get()
					.reads(1)
					.saturating_mul((s as u64).saturating_add(h as u64)),
			)
	}
}
