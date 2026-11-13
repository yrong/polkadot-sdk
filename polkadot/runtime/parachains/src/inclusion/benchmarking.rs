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

use bitvec::{bitvec, prelude::Lsb0};
use codec::Encode;
use frame_benchmarking::v2::*;
use pallet_message_queue as mq;
use polkadot_primitives::{
	v9::MAX_COMMITMENT_ENTRIES, CandidateCommitments,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, Hash, HrmpChannelId,
	OutboundHrmpMessage, RequiresSet, SessionIndex, StreamsRoot, UMPSignal, UMP_SEPARATOR,
};

use super::*;
use crate::{
	builder::generate_validator_pairs,
	configuration,
	hrmp::{HrmpChannel, HrmpChannels},
	initializer, HeadData, ValidationCode,
};

/// A distinct `StreamsRoot` per `n`. `H256::from_low_u64_be` is std-only, and benchmarks build for
/// the wasm runtime too.
fn root(n: u64) -> StreamsRoot {
	let mut bytes = [0u8; 32];
	bytes[24..].copy_from_slice(&n.to_be_bytes());
	StreamsRoot(Hash::from(bytes))
}

fn create_candidate_commitments<T: crate::hrmp::pallet::Config>(
	para_id: ParaId,
	head_data: HeadData,
	max_msg_len: usize,
	ump_msg_count: u32,
	hrmp_msg_count: u32,
	code_upgrade: bool,
) -> CandidateCommitments {
	let upward_messages = {
		let unbounded = create_messages(max_msg_len, ump_msg_count as _);
		BoundedVec::truncate_from(unbounded)
	};

	let horizontal_messages = {
		let unbounded = create_messages(max_msg_len, hrmp_msg_count as _);

		for n in 0..unbounded.len() {
			let channel_id = HrmpChannelId { sender: para_id, recipient: para_id + n as u32 + 1 };
			HrmpChannels::<T>::insert(
				&channel_id,
				HrmpChannel {
					sender_deposit: 42,
					recipient_deposit: 42,
					max_capacity: 10_000_000,
					max_total_size: 1_000_000_000,
					max_message_size: 10_000_000,
					msg_count: 0,
					total_size: 0,
					mqc_head: None,
				},
			);
		}

		let unbounded = unbounded
			.into_iter()
			.enumerate()
			.map(|(n, data)| OutboundHrmpMessage { recipient: para_id + n as u32 + 1, data })
			.collect();
		BoundedVec::truncate_from(unbounded)
	};

	let new_validation_code = code_upgrade.then_some(ValidationCode(vec![42_u8; 1024]));

	CandidateCommitments::<u32> {
		upward_messages,
		horizontal_messages,
		new_validation_code,
		head_data,
		processed_downward_messages: 0,
		hrmp_watermark: 10,
	}
}

fn create_messages(msg_len: usize, n_msgs: usize) -> Vec<Vec<u8>> {
	let best_number = 73_u8; // Chuck Norris of numbers
	vec![vec![best_number; msg_len]; n_msgs]
}

fn prepare_selection<T: Config>(c: u32, e: u32, b: u32) -> Vec<ValidatorId> {
	let validators = generate_validator_pairs::<T>(500)
		.into_iter()
		.map(|(_, validator)| validator)
		.collect::<Vec<_>>();
	let n = e.div_ceil(MAX_COMMITMENT_ENTRIES - 2).max(1);
	for i in 0..n + c {
		let next = if i < n { (i + 1) % n } else { 0 };
		let mut requirements = vec![(next.into(), root(next as u64))];
		let entries = if i < n {
			e.saturating_sub(i * (MAX_COMMITMENT_ENTRIES - 2))
				.min(MAX_COMMITMENT_ENTRIES - 2)
		} else {
			0
		};
		for j in 0..entries {
			let source = ParaId::from(10000 + i * MAX_COMMITMENT_ENTRIES + j);
			let required = root(u64::from(i) * u64::from(MAX_COMMITMENT_ENTRIES) + u64::from(j));
			{
				let window: BoundedVec<_, ConstU32<MAX_PROVIDES_WINDOW_SIZE>> =
					core::iter::once(required)
						.chain((1..MAX_PROVIDES_WINDOW_SIZE).map(|n| root(u64::MAX - n as u64)))
						.collect::<Vec<_>>()
						.try_into()
						.expect("exact window bound; qed");
				RecentProvides::<T>::insert(source, window);
			}
			requirements.push((source, required));
		}
		if i == 0 {
			requirements.push((u32::MAX.into(), root(u64::MAX)));
		}
		let para = ParaId::from(i);
		let descriptor = CandidateDescriptor::<T::Hash>::new(
			para,
			Default::default(),
			CoreIndex(i),
			1,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
			ValidationCode(vec![]).hash(),
		);
		let mut commitments = CandidateCommitments {
			upward_messages: Default::default(),
			horizontal_messages: Default::default(),
			new_validation_code: None,
			head_data: HeadData(vec![]),
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		commitments.upward_messages.force_push(UMP_SEPARATOR);
		commitments
			.upward_messages
			.force_push(UMPSignal::Provides(root(i as u64)).encode());
		commitments.upward_messages.force_push(
			UMPSignal::Requires(
				RequiresSet::try_from_iter(requirements).expect("sorted and bounded; qed"),
			)
			.encode(),
		);
		// Vary total payload bytes independently of candidate and requirement counts.
		if i == 0 {
			commitments.new_validation_code = Some(ValidationCode(vec![0; b as usize]));
		}
		PendingAvailability::<T>::insert(
			para,
			VecDeque::from([CandidatePendingAvailability::new(
				CoreIndex(i),
				Default::default(),
				descriptor,
				commitments,
				bitvec![u8, Lsb0; 1; validators.len()],
				bitvec![u8, Lsb0; 0; validators.len()],
				0u32.into(),
				1u32.into(),
				GroupIndex(0),
			)]),
		);
	}
	validators
}

fn run_selection<T: Config>(validators: &[ValidatorId]) {
	// Declaration and reservation both scan pending storage before selection.
	core::hint::black_box(Pallet::<T>::pending_selection_weight());
	core::hint::black_box(Pallet::<T>::pending_selection_weight());
	let (_, freed) =
		Pallet::<T>::update_pending_availability_and_get_freed_cores(validators, vec![]);
	assert!(freed.is_empty());
}

#[benchmarks(where T: mq::Config + configuration::Config + initializer::Config)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn enact_candidate(u: Linear<0, 2>, h: Linear<0, 2>, c: Linear<0, 1>, p: Linear<0, 1>) {
		let para = 42_u32.into(); // not especially important.

		let max_len = mq::MaxMessageLenOf::<T>::get() as usize;

		let config = configuration::ActiveConfig::<T>::get();
		let n_validators = config.max_validators.unwrap_or(500);
		let validators = generate_validator_pairs::<T>(n_validators);

		let session = SessionIndex::from(0_u32);
		initializer::Pallet::<T>::test_trigger_on_new_session(
			false,
			session,
			validators.iter().map(|(a, v)| (a, v.clone())),
			None,
		);
		let backing_group_size = config.scheduler_params.max_validators_per_core.unwrap_or(5);
		let head_data = HeadData(vec![0xFF; 1024]);

		let relay_parent_number = BlockNumberFor::<T>::from(10_u32);
		let mut commitments =
			create_candidate_commitments::<T>(para, head_data, max_len, u, h, c != 0);

		// With `p`, carry a `Provides` into a full window, so the record reads the whole window,
		// drops the oldest root and writes it back.
		if p != 0 {
			commitments.upward_messages.force_push(UMP_SEPARATOR);
			commitments
				.upward_messages
				.force_push(UMPSignal::Provides(root(u64::MAX)).encode());
			for i in 0..MAX_PROVIDES_WINDOW_SIZE {
				Pallet::<T>::record_provides(para, root(i as u64));
			}
		}
		let backers = bitvec![u8, Lsb0; 1; backing_group_size as usize];
		let availability_votes = bitvec![u8, Lsb0; 1; n_validators as usize];
		let core_index = CoreIndex::from(0);
		let backing_group = GroupIndex::from(0);

		let descriptor = CandidateDescriptor::<T::Hash>::new(
			para,
			Default::default(),
			CoreIndex(0),
			1,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
			ValidationCode(vec![1, 2, 3]).hash(),
		);

		let receipt = CommittedCandidateReceipt::<T::Hash> { descriptor, commitments };

		Pallet::<T>::receive_upward_messages(para, &vec![vec![0; max_len]; 1]);

		#[block]
		{
			Pallet::<T>::enact_candidate(
				relay_parent_number,
				receipt,
				backers,
				availability_votes,
				core_index,
				backing_group,
			);
		}
	}

	#[benchmark]
	fn record_provides() {
		let para = ParaId::from(42);
		for i in 0..MAX_PROVIDES_WINDOW_SIZE {
			Pallet::<T>::record_provides(para, root(i as u64));
		}
		#[block]
		{
			Pallet::<T>::record_provides(para, root(u64::MAX));
		}
		assert_eq!(Pallet::<T>::newest_provides(para), Some(root(u64::MAX)));
	}

	#[benchmark]
	fn enactment_selection(c: Linear<0, 1000>) {
		let validators = prepare_selection::<T>(c, 0, 0);
		#[block]
		{
			run_selection::<T>(&validators);
		}
		assert_eq!(Pallet::<T>::get_occupied_cores().count(), (c + 1) as usize);
	}

	#[benchmark]
	fn enactment_selection_requirements(e: Linear<0, 256000>) {
		// Extra queues conservatively cover requirement decoding and graph construction.
		let validators = prepare_selection::<T>(0, e, 0);
		#[block]
		{
			run_selection::<T>(&validators);
		}
		assert_eq!(
			Pallet::<T>::get_occupied_cores().count(),
			e.div_ceil(MAX_COMMITMENT_ENTRIES - 2).max(1) as usize
		);
	}

	/// Isolate payload copying from the much larger dependency graph workload.
	#[benchmark]
	fn enactment_selection_payload(b: Linear<0, { polkadot_primitives::MAX_CODE_SIZE }>) {
		let validators = prepare_selection::<T>(0, 0, b);
		#[block]
		{
			run_selection::<T>(&validators);
		}
		assert_eq!(Pallet::<T>::get_occupied_cores().count(), 1);
	}

	#[test]
	fn selection_storage_path_smoke() {
		use frame_benchmarking::{BenchmarkParameter, BenchmarkingSetup};
		for (c, e) in [(0, 0), (3, 4), (2, 512)] {
			crate::mock::new_test_ext(Default::default()).execute_with(|| {
				<enactment_selection as BenchmarkingSetup<crate::mock::Test>>::test_instance(
					&enactment_selection,
					&[(BenchmarkParameter::c, c)],
					&|| {},
				)
				.expect("selection benchmark verifies; qed");
			});
			crate::mock::new_test_ext(Default::default()).execute_with(|| {
				<enactment_selection_requirements as BenchmarkingSetup<crate::mock::Test>>::test_instance(
					&enactment_selection_requirements, &[(BenchmarkParameter::e, e)], &|| {},
				).expect("requirements benchmark verifies; qed");
			});
		}
		crate::mock::new_test_ext(Default::default()).execute_with(|| {
			<enactment_selection_payload as BenchmarkingSetup<crate::mock::Test>>::test_instance(
				&enactment_selection_payload,
				&[(BenchmarkParameter::b, polkadot_primitives::MAX_CODE_SIZE)],
				&|| {},
			)
			.expect("payload benchmark verifies; qed");
		});
	}

	impl_benchmark_test_suite! {
		Pallet,
		crate::mock::new_test_ext(Default::default()),
		crate::mock::Test
	}
}
