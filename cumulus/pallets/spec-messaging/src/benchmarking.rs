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

//! Benchmarks for the spec-messaging pallet. Each one sets up its worst case: new streams, full
//! frontiers, XCM-channel payloads (which the data handler enqueues), one hash per stream.

use super::*;
use alloc::vec;
use cumulus_primitives_spec_messaging::mmr::SpecMerge;
use frame_benchmarking::v2::*;
use frame_support::traits::{EnsureOrigin, Hooks};
use frame_system::RawOrigin;

/// A sibling distinct from this chain, by index.
fn sibling<T: Config>(index: u32) -> ParaId {
	ParaId::from(u32::from(T::SelfParaId::get()).saturating_add(1).saturating_add(index))
}

/// The largest frontier: 48 peaks, so frontier reads and the PoV are at their worst.
fn full_frontier() -> MmrFrontier {
	const LEAVES: u64 = (1 << 48) - 1;
	MmrFrontier::from_parts(vec![Hash::repeat_byte(1); LEAVES.count_ones() as usize], LEAVES)
		.expect("one peak per set bit, below the leaf cap; qed")
}

/// An outbound XCM channel to `peer`, `Open` under the runtime's grant with nothing in flight.
fn open_outbound<T: Config>(peer: ParaId) {
	OutChannels::<T>::insert(
		xcm_channel(peer),
		OutChannelState {
			closed_by_us: false,
			announced_version: PROTOCOL_VERSION,
			register: Some(Register {
				version: PROTOCOL_VERSION,
				up_to: MessagePosition(0),
				grant: T::DefaultWindowGrant::get(),
				closed: false,
			}),
		},
	);
}

/// Accept the inbound XCM channel from `sender` through the call, as a chain would.
fn accept_inbound<T: Config>(sender: ParaId) -> Result<(), BenchmarkError> {
	let origin =
		T::AcceptChannelOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
	Pallet::<T>::accept_open_channel(origin, sender, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM)
		.map_err(|_| BenchmarkError::Stop("accept failed"))
}

/// A distinct outbound stream per index, for the fold and drain benchmarks.
fn stream(index: u32) -> StreamId {
	StreamId::Channel { recipient: ParaId::from(index), domain: 0, num: 0 }
}

/// Queue one send on `stream` directly: payload, leaf hash and the block's send count.
fn queue_send<T: Config>(stream: StreamId) {
	let payload: BoundedVec<u8, T::MaxMsgLen> = vec![0u8].try_into().expect("1 byte fits; qed");
	let hash = leaf_hash(LEAF_VERSION, &payload);
	OutboundMessages::<T>::insert(stream, BoundedVec::truncate_from(vec![payload]));
	OutboundLeafHashes::<T>::insert(stream, BoundedVec::truncate_from(vec![hash]));
	SendsThisBlock::<T>::mutate(|sends| sends.saturating_inc());
	// The queued send is this block's: its roll-over has run. Start at block 1, which the
	// harness moves a block-0 setup to before measuring.
	let now = frame_system::Pallet::<T>::block_number().max(1u32.into());
	frame_system::Pallet::<T>::set_block_number(now);
	RolledOver::<T>::put((now, 0));
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn open_channel() -> Result<(), BenchmarkError> {
		let origin = T::OpenChannelOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(OutChannels::<T>::contains_key(xcm_channel(peer)));
		Ok(())
	}

	#[benchmark]
	fn accept_open_channel() -> Result<(), BenchmarkError> {
		let origin = T::AcceptChannelOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(InChannels::<T>::contains_key(xcm_channel(peer)));
		Ok(())
	}

	#[benchmark]
	fn close_channel() -> Result<(), BenchmarkError> {
		let origin = T::OpenChannelOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		open_outbound::<T>(peer);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(OutChannels::<T>::get(xcm_channel(peer)).is_some_and(|state| state.closed_by_us));
		Ok(())
	}

	#[benchmark]
	fn close_inbound_channel() -> Result<(), BenchmarkError> {
		let origin = T::AcceptChannelOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		accept_inbound::<T>(peer)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(InChannels::<T>::get(xcm_channel(peer)).is_some_and(|s| s.published.closed));
		Ok(())
	}

	#[benchmark]
	fn suspend_inbound_channel() -> Result<(), BenchmarkError> {
		let origin = T::ChannelManagementOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		accept_inbound::<T>(peer)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(InChannels::<T>::get(xcm_channel(peer)).is_some_and(|s| s.suspended));
		Ok(())
	}

	#[benchmark]
	fn resume_inbound_channel() -> Result<(), BenchmarkError> {
		let origin = T::ChannelManagementOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		accept_inbound::<T>(peer)?;
		Pallet::<T>::suspend_inbound_channel(
			origin.clone(),
			peer,
			XCM_CHANNEL_DOMAIN,
			XCM_CHANNEL_NUM,
		)
		.map_err(|_| BenchmarkError::Stop("suspend failed"))?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer, XCM_CHANNEL_DOMAIN, XCM_CHANNEL_NUM);

		assert!(InChannels::<T>::get(xcm_channel(peer)).is_some_and(|s| !s.suspended));
		Ok(())
	}

	#[benchmark]
	fn set_hrmp_closing() -> Result<(), BenchmarkError> {
		let origin = T::ChannelManagementOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		open_outbound::<T>(peer);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer);

		assert!(HrmpClosing::<T>::contains_key(peer));
		Ok(())
	}

	#[benchmark]
	fn clear_hrmp_closing() -> Result<(), BenchmarkError> {
		let origin = T::ChannelManagementOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let peer = sibling::<T>(0);
		HrmpClosing::<T>::insert(peer, ());

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, peer);

		assert!(!HrmpClosing::<T>::contains_key(peer));
		Ok(())
	}

	/// Skip an inbound XCM channel from 4,095 to 8,191 leaves (12 peaks to 13), with the
	/// consumption record one stream below [`Config::MaxTouchedStreams`]. The extension proof is
	/// real; verifying it costs O(log n) hashes, so a larger MMR changes little.
	#[benchmark]
	fn skip_inbound_stream() -> Result<(), BenchmarkError> {
		use mmr_lib::{
			leaf_index_to_mmr_size,
			util::{MemMMR, MemStore},
		};
		const FROM: u64 = (1 << 12) - 1;
		const TO: u64 = (1 << 13) - 1;

		let origin = T::ChannelManagementOrigin::try_successful_origin()
			.map_err(|_| BenchmarkError::Weightless)?;
		let sender = sibling::<T>(0);
		accept_inbound::<T>(sender)?;

		let store = MemStore::default();
		let mut mmr = MemMMR::<_, SpecMerge>::new(0, &store);
		let (mut old, mut new) = (MmrFrontier::new(), MmrFrontier::new());
		for i in 0..TO {
			let leaf = Hash::from_low_u64_be(i + 1);
			mmr.push(leaf).map_err(|_| BenchmarkError::Stop("mmr push failed"))?;
			if i < FROM {
				old.append(leaf);
			}
			new.append(leaf);
		}
		let proof = mmr
			.gen_ancestry_proof(leaf_index_to_mmr_size(FROM - 1))
			.map_err(|_| BenchmarkError::Stop("ancestry proof failed"))?;
		let extension = MMRExtensionProof {
			leaf_count: TO,
			connecting_nodes: proof
				.prev_peaks_proof
				.proof_items()
				.iter()
				.map(|(_, h)| *h)
				.collect(),
		};
		InboundFrontier::<T>::insert((sender, xcm_channel_stream::<T>()), old);

		// Other streams already consumed this block, each with a full frontier.
		for i in 1..T::MaxTouchedStreams::get() {
			let interval = Interval { start: MmrFrontier::new().root(), end: full_frontier() };
			ConsumptionOutbox::<T>::append((sibling::<T>(i), stream(i), interval));
		}

		#[extrinsic_call]
		_(
			origin as T::RuntimeOrigin,
			sender,
			XCM_CHANNEL_DOMAIN,
			XCM_CHANNEL_NUM,
			new.peaks().to_vec(),
			TO,
			extension,
		);

		assert_eq!(InboundFrontier::<T>::get((sender, xcm_channel_stream::<T>())), new);
		Ok(())
	}

	/// `c` accepted XCM channels consumed, the first with `n` more payloads and a `b`-byte one;
	/// `r` register reads on outbound channels.
	#[benchmark]
	fn enact_messages(
		c: Linear<1, { T::MaxTouchedStreams::get().saturating_sub(T::MaxContextGaps::get()) }>,
		n: Linear<0, 1000>,
		b: Linear<0, { T::MaxMsgLen::get().saturating_sub(8) }>,
		r: Linear<0, { T::MaxContextGaps::get() }>,
	) -> Result<(), BenchmarkError> {
		let mut items = Vec::new();
		for i in 0..c {
			let sender = sibling::<T>(i);
			accept_inbound::<T>(sender)?;
			let mut payloads = vec![SpecMsgKind::Data(Vec::new()).encode()];
			if i == 0 {
				payloads[0] = SpecMsgKind::Data(vec![0u8; b as usize]).encode();
				payloads.extend((0..n).map(|_| SpecMsgKind::Data(Vec::new()).encode()));
			}
			items.push((sender, xcm_channel_stream::<T>(), ConsumeItem::Channel { payloads }));
		}
		for j in 0..r {
			let peer = sibling::<T>(10_000 + j);
			open_outbound::<T>(peer);
			let register = Register {
				version: PROTOCOL_VERSION,
				up_to: MessagePosition(0),
				grant: T::DefaultWindowGrant::get(),
				closed: false,
			};
			items.push((
				peer,
				StreamId::Ack {
					recipient: T::SelfParaId::get(),
					domain: XCM_CHANNEL_DOMAIN,
					num: XCM_CHANNEL_NUM,
				},
				ConsumeItem::Events {
					base: MessagePosition(0),
					start_peaks: Vec::new(),
					payloads: vec![register.encode()],
				},
			));
		}
		// A new block: the setup's own register publishes leave the send budget.
		Pallet::<T>::on_initialize(frame_system::Pallet::<T>::block_number());
		let data = MessagingInherentData { items };

		#[extrinsic_call]
		_(RawOrigin::None, data);

		assert_eq!(ConsumptionOutbox::<T>::get().len() as u32, c + r);
		Ok(())
	}

	/// One send of `b` bytes on a new stream.
	#[benchmark]
	fn send(b: Linear<0, { T::MaxMsgLen::get() }>) {
		let payload = vec![0u8; b as usize];

		#[block]
		{
			Pallet::<T>::append_to_stream(stream(0), payload).expect("a first send fits; qed");
		}

		assert_eq!(StreamCount::<T>::get(), 1);
	}

	/// `on_initialize` draining `h` leaf hashes, each in its own stream with a full frontier.
	#[benchmark]
	fn drain(h: Linear<0, { T::MaxSendsPerBlock::get() }>) {
		for i in 0..h {
			OutboundFrontier::<T>::insert(stream(i), full_frontier());
			queue_send::<T>(stream(i));
		}

		// The sends were the previous block's.
		let next = frame_system::Pallet::<T>::block_number() + 1u32.into();
		frame_system::Pallet::<T>::set_block_number(next);

		#[block]
		{
			Pallet::<T>::on_initialize(next);
		}

		assert!(OutboundLeafHashes::<T>::iter().next().is_none());
	}

	/// The end-of-block fold: `s` untouched streams and `h` touched ones (one hash each), all with
	/// full frontiers.
	#[benchmark]
	fn commit_streams_root(
		s: Linear<0, { T::MaxStreams::get() }>,
		h: Linear<0, { T::MaxSendsPerBlock::get() }>,
	) {
		for i in 0..s.saturating_add(h) {
			OutboundFrontier::<T>::insert(stream(i), full_frontier());
		}
		for i in s..s.saturating_add(h) {
			queue_send::<T>(stream(i));
		}

		#[block]
		{
			Pallet::<T>::commit_streams_root();
		}

		assert_eq!(BlockStreamsRoot::<T>::get().is_some(), h > 0);
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}

/// The inbound XCM channel's data stream: the peer's key space, addressed to us.
fn xcm_channel_stream<T: Config>() -> StreamId {
	StreamId::Channel {
		recipient: T::SelfParaId::get(),
		domain: XCM_CHANNEL_DOMAIN,
		num: XCM_CHANNEL_NUM,
	}
}
