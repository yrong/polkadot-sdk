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
use polkadot_primitives::v9::{MessageBatch, OutgoingMessage, SpeculativeIngress};
use sp_core::H256;
use sp_runtime::BoundedVec;

/// The leaf hash the outbox produces for a message: binds `(source, destination,
/// position)` via `OutgoingMessage::hash_leaf`. `source` must equal the outbox's
/// own para id (`OutboxParaId`) for the inclusion proof to verify.
fn msg_leaf(destination: ParaId, position: u64, payload: &[u8]) -> H256 {
	OutgoingMessage::new(
		OutboxParaId::get(),
		destination,
		position,
		BoundedVec::try_from(payload.to_vec()).unwrap(),
	)
	.hash_leaf()
}

/// Build a batch from the outbox state for `messages` just appended to `destination`.
/// `MessageBatch.source` and each message's `source` are the outbox's para id.
fn build_valid_batch(destination: ParaId, messages: Vec<Vec<u8>>) -> MessageBatch {
	let source = OutboxParaId::get();
	let count = messages.len() as u64;
	SpeculativeOutbox::<Test>::record_outbound_messages(destination, messages.clone());
	let (subtree_root, leaf_count) =
		SpeculativeOutbox::<Test>::destination_state(destination).unwrap();
	let from_position = leaf_count - count;

	let (returned_msgs, subtree_mmr_size, messages_proof) =
		SpeculativeOutbox::<Test>::outbound_messages_with_proof(
			destination,
			from_position,
			count as u32,
		)
		.expect("messages_with_proof");

	MessageBatch {
		source,
		source_relay_parent_number: 1,
		subtree_root,
		subtree_mmr_size,
		messages_proof,
		messages: returned_msgs
			.into_iter()
			.map(|(position, payload)| {
				OutgoingMessage::new(
					source,
					destination,
					position,
					BoundedVec::try_from(payload).unwrap(),
				)
			})
			.collect(),
	}
}

#[test]
fn ingest_valid_batch_updates_requires() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();
		let batch = build_valid_batch(destination, vec![b"xcm-msg".to_vec()]);
		let subtree_root = batch.subtree_root;

		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch] },
		));

		let requires = SpeculativeInbox::<Test>::get_requires_commitments();
		assert_eq!(requires.len(), 1);
		// The recorded `(source, subtree_root)` entry matches the batch.
		assert_eq!(requires.get(OutboxParaId::get()), Some(&subtree_root));
	});
}

#[test]
fn ingest_rejects_non_consecutive_messages() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();
		let mut batch = build_valid_batch(destination, vec![b"first".to_vec()]);
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
fn ingest_rejects_invalid_messages_proof() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();
		let mut batch = build_valid_batch(destination, vec![b"first".to_vec()]);
		// Corrupt the payload — `hash_leaf` of the tampered message no longer
		// matches the leaf the proof was generated against.
		batch.messages[0].payload = BoundedVec::try_from(b"tampered".to_vec()).unwrap();

		assert_noop!(
			SpeculativeInbox::<Test>::ingest_verified_messages(
				RuntimeOrigin::none(),
				SpeculativeIngress { batches: vec![batch] },
			),
			Error::<Test>::InvalidMessagesProof,
		);
	});
}

#[test]
fn ingest_second_batch_requires_consecutive_positions() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();

		let batch1 = build_valid_batch(destination, vec![b"one".to_vec()]);
		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch1] },
		));

		// Build a batch covering only the second message.
		let batch2 = build_valid_batch(destination, vec![b"two".to_vec()]);

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
		let (old_subtree_root, _) =
			SpeculativeOutbox::<Test>::destination_state(destination).unwrap();

		// Finalize block 1 so the historical subtree state is captured.
		System::set_block_number(1);
		SpeculativeOutbox::<Test>::on_finalize(1);

		// Advance: record a third message (position 2).
		SpeculativeOutbox::<Test>::record_outbound_messages(destination, vec![b"msg3".to_vec()]);
		let (new_subtree_root, _) =
			SpeculativeOutbox::<Test>::destination_state(destination).unwrap();

		// Generate the late block proof connecting old → new subtree root.
		let proof =
			SpeculativeOutbox::<Test>::generate_late_block_proof(destination, old_subtree_root)
				.expect("proof should be generated");

		assert_eq!(proof.old_subtree_root, old_subtree_root);
		assert_eq!(proof.new_subtree_root, new_subtree_root);

		let ext = proof.subtree_extension.as_ref().expect("subtree extension must be present");
		assert_eq!(ext.incremental.len(), 1, "one new message appended");
		assert_eq!(
			ext.incremental[0],
			msg_leaf(destination, 2, b"msg3"),
			"incremental leaf must be the appended message's hash_leaf",
		);
	});
}

#[test]
fn ingest_after_root_advance_records_old_root_in_requires() {
	new_test_ext().execute_with(|| {
		let destination = SelfParaId::get();

		// Block 1: record msg1 and finalize.
		let batch1 = build_valid_batch(destination, vec![b"msg1".to_vec()]);
		let old_subtree_root = batch1.subtree_root;

		System::set_block_number(1);
		SpeculativeOutbox::<Test>::on_finalize(1);
		SpeculativeInbox::<Test>::on_initialize(1);

		// Ingest the first batch — requires records the old subtree root.
		assert_ok!(SpeculativeInbox::<Test>::ingest_verified_messages(
			RuntimeOrigin::none(),
			SpeculativeIngress { batches: vec![batch1] },
		));
		let requires = SpeculativeInbox::<Test>::get_requires_commitments();
		assert_eq!(requires.len(), 1);
		assert_eq!(requires.get(OutboxParaId::get()), Some(&old_subtree_root));

		// Block 2: record msg2 — the subtree root advances.
		System::set_block_number(2);
		SpeculativeInbox::<Test>::on_initialize(2);
		SpeculativeOutbox::<Test>::record_outbound_messages(destination, vec![b"msg2".to_vec()]);
		let (new_subtree_root, _) =
			SpeculativeOutbox::<Test>::destination_state(destination).unwrap();
		assert_ne!(old_subtree_root, new_subtree_root);

		// The late block proof connects old → new subtree root.
		let proof =
			SpeculativeOutbox::<Test>::generate_late_block_proof(destination, old_subtree_root)
				.expect("proof must be generated");
		assert_eq!(proof.new_subtree_root, new_subtree_root);
	});
}
