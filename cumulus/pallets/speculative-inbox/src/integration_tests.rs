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

use crate::{mock::*, Error, Pallet as SpeculativeInbox};
use cumulus_pallet_speculative_outbox::Pallet as SpeculativeOutbox;
use cumulus_primitives_core::ParaId;
use frame_support::{assert_noop, assert_ok};
use polkadot_primitives::v10::{MessageBatch, OutgoingMessage, SpeculativeIngress};
use sp_core::H256;

fn build_valid_batch(
	source: ParaId,
	destination: ParaId,
	messages: Vec<Vec<u8>>,
) -> MessageBatch {
	SpeculativeOutbox::<Test>::record_outbound_messages(destination, messages.clone());
	let provides = SpeculativeOutbox::<Test>::compute_provides_root().unwrap();
	let (subtree_root, _) = SpeculativeOutbox::<Test>::destination_state(destination).unwrap();
	let (proof, number_of_destinations, leaf_index) =
		SpeculativeOutbox::<Test>::subtree_inclusion_proof(destination, subtree_root).unwrap();

	MessageBatch {
		source,
		source_block: H256::from_low_u64_be(1),
		source_relay_parent_number: 1,
		provides_root: provides.root,
		subtree_root,
		subtree_inclusion_proof: proof,
		number_of_destinations,
		leaf_index,
		messages: messages
			.into_iter()
			.enumerate()
			.map(|(i, payload)| OutgoingMessage {
				position: i as u64,
				payload,
			})
			.collect(),
	}
}

#[test]
fn ingest_valid_batch_updates_requires() {
	new_test_ext().execute_with(|| {
		let source = ParaId::new(1000);
		let destination = SelfParaId::get();
		let batch = build_valid_batch(source, destination, vec![b"xcm-msg".to_vec()]);

		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch] },
		));

		let requires = SpeculativeInbox::<Test>::get_requires_commitments();
		assert_eq!(requires.len(), 1);
		assert_eq!(requires[0].source, source);
	});
}

#[test]
fn ingest_rejects_invalid_subtree_proof() {
	new_test_ext().execute_with(|| {
		let source = ParaId::new(1000);
		let destination = SelfParaId::get();
		let mut batch = build_valid_batch(source, destination, vec![b"xcm-msg".to_vec()]);
		batch.leaf_index = batch.number_of_destinations;

		assert_noop!(
			SpeculativeInbox::<Test>::ingest_verified_messages(
				RuntimeOrigin::none(),
				SpeculativeIngress { batches: vec![batch] },
			),
			Error::<Test>::InvalidSubtreeProof,
		);
	});
}

#[test]
fn ingest_rejects_non_consecutive_messages() {
	new_test_ext().execute_with(|| {
		let source = ParaId::new(1000);
		let destination = SelfParaId::get();
		let mut batch = build_valid_batch(source, destination, vec![b"first".to_vec()]);
		batch.messages[0].position = 1;

		assert_noop!(
			SpeculativeInbox::<Test>::ingest_verified_messages(
				RuntimeOrigin::none(),
				SpeculativeIngress { batches: vec![batch] },
			),
			Error::<Test>::NonConsecutiveMessage,
		);
	});
}

#[test]
fn ingest_second_batch_requires_consecutive_positions() {
	new_test_ext().execute_with(|| {
		let source = ParaId::new(1000);
		let destination = SelfParaId::get();

		let batch1 = build_valid_batch(source, destination, vec![b"one".to_vec()]);
		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch1] },
		));

		// Record one more message on the sender outbox.
		SpeculativeOutbox::<Test>::record_outbound_messages(destination, vec![b"two".to_vec()]);
		let provides = SpeculativeOutbox::<Test>::compute_provides_root().unwrap();
		let (subtree_root, _) = SpeculativeOutbox::<Test>::destination_state(destination).unwrap();
		let (proof, number_of_destinations, leaf_index) =
			SpeculativeOutbox::<Test>::subtree_inclusion_proof(destination, subtree_root).unwrap();

		let batch2 = MessageBatch {
			source,
			source_block: H256::from_low_u64_be(2),
			source_relay_parent_number: 2,
			provides_root: provides.root,
			subtree_root,
			subtree_inclusion_proof: proof,
			number_of_destinations,
			leaf_index,
			messages: vec![OutgoingMessage {
				position: 1,
				payload: b"two".to_vec(),
			}],
		};

		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch2] },
		));
		assert_eq!(SpeculativeInbox::<Test>::get_requires_commitments().len(), 1);
	});
}
