// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use std::{marker::PhantomData, path::PathBuf};

use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_primitives_core::{relay_chain::BlockId, SchedulingProof};
use cumulus_relay_chain_interface::RelayChainInterface;
use futures::prelude::*;
use polkadot_node_primitives::{MaybeCompressedPoV, SegmentCollation, SubmitSegmentParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_primitives::{CandidateDescriptorVersion, CollatorPair, Id as ParaId};
use sc_utils::mpsc::TracingUnboundedReceiver;
use sp_runtime::{
	traits::{Block as BlockT, Header},
	BoundedVec,
};

use crate::export_pov_to_path;

use super::message::{CollationParts, CollatorMessage};

const LOG_TARGET: &str = "aura::cumulus::collation_task";

/// Parameters for the collation task.
pub struct Params<Block: BlockT, RClient, CS> {
	/// A handle to the relay-chain client.
	pub relay_client: RClient,
	/// The collator key used to sign collations before submitting to validators.
	pub collator_key: CollatorPair,
	/// The para's ID.
	pub para_id: ParaId,
	/// Whether we should reinitialize the collator config (i.e. we are transitioning to aura).
	pub reinitialize: bool,
	/// Collator service interface
	pub collator_service: CS,
	/// Receiver channel for collation/segment messages from the block builder task.
	pub collator_receiver: TracingUnboundedReceiver<CollatorMessage<Block>>,
	/// When set, the collator will export every produced `POV` to this folder.
	pub export_pov: Option<PathBuf>,
}

/// Asynchronously executes the collation task for a parachain.
///
/// This function initializes the collator subsystems necessary for producing and submitting
/// collations to the relay chain. It listens for new best relay chain block notifications and
/// handles collator messages. If our parachain is scheduled on a core and we have a candidate,
/// the task will build a collation and send it to the relay chain.
pub async fn run_collation_task<Block, RClient, CS>(
	Params {
		relay_client,
		collator_key,
		para_id,
		reinitialize,
		collator_service,
		mut collator_receiver,
		export_pov,
	}: Params<Block, RClient, CS>,
) where
	Block: BlockT,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	RClient: RelayChainInterface + Clone + 'static,
{
	let Ok(mut overseer_handle) = relay_client.overseer_handle() else {
		tracing::error!(target: LOG_TARGET, "Failed to get overseer handle.");
		return;
	};

	cumulus_client_collator::initialize_collator_subsystems(
		&mut overseer_handle,
		collator_key,
		para_id,
		reinitialize,
	)
	.await;

	let submitter =
		CollationSubmitter { collator_service, relay_client, export_pov, _phantom: PhantomData };

	while let Some(message) = collator_receiver.next().await {
		if let Some(params) = submitter.prepare_submission(message).await {
			overseer_handle
				.send_msg(CollationGenerationMessage::SubmitSegment(params), "SubmitSegment")
				.await;
		}
	}
}

/// Prepares collations for forwarding to the collation-generation subsystem.
struct CollationSubmitter<Block: BlockT, RClient, CS> {
	collator_service: CS,
	relay_client: RClient,
	export_pov: Option<PathBuf>,
	_phantom: PhantomData<Block>,
}

impl<Block, RClient, CS> CollationSubmitter<Block, RClient, CS>
where
	Block: BlockT,
	CS: CollatorServiceInterface<Block>,
	RClient: RelayChainInterface + Clone + 'static,
{
	/// Prepare a V2 collation or V3 segment for the collation-generation subsystem.
	async fn prepare_submission(
		&self,
		message: CollatorMessage<Block>,
	) -> Option<SubmitSegmentParams> {
		match message {
			CollatorMessage::Collation { core_index, parts } => {
				// V2: no scheduling proof, scheduling parent is the relay parent.
				let Some(segment_collation) = self.build_collation(parts, None).await else {
					return None;
				};
				let scheduling_parent = segment_collation.relay_parent;

				tracing::debug!(target: LOG_TARGET, ?core_index, ?scheduling_parent, "Submitting collation for core.");

				Some(SubmitSegmentParams {
					scheduling_parent,
					core_index,
					candidates_descriptor_version: CandidateDescriptorVersion::V2,
					collations: BoundedVec::truncate_from(vec![segment_collation]),
				})
			},
			CollatorMessage::Segment { scheduling_proof, core_index, resubmittable, bundle } => {
				// V3: scheduling parent comes from the proof.
				let scheduling_parent = scheduling_proof.scheduling_parent();

				let resubmitted = resubmittable.len();
				let mut collations = Vec::new();
				for (parts, proof) in resubmittable
					.into_iter()
					.chain(bundle.map(|parts| (parts, scheduling_proof.clone())))
				{
					let Some(collation) = self.build_collation(parts, Some(proof)).await else {
						// Submitting a descendant without its predecessor leaves a segment gap.
						return None;
					};
					collations.push(collation);
				}

				if collations.is_empty() {
					tracing::debug!(
						target: LOG_TARGET,
						?core_index,
						"No collations built for segment; nothing submitted for core.",
					);
					return None;
				}

				tracing::debug!(
					target: LOG_TARGET,
					?core_index,
					segment_len = collations.len(),
					resubmitted = resubmitted,
					"Submitting segment for core.",
				);

				Some(SubmitSegmentParams {
					scheduling_parent,
					core_index,
					candidates_descriptor_version: CandidateDescriptorVersion::V3,
					collations: BoundedVec::truncate_from(collations),
				})
			},
		}
	}
}

impl<Block, RClient, CS> CollationSubmitter<Block, RClient, CS>
where
	Block: BlockT,
	CS: CollatorServiceInterface<Block>,
	RClient: RelayChainInterface + Clone + 'static,
{
	/// Build one `SegmentCollation` from `parts`: build the PoV, export it if configured, and look
	/// up the session index. Returns `None` if the collation could not be built or the session
	/// lookup failed.
	async fn build_collation(
		&self,
		parts: CollationParts<Block>,
		scheduling_proof: Option<SchedulingProof>,
	) -> Option<SegmentCollation> {
		let CollationParts {
			relay_parent,
			parent_header,
			blocks,
			proof,
			validation_code_hash,
			validation_data,
		} = parts;

		// The scheduling proof, including its UMP scheduling-tail override, is applied inside
		// `build_multi_block_collation`.
		let (collation, block_data) = match self.collator_service.build_multi_block_collation(
			&parent_header,
			blocks,
			proof,
			scheduling_proof,
		) {
			Some(collation) => collation,
			None => {
				tracing::warn!(target: LOG_TARGET, ?relay_parent, "Unable to build collation.");
				return None;
			},
		};

		block_data.log_size_info();

		if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
			if let Some(pov_path) = self.export_pov.as_ref() {
				if let Ok(Some(relay_parent_header)) =
					self.relay_client.header(BlockId::Hash(relay_parent)).await
				{
					if let Some(header) = block_data.blocks().first().map(|b| b.header()) {
						export_pov_to_path::<Block>(
							pov_path,
							pov.clone(),
							header.hash(),
							*header.number(),
							parent_header.clone(),
							relay_parent_header.state_root,
							relay_parent_header.number,
							validation_data.max_pov_size,
						);
					}
				} else {
					tracing::error!(target: LOG_TARGET, "Failed to get relay parent header from hash: {relay_parent:?}");
				}
			}

			tracing::info!(
				target: LOG_TARGET,
				block_numbers = ?block_data.blocks().iter().map(|b| *b.header().number()).collect::<Vec<_>>(),
				"Compressed PoV size: {}kb",
				pov.block_data.0.len() as f64 / 1024f64,
			);
		}

		let session_index = match self.relay_client.session_index_for_child(relay_parent).await {
			Ok(session_index) => session_index,
			Err(err) => {
				tracing::error!(
					target: LOG_TARGET,
					?err,
					?relay_parent,
					"Failed to fetch session index."
				);
				return None;
			},
		};

		Some(SegmentCollation {
			relay_parent,
			collation,
			validation_code_hash,
			session_index,
			validation_data,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{
		super::{message::CollatorMessageBuilder, tests::TestRelayClient},
		*,
	};
	use codec::Encode;
	use cumulus_client_consensus_common::ParachainCandidate;
	use cumulus_primitives_core::ParachainBlockData;
	use cumulus_test_client::runtime::Block;
	use polkadot_node_primitives::{BlockData, Collation, PoV};
	use polkadot_primitives::{Hash, HeadData, ValidationCodeHash};
	use sp_api::StorageProof;
	use sp_runtime::traits::Block as BlockT;
	use std::{
		collections::HashMap,
		sync::{Arc, Mutex},
	};

	#[derive(Clone)]
	struct Service {
		seen: Arc<Mutex<Vec<u32>>>,
		proofs: Arc<Mutex<Vec<Vec<u8>>>>,
		fail: Option<u32>,
	}
	impl CollatorServiceInterface<Block> for Service {
		fn check_block_status(&self, _: Hash, _: &<Block as BlockT>::Header) -> bool {
			true
		}
		fn announce_block(&self, _: Hash, _: Option<Vec<u8>>) {}
		fn build_collation(
			&self,
			_: &<Block as BlockT>::Header,
			_: Hash,
			_: ParachainCandidate<Block>,
			_: Option<SchedulingProof>,
		) -> Option<(Collation, ParachainBlockData<Block>)> {
			panic!("segment must use multi-block assembly")
		}
		fn build_multi_block_collation(
			&self,
			_: &<Block as BlockT>::Header,
			blocks: Vec<Block>,
			_: StorageProof,
			scheduling: Option<SchedulingProof>,
		) -> Option<(Collation, ParachainBlockData<Block>)> {
			let number = *blocks[0].header().number();
			self.seen.lock().expect("test mutex; qed").push(number);
			self.proofs.lock().expect("test mutex; qed").push(scheduling.encode());
			if self.fail == Some(number) {
				return None;
			}
			assert!(
				number == 3 ||
					scheduling.as_ref().is_some_and(|p| p.signed_scheduling_info.is_some()),
				"recovery scheduling proof must reach collation assembly"
			);
			let data = ParachainBlockData::new(
				blocks,
				sp_trie::CompactProof { encoded_nodes: vec![] },
				scheduling,
			);
			let collation = Collation {
				upward_messages: Default::default(),
				horizontal_messages: Default::default(),
				new_validation_code: None,
				head_data: HeadData(number.encode()),
				proof_of_validity: MaybeCompressedPoV::Raw(PoV {
					block_data: BlockData(data.encode()),
				}),
				processed_downward_messages: 0,
				hrmp_watermark: 0,
			};
			Some((collation, data))
		}
	}
	fn parts(number: u32) -> CollationParts<Block> {
		let header = <Block as BlockT>::Header::new(
			number,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);
		CollationParts {
			relay_parent: Hash::zero(),
			parent_header: header.clone(),
			blocks: vec![Block::new(header, vec![])],
			proof: StorageProof::empty(),
			validation_code_hash: ValidationCodeHash::from(Hash::zero()),
			validation_data: cumulus_primitives_core::PersistedValidationData {
				parent_head: Default::default(),
				relay_parent_number: 0,
				relay_parent_storage_root: Hash::zero(),
				max_pov_size: 1024,
			},
		}
	}
	fn proof() -> SchedulingProof {
		use sp_consensus_aura::sr25519::{AuthorityId, AuthorityPair};
		use sp_core::Pair;
		use sp_keystore::{testing::MemoryKeystore, Keystore};
		let keys: sp_keystore::KeystorePtr = MemoryKeystore::new().into();
		let author: AuthorityId = keys
			.sr25519_generate_new(sp_application_crypto::key_types::AURA, Some("//Alice"))
			.expect("test key; qed")
			.into();
		let core = cumulus_primitives_core::CoreInfo {
			selector: cumulus_primitives_core::CoreSelector(2),
			claim_queue_offset: cumulus_primitives_core::ClaimQueueOffset(0),
			number_of_cores: 1u16.into(),
		};
		let proof = super::super::scheduling::SchedulingProofBuilder::<AuthorityPair>::new(
			polkadot_primitives::Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				Default::default(),
			),
		)
		.for_core(&core)
		.crediting_peer(sc_network_types::PeerId::random())
		.keystore(&keys)
		.build_with_signed_payload(&author)
		.expect("test author can sign; qed");
		let signed = proof.signed_scheduling_info.as_ref().expect("signed proof; qed");
		assert!(AuthorityPair::verify(
			&sp_core::sr25519::Signature::from_raw(signed.signature).into(),
			signed.payload.encode(),
			&author
		));
		proof
	}
	fn submitter(fail: Option<u32>) -> CollationSubmitter<Block, TestRelayClient, Service> {
		CollationSubmitter {
			collator_service: Service {
				seen: Default::default(),
				proofs: Default::default(),
				fail,
			},
			relay_client: TestRelayClient::new(HashMap::new()),
			export_pov: None,
			_phantom: PhantomData,
		}
	}
	#[tokio::test]
	async fn segment_preparation_keeps_recovery_before_new_work() {
		let submitter = submitter(None);
		let mut scheduling = proof();
		scheduling.signed_scheduling_info = None;
		let old = vec![(parts(1), proof()), (parts(2), proof())];
		let expected: Vec<_> = old
			.iter()
			.map(|(_, proof)| Some(proof.clone()).encode())
			.chain(core::iter::once(Some(scheduling.clone()).encode()))
			.collect();
		let scheduling_parent = scheduling.scheduling_parent();
		let message = CollatorMessageBuilder::new(polkadot_primitives::CoreIndex(2), old)
			.with_bundle(parts(3))
			.with_scheduling_proof(scheduling)
			.build()
			.expect("nonempty segment; qed");
		let params =
			submitter.prepare_submission(message).await.expect("all collations build; qed");
		assert_eq!(params.candidates_descriptor_version, CandidateDescriptorVersion::V3);
		assert_eq!(params.scheduling_parent, scheduling_parent);
		assert_eq!(params.core_index, polkadot_primitives::CoreIndex(2));
		assert_eq!(params.collations.len(), 3);
		assert_eq!(*submitter.collator_service.proofs.lock().expect("test mutex; qed"), expected);
		assert_eq!(
			*submitter.collator_service.seen.lock().expect("test mutex; qed"),
			vec![1, 2, 3]
		);
		assert_eq!(
			params
				.collations
				.iter()
				.map(|c| c.collation.head_data.0.clone())
				.collect::<Vec<_>>(),
			vec![1u32.encode(), 2u32.encode(), 3u32.encode()]
		);
	}
	#[tokio::test]
	async fn failed_predecessor_prevents_descendant_submission() {
		let submitter = submitter(Some(1));
		let message = CollatorMessageBuilder::new(
			polkadot_primitives::CoreIndex(2),
			vec![(parts(1), proof()), (parts(2), proof())],
		)
		.with_bundle(parts(3))
		.with_scheduling_proof(proof())
		.build()
		.expect("nonempty segment; qed");
		assert!(submitter.prepare_submission(message).await.is_none());
		assert_eq!(*submitter.collator_service.seen.lock().expect("test mutex; qed"), vec![1]);
	}
}
