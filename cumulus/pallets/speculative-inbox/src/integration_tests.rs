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
use frame_support::{assert_noop, assert_ok, traits::Hooks};
use polkadot_primitives::v10::{MessageBatch, OutgoingMessage, SpeculativeIngress};
use sp_core::H256;
use sp_runtime::traits::{Hash as _, Keccak256};

fn build_valid_batch(source: ParaId, destination: ParaId, messages: Vec<Vec<u8>>) -> MessageBatch {
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
			.map(|(i, payload)| OutgoingMessage { position: i as u64, payload })
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
			messages: vec![OutgoingMessage { position: 1, payload: b"two".to_vec() }],
		};

		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch2] },
		));
		assert_eq!(SpeculativeInbox::<Test>::get_requires_commitments().len(), 1);
	});
}

#[test]
fn late_block_proof_roundtrip() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();

		// Block 1: record two messages.
		SpeculativeOutbox::<Test>::record_outbound_messages(
			destination,
			vec![b"msg1".to_vec(), b"msg2".to_vec()],
		);
		let old_provides_root = SpeculativeOutbox::<Test>::compute_provides_root().unwrap().root;

		// Finalize block 1 so history is captured.
		System::set_block_number(1);
		SpeculativeOutbox::<Test>::on_finalize(1);

		// Advance: record a third message.
		SpeculativeOutbox::<Test>::record_outbound_messages(destination, vec![b"msg3".to_vec()]);
		let new_provides_root = SpeculativeOutbox::<Test>::compute_provides_root().unwrap().root;

		// Generate the late block proof.
		let proof =
			SpeculativeOutbox::<Test>::generate_late_block_proof(destination, old_provides_root)
				.expect("proof should be generated");

		assert_eq!(proof.old_provides_root, old_provides_root);
		assert_eq!(proof.new_provides_root, new_provides_root);
		// Note: proof.source is filled in by the runtime API layer (penpal sets it to
		// ParachainInfo::parachain_id()). At the pallet level it is set to `dest`.

		let ext = proof.subtree_extension.as_ref().expect("subtree extension must be present");
		assert_eq!(ext.connecting_nodes.len(), 1, "one new message appended");
		assert_eq!(
			ext.connecting_nodes[0],
			Keccak256::hash(b"msg3"),
			"connecting_node must be Keccak256(payload)"
		);
		assert_eq!(ext.old_leaf_count, 2, "old MMR had 2 leaves");
	});
}

#[test]
fn ingest_after_root_advance_records_old_root_in_requires() {
	new_test_ext().execute_with(|| {
		let source = ParaId::new(1000);
		let destination = SelfParaId::get();

		// Block 1: record msg1 and finalize.
		let batch1 = build_valid_batch(source, destination, vec![b"msg1".to_vec()]);
		let old_provides_root = batch1.provides_root;

		System::set_block_number(1);
		SpeculativeOutbox::<Test>::on_finalize(1);
		SpeculativeInbox::<Test>::on_initialize(1);

		// Ingest the first batch — requires records old_provides_root.
		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch1] },
		));
		let requires = SpeculativeInbox::<Test>::get_requires_commitments();
		assert_eq!(requires.len(), 1);
		assert_eq!(requires[0].source, source);
		assert_eq!(requires[0].expected_root, old_provides_root);

		// Block 2: record msg2 — root advances.
		System::set_block_number(2);
		SpeculativeInbox::<Test>::on_initialize(2);
		SpeculativeOutbox::<Test>::record_outbound_messages(destination, vec![b"msg2".to_vec()]);
		let new_provides_root = SpeculativeOutbox::<Test>::compute_provides_root().unwrap().root;
		assert_ne!(old_provides_root, new_provides_root);

		// The late block proof connects old → new root.
		let proof =
			SpeculativeOutbox::<Test>::generate_late_block_proof(destination, old_provides_root)
				.expect("proof must be generated");
		assert_eq!(proof.new_provides_root, new_provides_root);
	});
}
