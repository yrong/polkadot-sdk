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

//! Tests for the backing subsystem with enabled prospective parachains.

use polkadot_node_subsystem::{
	messages::{ChainApiMessage, HypotheticalMembership},
	ActivatedLeaf, TimeoutExt,
};
use polkadot_primitives::{vstaging::OccupiedCore, AsyncBackingParams, BlockNumber, Header};

use super::*;

const ASYNC_BACKING_PARAMETERS: AsyncBackingParams =
	AsyncBackingParams { max_candidate_depth: 4, allowed_ancestry_len: 3 };

struct TestLeaf {
	activated: ActivatedLeaf,
	min_relay_parents: Vec<(ParaId, u32)>,
}

fn get_parent_hash(hash: Hash) -> Hash {
	Hash::from_low_u64_be(hash.to_low_u64_be() + 1)
}

async fn activate_leaf(
	virtual_overseer: &mut VirtualOverseer,
	leaf: TestLeaf,
	test_state: &mut TestState,
) {
	let TestLeaf { activated, min_relay_parents } = leaf;
	let leaf_hash = activated.hash;
	let leaf_number = activated.number;
	// Start work on some new parent.
	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			activated,
		))))
		.await;

	// Prospective parachains mode is temporarily defined by the Runtime API version.
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::AsyncBackingParams(tx))
		) if parent == leaf_hash => {
			tx.send(Ok(ASYNC_BACKING_PARAMETERS)).unwrap();
		}
	);

	let min_min = *min_relay_parents
		.iter()
		.map(|(_, block_num)| block_num)
		.min()
		.unwrap_or(&leaf_number);

	let ancestry_len = leaf_number + 1 - min_min;

	let ancestry_hashes = std::iter::successors(Some(leaf_hash), |h| Some(get_parent_hash(*h)))
		.take(ancestry_len as usize);
	let ancestry_numbers = (min_min..=leaf_number).rev();
	let ancestry_iter = ancestry_hashes.zip(ancestry_numbers).peekable();

	let mut next_overseer_message = None;
	// How many blocks were actually requested.
	let mut requested_len = 0;
	{
		let mut ancestry_iter = ancestry_iter.clone();
		while let Some((hash, number)) = ancestry_iter.next() {
			// May be `None` for the last element.
			let parent_hash =
				ancestry_iter.peek().map(|(h, _)| *h).unwrap_or_else(|| get_parent_hash(hash));

			let msg = virtual_overseer.recv().await;
			// It may happen that some blocks were cached by implicit view,
			// reuse the message.
			if !matches!(&msg, AllMessages::ChainApi(ChainApiMessage::BlockHeader(..))) {
				next_overseer_message.replace(msg);
				break
			}

			assert_matches!(
				msg,
				AllMessages::ChainApi(
					ChainApiMessage::BlockHeader(_hash, tx)
				) if _hash == hash => {
					let header = Header {
						parent_hash,
						number,
						state_root: Hash::zero(),
						extrinsics_root: Hash::zero(),
						digest: Default::default(),
					};

					tx.send(Ok(Some(header))).unwrap();
				}
			);

			if requested_len == 0 {
				assert_matches!(
					virtual_overseer.recv().await,
					AllMessages::ProspectiveParachains(
						ProspectiveParachainsMessage::GetMinimumRelayParents(parent, tx)
					) if parent == leaf_hash => {
						tx.send(min_relay_parents.clone()).unwrap();
					}
				);
			}

			requested_len += 1;
		}
	}

	for (hash, number) in ancestry_iter.take(requested_len) {
		let msg = match next_overseer_message.take() {
			Some(msg) => msg,
			None => virtual_overseer.recv().await,
		};

		// Check that subsystem job issues a request for the session index for child.
		assert_matches!(
			msg,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
			) if parent == hash => {
				tx.send(Ok(test_state.signing_context.session_index)).unwrap();
			}
		);

		// Check that subsystem job issues a request for the validator groups.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ValidatorGroups(tx))
			) if parent == hash => {
				let (validator_groups, mut group_rotation_info) = test_state.validator_groups.clone();
				group_rotation_info.now = number;
				tx.send(Ok((validator_groups, group_rotation_info))).unwrap();
			}
		);

		// Check that subsystem job issues a request for the availability cores.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::AvailabilityCores(tx))
			) if parent == hash => {
				tx.send(Ok(test_state.availability_cores.clone())).unwrap();
			}
		);

		if !test_state.per_session_cache_state.has_cached_validators {
			// Check that subsystem job issues a request for a validator set.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::Validators(tx))
				) if parent == hash => {
					tx.send(Ok(test_state.validator_public.clone())).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_validators = true;
		}

		if !test_state.per_session_cache_state.has_cached_node_features {
			// Node features request from runtime: all features are disabled.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::NodeFeatures(_session_index, tx))
				) if parent == hash => {
					tx.send(Ok(Default::default())).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_node_features = true;
		}

		if !test_state.per_session_cache_state.has_cached_executor_params {
			// Check if subsystem job issues a request for the executor parameters.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionExecutorParams(_session_index, tx))
				) if parent == hash => {
					tx.send(Ok(Some(ExecutorParams::default()))).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_executor_params = true;
		}

		if !test_state.per_session_cache_state.has_cached_minimum_backing_votes {
			// Check if subsystem job issues a request for the minimum backing votes.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					parent,
					RuntimeApiRequest::MinimumBackingVotes(session_index, tx),
				)) if parent == hash && session_index == test_state.signing_context.session_index => {
					tx.send(Ok(test_state.minimum_backing_votes)).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_minimum_backing_votes = true;
		}

		// Check that subsystem job issues a request for the runtime version.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::Version(tx))
			) if parent == hash => {
				tx.send(Ok(RuntimeApiRequest::DISABLED_VALIDATORS_RUNTIME_REQUIREMENT)).unwrap();
			}
		);

		// Check that the subsystem job issues a request for the disabled validators.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::DisabledValidators(tx))
			) if parent == hash => {
				tx.send(Ok(Vec::new())).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::Version(tx))
			) if parent == hash => {
				tx.send(Ok(RuntimeApiRequest::CLAIM_QUEUE_RUNTIME_REQUIREMENT)).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ClaimQueue(tx))
			) if parent == hash => {
				tx.send(Ok(
					test_state.claim_queue.clone()
				)).unwrap();
			}
		);
	}
}

async fn assert_validate_seconded_candidate(
	virtual_overseer: &mut VirtualOverseer,
	relay_parent: Hash,
	candidate: &CommittedCandidateReceipt,
	assert_pov: &PoV,
	assert_pvd: &PersistedValidationData,
	assert_validation_code: &ValidationCode,
	expected_head_data: &HeadData,
	fetch_pov: bool,
) {
	assert_validation_requests(virtual_overseer, assert_validation_code.clone()).await;

	if fetch_pov {
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent: hash,
					tx,
					..
				}
			) if hash == relay_parent => {
				tx.send(assert_pov.clone()).unwrap();
			}
		);
	}

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::CandidateValidation(CandidateValidationMessage::ValidateFromExhaustive {
			validation_data,
			validation_code,
			candidate_receipt,
			pov,
			exec_kind,
			response_sender,
			..
		}) if &validation_data == assert_pvd &&
			&validation_code == assert_validation_code &&
			&*pov == assert_pov &&
			candidate_receipt.descriptor == candidate.descriptor &&
			matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
			candidate.commitments.hash() == candidate_receipt.commitments_hash =>
		{
			response_sender.send(Ok(ValidationResult::Valid(
				CandidateCommitments {
					head_data: expected_head_data.clone(),
					horizontal_messages: Default::default(),
					upward_messages: Default::default(),
					new_validation_code: None,
					processed_downward_messages: 0,
					hrmp_watermark: 0,
				},
				assert_pvd.clone(),
			)))
			.unwrap();
		}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::AvailabilityStore(
			AvailabilityStoreMessage::StoreAvailableData { candidate_hash, tx, .. }
		) if candidate_hash == candidate.hash() => {
			tx.send(Ok(())).unwrap();
		}
	);
}

async fn assert_hypothetical_membership_requests(
	virtual_overseer: &mut VirtualOverseer,
	mut expected_requests: Vec<(
		HypotheticalMembershipRequest,
		Vec<(HypotheticalCandidate, HypotheticalMembership)>,
	)>,
) {
	// Requests come with no particular order.
	let requests_num = expected_requests.len();

	for _ in 0..requests_num {
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::GetHypotheticalMembership(request, tx),
			) => {
				let idx = match expected_requests.iter().position(|r| r.0 == request) {
					Some(idx) => idx,
					None =>
						panic!(
						"unexpected hypothetical membership request, no match found for {:?}",
						request
						),
				};
				let resp = std::mem::take(&mut expected_requests[idx].1);
				tx.send(resp).unwrap();

				expected_requests.remove(idx);
			}
		);
	}
}

fn make_hypothetical_membership_response(
	hypothetical_candidate: HypotheticalCandidate,
	relay_parent_hash: Hash,
) -> Vec<(HypotheticalCandidate, HypotheticalMembership)> {
	vec![(hypothetical_candidate, vec![relay_parent_hash])]
}

// Test that `seconding_sanity_check` works when a candidate is allowed
// for all leaves.
#[test]
fn seconding_sanity_check_allowed_on_all() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let leaf_b_hash = Hash::from_low_u64_be(128);
		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;
		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate.clone(), leaf_a_hash);
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		let expected_response_b =
			make_hypothetical_membership_response(hypothetical_candidate, leaf_b_hash);
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_response_a),
				(expected_request_b, expected_response_b),
			],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(
					parent_hash,
					_signed_statement,
				)
			) if parent_hash == leaf_a_parent => {}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
				assert_eq!(leaf_a_parent, hash);
				assert_matches!(statement.payload(), Statement::Seconded(_));
			}
		);

		virtual_overseer
	});
}

// Test that `seconding_sanity_check` disallows seconding when a candidate is disallowed
// for all leaves.
#[test]
fn seconding_sanity_check_disallowed() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_b_hash = Hash::from_low_u64_be(128);
		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash);
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![(expected_request_a, expected_response_a)],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(
					parent_hash,
					_signed_statement,
				)
			) if parent_hash == leaf_a_parent => {}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
				assert_eq!(leaf_a_parent, hash);
				assert_matches!(statement.payload(), Statement::Seconded(_));
			}
		);

		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;
		let leaf_a_grandparent = get_parent_hash(leaf_a_parent);
		let expected_head_data = test_state.head_data.get(&para_id).unwrap();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_grandparent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_grandparent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`

		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate),
			persisted_validation_data: pvd,
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_empty_response = vec![(hypothetical_candidate.clone(), vec![])];
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_empty_response.clone()),
				(expected_request_b, expected_empty_response),
			],
		)
		.await;

		assert!(virtual_overseer
			.recv()
			.timeout(std::time::Duration::from_millis(50))
			.await
			.is_none());

		virtual_overseer
	});
}

// Test that `seconding_sanity_check` allows seconding a candidate when it's allowed on at least one
// leaf.
#[test]
fn seconding_sanity_check_allowed_on_at_least_one_leaf() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let leaf_b_hash = Hash::from_low_u64_be(128);
		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;
		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate.clone(), leaf_a_hash);
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		let expected_response_b = vec![(hypothetical_candidate.clone(), vec![])];
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_response_a),
				(expected_request_b, expected_response_b),
			],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(
					parent_hash,
					_signed_statement,
				)
			) if parent_hash == leaf_a_parent => {}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
				assert_eq!(leaf_a_parent, hash);
				assert_matches!(statement.payload(), Statement::Seconded(_));
			}
		);

		virtual_overseer
	});
}

// Test that a seconded candidate which is not approved by prospective parachains
// subsystem doesn't change the view.
#[test]
fn prospective_parachains_reject_candidate() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = vec![(
			HypotheticalMembershipRequest {
				candidates: vec![hypothetical_candidate.clone()],
				fragment_chain_relay_parent: Some(leaf_a_hash),
			},
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash),
		)];
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request_a.clone())
			.await;

		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				// Reject it.
				tx.send(false).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(
				relay_parent,
				candidate_receipt,
			)) if candidate_receipt.descriptor == candidate.descriptor &&
				candidate_receipt.commitments_hash == candidate.commitments.hash() &&
				relay_parent == leaf_a_parent
		);

		// Try seconding the same candidate.

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request_a).await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(
					parent_hash,
					_signed_statement,
				)
			) if parent_hash == leaf_a_parent => {}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
				assert_eq!(leaf_a_parent, hash);
				assert_matches!(statement.payload(), Statement::Seconded(_));
			}
		);

		virtual_overseer
	});
}

// Test that a validator can second multiple candidates per single relay parent.
#[test]
fn second_multiple_candidates_per_relay_parent() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a parent of the activated `leaf`.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);
		let leaf_grandparent = get_parent_hash(leaf_parent);
		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		};
		let mut candidate_b = candidate_a.clone();
		candidate_b.relay_parent = leaf_grandparent;

		let candidate_a = candidate_a.build();
		let candidate_b = candidate_b.build();

		for candidate in &[candidate_a, candidate_b] {
			let second = CandidateBackingMessage::Second(
				leaf_hash,
				candidate.to_plain(),
				pvd.clone(),
				pov.clone(),
			);

			virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

			assert_validate_seconded_candidate(
				&mut virtual_overseer,
				candidate.descriptor.relay_parent(),
				&candidate,
				&pov,
				&pvd,
				&validation_code,
				expected_head_data,
				false,
			)
			.await;

			// `seconding_sanity_check`
			let hypothetical_candidate = HypotheticalCandidate::Complete {
				candidate_hash: candidate.hash(),
				receipt: Arc::new(candidate.clone()),
				persisted_validation_data: pvd.clone(),
			};
			let expected_request_a = vec![(
				HypotheticalMembershipRequest {
					candidates: vec![hypothetical_candidate.clone()],
					fragment_chain_relay_parent: Some(leaf_hash),
				},
				make_hypothetical_membership_response(hypothetical_candidate, leaf_hash),
			)];
			assert_hypothetical_membership_requests(
				&mut virtual_overseer,
				expected_request_a.clone(),
			)
			.await;

			// Prospective parachains are notified.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(
						req,
						tx,
					),
				) if
					&req.candidate_receipt == candidate
					&& req.candidate_para == para_id
					&& pvd == req.persisted_validation_data
				=> {
					tx.send(true).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						_signed_statement,
					)
				) if parent_hash == candidate.descriptor.relay_parent() => {}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
					assert_eq!(candidate.descriptor.relay_parent(), hash);
					assert_matches!(statement.payload(), Statement::Seconded(_));
				}
			);
		}

		virtual_overseer
	});
}

// Test that the candidate reaches quorum successfully.
#[test]
fn backing_works() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a parent of the activated `leaf`.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);
		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();

		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			validation_code: validation_code.0.clone(),
			persisted_validation_data_hash: pvd.hash(),
		}
		.build();

		let candidate_a_hash = candidate_a.hash();

		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		// Signing context should have a parent hash candidate is based on.
		let signing_context =
			SigningContext { parent_hash: leaf_parent, session_index: test_state.session() };
		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd.clone()),
			&signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement(leaf_parent, signed_a.clone());

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// Prospective parachains are notified about candidate seconded first.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate_a
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			candidate_a.descriptor.relay_parent(),
			&candidate_a,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			true,
		)
		.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(hash, _stmt)
			) => {
				assert_eq!(leaf_parent, hash);
			}
		);

		// Prospective parachains and collator protocol are notified about candidate backed.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::CandidateBacked(
					candidate_para_id, candidate_hash
				),
			) if candidate_a_hash == candidate_hash && candidate_para_id == para_id
		);
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(StatementDistributionMessage::Backed (
				candidate_hash
			)) if candidate_a_hash == candidate_hash
		);

		let statement = CandidateBackingMessage::Statement(leaf_parent, signed_b.clone());

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		virtual_overseer
	});
}

// Tests that validators start work on consecutive prospective parachain blocks.
#[test]
fn concurrent_dependent_candidates() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a grandparent of the activated `leaf`,
		// candidate `b` -- in parent.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);
		let leaf_grandparent = get_parent_hash(leaf_parent);
		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let head_data = &[
			HeadData(vec![10, 20, 30]), // Before `a`.
			HeadData(vec![11, 21, 31]), // After `a`.
			HeadData(vec![12, 22]),     // After `b`.
		];

		let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = PersistedValidationData {
			parent_head: head_data[0].clone(),
			relay_parent_number: LEAF_BLOCK_NUMBER - 2,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 1024,
		};

		let pov_b = PoV { block_data: BlockData(vec![22, 14, 100]) };
		let pvd_b = PersistedValidationData {
			parent_head: head_data[1].clone(),
			relay_parent_number: LEAF_BLOCK_NUMBER - 1,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 1024,
		};
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_grandparent,
			pov_hash: pov_a.hash(),
			head_data: head_data[1].clone(),
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			persisted_validation_data_hash: pvd_a.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		let candidate_b = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_parent,
			pov_hash: pov_b.hash(),
			head_data: head_data[2].clone(),
			erasure_root: make_erasure_root(&test_state, pov_b.clone(), pvd_b.clone()),
			persisted_validation_data_hash: pvd_b.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		let candidate_a_hash = candidate_a.hash();
		let candidate_b_hash = candidate_b.hash();

		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		// Signing context should have a parent hash candidate is based on.
		let signing_context =
			SigningContext { parent_hash: leaf_grandparent, session_index: test_state.session() };
		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd_a.clone()),
			&signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signing_context =
			SigningContext { parent_hash: leaf_parent, session_index: test_state.session() };
		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_b.clone(), pvd_b.clone()),
			&signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement_a = CandidateBackingMessage::Statement(leaf_grandparent, signed_a.clone());
		let statement_b = CandidateBackingMessage::Statement(leaf_parent, signed_b.clone());

		virtual_overseer.send(FromOrchestra::Communication { msg: statement_a }).await;

		// At this point the subsystem waits for response, the previous message is received,
		// send a second one without blocking.
		let _ = virtual_overseer
			.tx
			.start_send_unpin(FromOrchestra::Communication { msg: statement_b });

		let mut valid_statements = HashSet::new();
		let mut backed_statements = HashSet::new();

		loop {
			let msg = virtual_overseer
				.recv()
				.timeout(std::time::Duration::from_secs(1))
				.await
				.expect("overseer recv timed out");

			// Order is not guaranteed since we have 2 statements being handled concurrently.
			match msg {
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(_, tx),
				) => {
					tx.send(true).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::ValidationCodeByHash(_, tx),
				)) => {
					tx.send(Ok(Some(validation_code.clone()))).unwrap();
				},
				AllMessages::AvailabilityDistribution(
					AvailabilityDistributionMessage::FetchPoV { candidate_hash, tx, .. },
				) => {
					let pov = if candidate_hash == candidate_a_hash {
						&pov_a
					} else if candidate_hash == candidate_b_hash {
						&pov_b
					} else {
						panic!("unknown candidate hash")
					};
					tx.send(pov.clone()).unwrap();
				},
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromExhaustive {
						candidate_receipt,
						response_sender,
						..
					},
				) => {
					let candidate_hash = candidate_receipt.hash();
					let (head_data, pvd) = if candidate_hash == candidate_a_hash {
						(&head_data[1], &pvd_a)
					} else if candidate_hash == candidate_b_hash {
						(&head_data[2], &pvd_b)
					} else {
						panic!("unknown candidate hash")
					};
					response_sender
						.send(Ok(ValidationResult::Valid(
							CandidateCommitments {
								head_data: head_data.clone(),
								horizontal_messages: Default::default(),
								upward_messages: Default::default(),
								new_validation_code: None,
								processed_downward_messages: 0,
								hrmp_watermark: 0,
							},
							pvd.clone(),
						)))
						.unwrap();
				},
				AllMessages::AvailabilityStore(AvailabilityStoreMessage::StoreAvailableData {
					tx,
					..
				}) => {
					tx.send(Ok(())).unwrap();
				},
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::CandidateBacked(..),
				) => {},
				AllMessages::StatementDistribution(StatementDistributionMessage::Share(
					_,
					statement,
				)) => {
					assert_eq!(statement.validator_index(), ValidatorIndex(0));
					let payload = statement.payload();
					assert_matches!(
						payload.clone(),
						StatementWithPVD::Valid(hash)
							if hash == candidate_a_hash || hash == candidate_b_hash =>
						{
							assert!(valid_statements.insert(hash));
						}
					);
				},
				AllMessages::StatementDistribution(StatementDistributionMessage::Backed(hash)) => {
					// Ensure that `Share` was received first for the candidate.
					assert!(valid_statements.contains(&hash));
					backed_statements.insert(hash);

					if backed_statements.len() == 2 {
						break
					}
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_parent,
					RuntimeApiRequest::ValidatorGroups(tx),
				)) => {
					tx.send(Ok(test_state.validator_groups.clone())).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_parent,
					RuntimeApiRequest::AvailabilityCores(tx),
				)) => {
					tx.send(Ok(test_state.availability_cores.clone())).unwrap();
				},
				_ => panic!("unexpected message received from overseer: {:?}", msg),
			}
		}

		assert!(valid_statements.contains(&candidate_a_hash));
		assert!(valid_statements.contains(&candidate_b_hash));
		assert!(backed_statements.contains(&candidate_a_hash));
		assert!(backed_statements.contains(&candidate_b_hash));

		virtual_overseer
	});
}

// Test that multiple candidates from different paras can occupy the same depth
// in a given relay parent.
#[test]
fn seconding_sanity_check_occupy_same_depth() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a parent of the activated `leaf`.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;

		let para_id_a = test_state.chain_ids[0];
		let para_id_b = test_state.chain_ids[1];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);

		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_block_number = LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN;
		let min_relay_parents = vec![(para_id_a, min_block_number), (para_id_b, min_block_number)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data_a = test_state.head_data.get(&para_id_a).unwrap();
		let expected_head_data_b = test_state.head_data.get(&para_id_b).unwrap();

		let pov_hash = pov.hash();
		let candidate_a = TestCandidateBuilder {
			para_id: para_id_a,
			relay_parent: leaf_parent,
			pov_hash,
			head_data: expected_head_data_a.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		};

		let mut candidate_b = candidate_a.clone();
		candidate_b.para_id = para_id_b;
		candidate_b.head_data = expected_head_data_b.clone();
		// A rotation happens, test validator is assigned to second para here.
		candidate_b.relay_parent = leaf_hash;

		let candidate_a = (candidate_a.build(), expected_head_data_a, para_id_a);
		let candidate_b = (candidate_b.build(), expected_head_data_b, para_id_b);

		for candidate in &[candidate_a, candidate_b] {
			let (candidate, expected_head_data, para_id) = candidate;
			let second = CandidateBackingMessage::Second(
				leaf_hash,
				candidate.to_plain(),
				pvd.clone(),
				pov.clone(),
			);

			virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

			assert_validate_seconded_candidate(
				&mut virtual_overseer,
				candidate.descriptor.relay_parent(),
				&candidate,
				&pov,
				&pvd,
				&validation_code,
				expected_head_data,
				false,
			)
			.await;

			// `seconding_sanity_check`
			let hypothetical_candidate = HypotheticalCandidate::Complete {
				candidate_hash: candidate.hash(),
				receipt: Arc::new(candidate.clone()),
				persisted_validation_data: pvd.clone(),
			};
			let expected_request_a = vec![(
				HypotheticalMembershipRequest {
					candidates: vec![hypothetical_candidate.clone()],
					fragment_chain_relay_parent: Some(leaf_hash),
				},
				// Send the same membership for both candidates.
				make_hypothetical_membership_response(hypothetical_candidate, leaf_hash),
			)];

			assert_hypothetical_membership_requests(
				&mut virtual_overseer,
				expected_request_a.clone(),
			)
			.await;

			// Prospective parachains are notified.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(
						req,
						tx,
					),
				) if
					&req.candidate_receipt == candidate
					&& &req.candidate_para == para_id
					&& pvd == req.persisted_validation_data
				=> {
					tx.send(true).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						_signed_statement,
					)
				) if parent_hash == candidate.descriptor.relay_parent() => {}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
					assert_eq!(candidate.descriptor.relay_parent(), hash);
					assert_matches!(statement.payload(), Statement::Seconded(_));
				}
			);
		}

		virtual_overseer
	});
}

// Test that the subsystem doesn't skip occupied cores assignments.
#[test]
fn occupied_core_assignment() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];
		let previous_para_id = test_state.chain_ids[1];

		// Set the core state to occupied.
		let mut candidate_descriptor =
			polkadot_primitives_test_helpers::dummy_candidate_descriptor(Hash::zero());
		candidate_descriptor.para_id = previous_para_id;
		test_state.availability_cores[0] = CoreState::Occupied(OccupiedCore {
			group_responsible: Default::default(),
			next_up_on_available: Some(ScheduledCore { para_id, collator: None }),
			occupied_since: 100_u32,
			time_out_at: 200_u32,
			next_up_on_time_out: None,
			availability: Default::default(),
			candidate_descriptor: candidate_descriptor.into(),
			candidate_hash: Default::default(),
		});

		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second(
			leaf_a_hash,
			candidate.to_plain(),
			pvd.clone(),
			pov.clone(),
		);

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request = vec![(
			HypotheticalMembershipRequest {
				candidates: vec![hypothetical_candidate.clone()],
				fragment_chain_relay_parent: Some(leaf_a_hash),
			},
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash),
		)];
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request).await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data
			=> {
				tx.send(true).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::StatementDistribution(
				StatementDistributionMessage::Share(
					parent_hash,
					_signed_statement,
				)
			) if parent_hash == leaf_a_parent => {}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
				assert_eq!(leaf_a_parent, hash);
				assert_matches!(statement.payload(), Statement::Seconded(_));
			}
		);

		virtual_overseer
	});
}
