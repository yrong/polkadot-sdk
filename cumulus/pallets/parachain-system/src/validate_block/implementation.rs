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

//! The actual implementation of the validate block functionality.

use super::{trie_cache, trie_recorder, MemoryOptimizedValidationParams};
use alloc::vec::Vec;
use codec::{Decode, Encode};
use cumulus_primitives_core::{
	relay_chain::{
		BlockNumber as RNumber, Hash as RHash, UMPSignal, MAX_HEAD_DATA_SIZE, UMP_SEPARATOR,
	},
	ClaimQueueOffset, CoreSelector, CumulusDigestItem, ParachainBlockData, PersistedValidationData,
};
use cumulus_primitives_spec_messaging::mmr::SpecMerge;
use frame_support::{
	traits::{ExecuteBlock, Get, IsSubType},
	BoundedVec,
};
use mmr_lib::MerkleProof;
use polkadot_parachain_primitives::primitives::{HeadData, ValidationResult};
use sp_core::storage::{well_known_keys, ChildInfo, StateVersion};
use sp_externalities::{set_and_run_with_externalities, Externalities};
use sp_io::{hashing::blake2_128, KillStorageResult};
use sp_runtime::traits::{
	Block as BlockT, ExtrinsicCall, Hash as HashT, HashingFor, Header as HeaderT, LazyBlock,
};
use sp_state_machine::OverlayedChanges;
use sp_trie::{HashDBT, ProofSizeProvider, EMPTY_PREFIX};
use trie_recorder::{SeenNodes, SizeOnlyRecorderProvider};

/// Transform `requires` commitments that reference an older source subtree root
/// into commitments referencing the current source subtree root, given a valid
/// append-only (ancestry) proof in the PoV.
///
/// Flat commitment: there is no top-level inclusion proof — the relay chain
/// matches each `subtree_root` directly. So the proof only needs to show the
/// receiver's per-destination MMR was extended `old_subtree_root → new_subtree_root`
/// (`mmr_lib` `verify_incremental` with `SpecMerge`).
fn apply_messaging_proofs(
	extension: &mut Option<polkadot_parachain_primitives::primitives::ValidationResultExtension>,
	proofs: Vec<polkadot_primitives::v9::LateBlockProof>,
) {
	if let Some(polkadot_parachain_primitives::primitives::ValidationResultExtension::V4 {
		ref mut requires,
		..
	}) = extension
	{
		for proof in proofs {
			for req in requires.iter_mut() {
				if req.0 == proof.source && req.1 == proof.old_subtree_root {
					// Identical roots need no proof; otherwise verify the
					// append-only extension from old → new subtree root.
					let valid = if proof.old_subtree_root == proof.new_subtree_root {
						true
					} else {
						proof.subtree_extension.as_ref().map_or(false, |ext| {
							MerkleProof::<polkadot_primitives::Hash, SpecMerge>::new(
								ext.new_mmr_size,
								ext.proof.clone(),
							)
							.verify_incremental(
								proof.new_subtree_root,
								proof.old_subtree_root,
								ext.incremental.clone(),
							)
							.unwrap_or(false)
						})
					};

					if valid {
						req.1 = proof.new_subtree_root;
					}
				}
			}
		}
	}
}

type Ext<'a, Block, Backend> = sp_state_machine::Ext<'a, HashingFor<Block>, Backend>;

fn with_externalities<F: FnOnce(&mut dyn Externalities) -> R, R>(f: F) -> R {
	sp_externalities::with_externalities(f).expect("Environmental externalities not set.")
}

// Recorder instance to be used during this validate_block call.
environmental::environmental!(recorder: trait ProofSizeProvider);

/// Validate the given parachain block.
///
/// This function is doing roughly the following:
///
/// 1. We decode the [`ParachainBlockData`] from the `block_data` in `params`.
///
/// 2. We are doing some security checks like checking that the `parent_head` in `params`
/// is the parent of the block we are going to check. We also ensure that the `set_validation_data`
/// inherent is present in the block and that the validation data matches the values in `params`.
///
/// 3. We construct the sparse in-memory database from the storage proof inside the block data and
/// then ensure that the storage root matches the storage root in the `parent_head`.
///
/// 4. We replace all the storage related host functions with functions inside the wasm blob.
/// This means instead of calling into the host, we will stay inside the wasm execution. This is
/// very important as the relay chain validator hasn't the state required to verify the block. But
/// we have the in-memory database that contains all the values from the state of the parachain
/// that we require to verify the block.
///
/// 5. The last step is to execute the entire block in the machinery we just have setup. Executing
/// the blocks include running all transactions in the block against our in-memory database and
/// ensuring that the final storage root matches the storage root in the header of the block. In the
/// end we return back the [`ValidationResult`] with all the required information for the validator.
#[doc(hidden)]
pub fn validate_block<B: BlockT, E: ExecuteBlock<B>, PSC: crate::Config>(
	MemoryOptimizedValidationParams {
		block_data,
		parent_head: parachain_head,
		relay_parent_number,
		relay_parent_storage_root,
	}: MemoryOptimizedValidationParams,
) -> ValidationResult
where
	B::Extrinsic: ExtrinsicCall,
	<B::Extrinsic as ExtrinsicCall>::Call: IsSubType<crate::Call<PSC>>,
{
	let _guard = (
		// Replace storage calls with our own implementations
		sp_io::storage::host_read.replace_implementation(host_storage_read),
		sp_io::storage::host_set.replace_implementation(host_storage_set),
		sp_io::storage::host_get.replace_implementation(host_storage_get),
		sp_io::storage::host_exists.replace_implementation(host_storage_exists),
		sp_io::storage::host_clear.replace_implementation(host_storage_clear),
		sp_io::storage::host_root.replace_implementation(host_storage_root),
		sp_io::storage::host_clear_prefix.replace_implementation(host_storage_clear_prefix),
		sp_io::storage::host_append.replace_implementation(host_storage_append),
		sp_io::storage::host_next_key.replace_implementation(host_storage_next_key),
		sp_io::storage::host_start_transaction
			.replace_implementation(host_storage_start_transaction),
		sp_io::storage::host_rollback_transaction
			.replace_implementation(host_storage_rollback_transaction),
		sp_io::storage::host_commit_transaction
			.replace_implementation(host_storage_commit_transaction),
		sp_io::default_child_storage::host_get
			.replace_implementation(host_default_child_storage_get),
		sp_io::default_child_storage::host_read
			.replace_implementation(host_default_child_storage_read),
		sp_io::default_child_storage::host_set
			.replace_implementation(host_default_child_storage_set),
		sp_io::default_child_storage::host_clear
			.replace_implementation(host_default_child_storage_clear),
		sp_io::default_child_storage::host_storage_kill
			.replace_implementation(host_default_child_storage_storage_kill),
		sp_io::default_child_storage::host_exists
			.replace_implementation(host_default_child_storage_exists),
		sp_io::default_child_storage::host_clear_prefix
			.replace_implementation(host_default_child_storage_clear_prefix),
		sp_io::default_child_storage::host_root
			.replace_implementation(host_default_child_storage_root),
		sp_io::default_child_storage::host_next_key
			.replace_implementation(host_default_child_storage_next_key),
		sp_io::offchain_index::host_set.replace_implementation(host_offchain_index_set),
		sp_io::offchain_index::host_clear.replace_implementation(host_offchain_index_clear),
		cumulus_primitives_proof_size_hostfunction::storage_proof_size::host_storage_proof_size
			.replace_implementation(host_storage_proof_size),
		#[cfg(feature = "transaction-index")]
		sp_io::transaction_index::host_index.replace_implementation(host_transaction_index_index),
		#[cfg(feature = "transaction-index")]
		sp_io::transaction_index::host_renew.replace_implementation(host_transaction_index_renew),
	);

	let block_data = codec::decode_from_bytes::<ParachainBlockData<B::LazyBlock>>(block_data)
		.expect("Invalid parachain block data");

	let messaging_proofs = match &block_data {
		ParachainBlockData::V2 { late_block_proofs, .. } => Some(late_block_proofs.clone()),
		_ => None,
	};

	// Initialize hashmaps randomness.
	sp_trie::add_extra_randomness(build_seed_from_head_data::<B>(
		&block_data,
		relay_parent_storage_root,
	));

	let mut parent_header =
		codec::decode_from_bytes::<B::Header>(parachain_head.clone()).expect("Invalid parent head");

	let (blocks, proof) = block_data.into_inner();

	verify_blocks_form_chain::<B>(&blocks, &parent_header);

	let mut processed_downward_messages = 0;
	let mut upward_messages = BoundedVec::default();
	let mut upward_message_signals = Vec::<Vec<_>>::new();
	let mut horizontal_messages = BoundedVec::default();
	let mut hrmp_watermark = Default::default();
	let mut head_data = None;
	let mut new_validation_code = None;
	// Captured inside the last block's externalities context so storage APIs are available.
	let mut speculative_ext: Option<
		polkadot_parachain_primitives::primitives::ValidationResultExtension,
	> = None;
	let num_blocks = blocks.len();

	// Create the db
	let mut db = match proof.to_memory_db(Some(parent_header.state_root())) {
		Ok((db, _)) => db,
		Err(_) => panic!("Compact proof decoding failure."),
	};

	core::mem::drop(proof);

	let cache_provider = trie_cache::CacheProvider::new();
	let seen_nodes = SeenNodes::<HashingFor<B>>::default();

	for (block_index, mut block) in blocks.into_iter().enumerate() {
		// We use the storage root of the `parent_head` to ensure that it is the correct root.
		// This is already being done above while creating the in-memory db, but let's be paranoid!!
		let backend = sp_state_machine::TrieBackendBuilder::new_with_cache(
			&db,
			*parent_header.state_root(),
			&cache_provider,
		)
		.build();

		// Each node only contributes once to the total size of the storage proof. So, we keep track
		// of them inside `seen_nodes` to always return the correct proof size.
		let mut execute_recorder = SizeOnlyRecorderProvider::with_seen_nodes(seen_nodes.clone());
		// `backend` with the `execute_recorder`. As the `execute_recorder`, this should only be
		// used for `execute_block`.
		let execute_backend = sp_state_machine::TrieBackendBuilder::wrap(&backend)
			.with_recorder(execute_recorder.clone())
			.build();

		let mut overlay = OverlayedChanges::default();

		parent_header = block.header().clone();

		run_with_externalities_and_recorder::<B, _, _>(
			&backend,
			&mut Default::default(),
			&mut Default::default(),
			|| {
				E::verify_and_remove_seal(&mut block);
			},
		);

		run_with_externalities_and_recorder::<B, _, _>(
			&execute_backend,
			// Here is the only place where we want to use the recorder.
			// We want to ensure that we not accidentally read something from the proof, that
			// was not yet read and thus, alter the proof size. Otherwise, we end up with
			// mismatches in later blocks.
			&mut execute_recorder,
			&mut overlay,
			|| {
				E::execute_verified_block(block);
			},
		);

		let code_upgrade_detected =
			if <PSC as frame_system::Config>::Version::get().system_version >= 3 {
				overlay.storage(well_known_keys::PENDING_CODE).is_some()
			} else {
				overlay.storage(well_known_keys::CODE).is_some()
			};
		if code_upgrade_detected && num_blocks > 1 {
			panic!(
				"When applying a runtime upgrade, only one block per PoV is allowed. Received {num_blocks}."
			)
		}
		run_with_externalities_and_recorder::<B, _, _>(
			&backend,
			&mut Default::default(),
			// We are only reading here, but need to know what the old block has written. Thus, we
			// are passing here the overlay.
			&mut overlay,
			|| {
				// Ensure the validation data is correct.
				validate_validation_data(
					crate::ValidationData::<PSC>::get()
						.expect("`ValidationData` must be set after executing a block; qed"),
					&parachain_head,
					relay_parent_number,
					relay_parent_storage_root,
				);

				new_validation_code =
					new_validation_code.take().or(crate::NewValidationCode::<PSC>::get());

				let mut found_separator = false;
				crate::UpwardMessages::<PSC>::get()
					.into_iter()
					.filter_map(|m| {
						// Filter out the `UMP_SEPARATOR` and the `UMPSignals`.
						if m == UMP_SEPARATOR {
							found_separator = true;
							None
						} else if found_separator {
							upward_message_signals.push(m);
							None
						} else {
							// No signal or separator
							Some(m)
						}
					})
					.for_each(|m| {
						upward_messages.try_push(m).expect(
							"Number of upward messages should not be greater than `MAX_UPWARD_MESSAGE_NUM`",
						)
					});

				processed_downward_messages += crate::ProcessedDownwardMessages::<PSC>::get();
				horizontal_messages
					.try_extend(crate::HrmpOutboundMessages::<PSC>::get().into_iter())
					.expect(
						"Number of horizontal messages should not be greater than `MAX_HORIZONTAL_MESSAGE_NUM`",
					);
				hrmp_watermark = crate::HrmpWatermark::<PSC>::get();

				if block_index + 1 == num_blocks {
					head_data = Some(
						crate::CustomValidationHeadData::<PSC>::get()
							.map_or_else(|| HeadData(parent_header.encode()), HeadData),
					);
					// Must be called here while externalities are still active; storage
					// iteration in `compute_provides` panics outside this context.
					speculative_ext = PSC::speculative_extension();
				}
			},
		);

		if block_index + 1 != num_blocks {
			let mut changes = overlay
				.drain_storage_changes(
					&backend,
					<PSC as frame_system::Config>::Version::get().state_version(),
				)
				.expect("Failed to get drain storage changes from the overlay.");

			drop(backend);

			// We just forward the changes directly to our db.
			changes.transaction.drain().into_iter().for_each(|(_, (value, count))| {
				// We only care about inserts and not deletes.
				if count > 0 {
					db.insert(EMPTY_PREFIX, &value);

					let hash = HashingFor::<B>::hash(&value);
					seen_nodes.borrow_mut().insert(hash);
				}
			});
		}
	}

	if !upward_message_signals.is_empty() {
		let mut selected_core: Option<(CoreSelector, ClaimQueueOffset)> = None;
		let mut approved_peer = None;

		upward_message_signals.iter().for_each(|s| {
			match UMPSignal::decode(&mut &s[..]).expect("Failed to decode `UMPSignal`") {
				UMPSignal::SelectCore(selector, offset) => match &selected_core {
					Some(selected_core) if *selected_core != (selector, offset) => {
						panic!(
							"All `SelectCore` signals need to select the same core: {selected_core:?} vs {:?}",
							(selector, offset),
						)
					},
					Some(_) => {},
					None => {
						selected_core = Some((selector, offset));
					},
				},
				UMPSignal::ApprovedPeer(new_approved_peer) => match &approved_peer {
					Some(approved_peer) if *approved_peer != new_approved_peer => {
						panic!(
							"All `ApprovedPeer` signals need to select the same peer_id: {new_approved_peer:?} vs {approved_peer:?}",
						)
					},
					Some(_) => {},
					None => {
						approved_peer = Some(new_approved_peer);
					},
				},
			}
		});

		upward_messages
			.try_push(UMP_SEPARATOR)
			.expect("UMPSignals does not fit in UMPMessages");

		upward_messages
			.try_extend(upward_message_signals.into_iter())
			.expect("UMPSignals does not fit in UMPMessages");
	}

	horizontal_messages.sort_by(|a, b| a.recipient.cmp(&b.recipient));

	let mut extension = speculative_ext;
	if let Some(proofs) = messaging_proofs {
		apply_messaging_proofs(&mut extension, proofs);
	}

	ValidationResult {
		head_data: head_data.expect("HeadData not set"),
		new_validation_code: new_validation_code.map(Into::into),
		upward_messages,
		processed_downward_messages,
		horizontal_messages,
		hrmp_watermark,
		speculative: polkadot_primitives::TrailingOption(extension),
	}
}

/// Validates the given [`PersistedValidationData`] against the data from the relay chain.
fn validate_validation_data(
	validation_data: PersistedValidationData,
	parent_header: &[u8],
	relay_parent_number: RNumber,
	relay_parent_storage_root: RHash,
) {
	assert_eq!(parent_header, &validation_data.parent_head.0, "Parent head doesn't match");
	assert_eq!(
		relay_parent_number, validation_data.relay_parent_number,
		"Relay parent number doesn't match",
	);
	assert_eq!(
		relay_parent_storage_root, validation_data.relay_parent_storage_root,
		"Relay parent storage root doesn't match",
	);
}

fn verify_blocks_form_chain<B: BlockT>(blocks: &[B::LazyBlock], parent_header: &B::Header) {
	let num_blocks = blocks.len();

	// Check first block's parent matches the given parent_header
	assert_eq!(
		*blocks
			.first()
			.expect("BlockData should have at least one block")
			.header()
			.parent_hash(),
		parent_header.hash(),
		"Parachain head needs to be the parent of the first block"
	);

	let mut first_block_has_bundle_info: Option<bool> = None;

	blocks.iter().enumerate().fold(
		parent_header.hash(),
		|expected_parent, (block_index, block)| {
			// Check chain validity
			assert_eq!(
				expected_parent,
				*block.header().parent_hash(),
				"Not a valid chain of blocks :(; {:?} not a parent of {:?}?",
				array_bytes::bytes2hex("0x", expected_parent.as_ref()),
				array_bytes::bytes2hex("0x", block.header().parent_hash().as_ref()),
			);

			let encoded_header_size = block.header().encoded_size();
			assert!(
				encoded_header_size <= MAX_HEAD_DATA_SIZE as usize,
				"Header size {encoded_header_size} exceeds MAX_HEAD_DATA_SIZE {MAX_HEAD_DATA_SIZE}",
			);

			// Validate BlockBundleInfo consistency
			let bundle_info = CumulusDigestItem::find_block_bundle_info(block.header().digest());
			match (first_block_has_bundle_info, &bundle_info) {
				(None, info) => {
					first_block_has_bundle_info = Some(info.is_some());
				},
				(Some(true), None) => {
					panic!("All blocks in a bundled PoV must include `BlockBundleInfo`");
				},
				(Some(false), _) => {
					panic!("A PoV without `BlockBundleInfo` may only contain a single block");
				},
				_ => {},
			}

			if let Some(ref info) = bundle_info {
				assert_eq!(
					info.index as usize, block_index,
					"BlockBundleInfo index mismatch: expected {block_index}, got {}",
					info.index
				);

				if block_index + 1 < num_blocks {
					assert!(
						!CumulusDigestItem::is_last_block_in_core(block.header().digest()).unwrap_or(false),
						"Intermediate block at index {block_index} is marked as last block in core, \
						but more blocks follow in the PoV",
					);
				} else if !CumulusDigestItem::is_last_block_in_core(block.header().digest())
					.unwrap_or(true)
				{
					panic!(
						"Last block in PoV must include the digest that marks it as the last block in the core"
					);
				}
			}

			block.header().hash()
		},
	);
}

/// Build a seed from the head data of the parachain block.
///
/// Uses both the relay parent storage root and the hash of the blocks
/// in the block data, to make sure the seed changes every block and that
/// the user cannot find about it ahead of time.
fn build_seed_from_head_data<B: BlockT>(
	block_data: &ParachainBlockData<B::LazyBlock>,
	relay_parent_storage_root: crate::relay_chain::Hash,
) -> [u8; 16] {
	let mut bytes_to_hash = Vec::with_capacity(
		block_data.blocks().len() * size_of::<B::Hash>() + size_of::<crate::relay_chain::Hash>(),
	);

	bytes_to_hash.extend_from_slice(relay_parent_storage_root.as_ref());
	block_data.blocks().iter().for_each(|block| {
		bytes_to_hash.extend_from_slice(block.header().hash().as_ref());
	});

	blake2_128(&bytes_to_hash)
}

/// Run the given closure with the externalities and recorder set.
fn run_with_externalities_and_recorder<Block: BlockT, R, F: FnOnce() -> R>(
	backend: &impl sp_state_machine::Backend<HashingFor<Block>>,
	recorder: &mut SizeOnlyRecorderProvider<HashingFor<Block>>,
	overlay: &mut OverlayedChanges<HashingFor<Block>>,
	execute: F,
) -> R {
	let mut ext = Ext::<Block, _>::new(overlay, backend);

	recorder::using(recorder, || set_and_run_with_externalities(&mut ext, || execute()))
}

fn host_storage_read(key: &[u8], value_out: &mut [u8], value_offset: u32) -> Option<u32> {
	match with_externalities(|ext| ext.storage(key)) {
		Some(value) => {
			let value_offset = value_offset as usize;
			let data = &value[value_offset.min(value.len())..];
			let written = core::cmp::min(data.len(), value_out.len());
			value_out[..written].copy_from_slice(&data[..written]);
			Some(value.len() as u32)
		},
		None => None,
	}
}

fn host_storage_set(key: &[u8], value: &[u8]) {
	with_externalities(|ext| ext.place_storage(key.to_vec(), Some(value.to_vec())))
}

fn host_storage_get(key: &[u8]) -> Option<bytes::Bytes> {
	with_externalities(|ext| ext.storage(key).map(|value| value.into()))
}

fn host_storage_exists(key: &[u8]) -> bool {
	with_externalities(|ext| ext.exists_storage(key))
}

fn host_storage_clear(key: &[u8]) {
	with_externalities(|ext| ext.place_storage(key.to_vec(), None))
}

fn host_storage_proof_size() -> u64 {
	recorder::with(|rec| rec.estimate_encoded_size()).expect("Recorder is always set; qed") as _
}

fn host_storage_root(version: StateVersion) -> Vec<u8> {
	with_externalities(|ext| ext.storage_root(version))
}

fn host_storage_clear_prefix(prefix: &[u8], limit: Option<u32>) -> KillStorageResult {
	with_externalities(|ext| ext.clear_prefix(prefix, limit, None).into())
}

fn host_storage_append(key: &[u8], value: Vec<u8>) {
	with_externalities(|ext| ext.storage_append(key.to_vec(), value))
}

fn host_storage_next_key(key: &[u8]) -> Option<Vec<u8>> {
	with_externalities(|ext| ext.next_storage_key(key))
}

fn host_storage_start_transaction() {
	with_externalities(|ext| ext.storage_start_transaction())
}

fn host_storage_rollback_transaction() {
	with_externalities(|ext| ext.storage_rollback_transaction().ok())
		.expect("No open transaction that can be rolled back.");
}

fn host_storage_commit_transaction() {
	with_externalities(|ext| ext.storage_commit_transaction().ok())
		.expect("No open transaction that can be committed.");
}

fn host_default_child_storage_get(storage_key: &[u8], key: &[u8]) -> Option<Vec<u8>> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.child_storage(&child_info, key))
}

fn host_default_child_storage_read(
	storage_key: &[u8],
	key: &[u8],
	value_out: &mut [u8],
	value_offset: u32,
) -> Option<u32> {
	let child_info = ChildInfo::new_default(storage_key);
	match with_externalities(|ext| ext.child_storage(&child_info, key)) {
		Some(value) => {
			let value_offset = value_offset as usize;
			let data = &value[value_offset.min(value.len())..];
			let written = core::cmp::min(data.len(), value_out.len());
			value_out[..written].copy_from_slice(&data[..written]);
			Some(value.len() as u32)
		},
		None => None,
	}
}

fn host_default_child_storage_set(storage_key: &[u8], key: &[u8], value: &[u8]) {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| {
		ext.place_child_storage(&child_info, key.to_vec(), Some(value.to_vec()))
	})
}

fn host_default_child_storage_clear(storage_key: &[u8], key: &[u8]) {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.place_child_storage(&child_info, key.to_vec(), None))
}

fn host_default_child_storage_storage_kill(
	storage_key: &[u8],
	limit: Option<u32>,
) -> KillStorageResult {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.kill_child_storage(&child_info, limit, None).into())
}

fn host_default_child_storage_exists(storage_key: &[u8], key: &[u8]) -> bool {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.exists_child_storage(&child_info, key))
}

fn host_default_child_storage_clear_prefix(
	storage_key: &[u8],
	prefix: &[u8],
	limit: Option<u32>,
) -> KillStorageResult {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.clear_child_prefix(&child_info, prefix, limit, None).into())
}

fn host_default_child_storage_root(storage_key: &[u8], version: StateVersion) -> Vec<u8> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.child_storage_root(&child_info, version))
}

fn host_default_child_storage_next_key(storage_key: &[u8], key: &[u8]) -> Option<Vec<u8>> {
	let child_info = ChildInfo::new_default(storage_key);
	with_externalities(|ext| ext.next_child_storage_key(&child_info, key))
}

fn host_offchain_index_set(_key: &[u8], _value: &[u8]) {}

fn host_offchain_index_clear(_key: &[u8]) {}

/// Parachain validation does not require maintaining a transaction index,
/// and indexing transactions does **not** contribute to the parachain state.
/// However, the host environment still expects this function to exist,
/// so we provide a no-op implementation.
#[cfg(feature = "transaction-index")]
fn host_transaction_index_index(_extrinsic: u32, _size: u32, _context_hash: [u8; 32]) {
	// No-op host function used during parachain validation.
}

/// Parachain validation does not require maintaining a transaction index,
/// and indexing transactions does **not** contribute to the parachain state.
/// However, the host environment still expects this function to exist,
/// so we provide a no-op implementation.
#[cfg(feature = "transaction-index")]
fn host_transaction_index_renew(_extrinsic: u32, _context_hash: [u8; 32]) {
	// No-op host function used during parachain validation.
}

#[cfg(test)]
mod tests {
	use super::*;
	use mmr_lib::{
		leaf_index_to_pos,
		util::{MemMMR, MemStore},
	};
	use polkadot_parachain_primitives::primitives::ValidationResultExtension;
	use polkadot_primitives::v9::{LateBlockProof, SubtreeExtension};
	use sp_core::H256;

	/// Build a `LateBlockProof` whose subtree MMR (under `SpecMerge`) was extended
	/// from `old_count` to `new_count` leaves, with a real `mmr_lib` incremental proof.
	fn build_late_block_proof(
		source: polkadot_primitives::Id,
		old_count: u64,
		new_count: u64,
	) -> (H256, H256, LateBlockProof) {
		let store = MemStore::<H256>::default();
		let mut mmr = MemMMR::<H256, SpecMerge>::new(0, &store);
		let leaves: Vec<H256> =
			(0..new_count).map(|i| H256::repeat_byte((i as u8).wrapping_add(1))).collect();

		for &l in leaves.iter().take(old_count as usize) {
			mmr.push(l).unwrap();
		}
		let old_subtree_root = mmr.get_root().unwrap();

		let mut incremental = Vec::new();
		for &l in leaves.iter().take(new_count as usize).skip(old_count as usize) {
			mmr.push(l).unwrap();
			incremental.push(l);
		}
		let new_subtree_root = mmr.get_root().unwrap();

		let positions: Vec<u64> = (old_count..new_count).map(leaf_index_to_pos).collect();
		let proof = mmr.gen_proof(positions).unwrap();

		let lbp = LateBlockProof {
			source,
			old_subtree_root,
			new_subtree_root,
			subtree_extension: Some(SubtreeExtension {
				new_mmr_size: mmr.mmr_size(),
				proof: proof.proof_items().to_vec(),
				incremental,
			}),
		};
		(old_subtree_root, new_subtree_root, lbp)
	}

	#[test]
	fn apply_messaging_proofs_transforms_requires_on_valid_proof() {
		let source: polkadot_primitives::Id = 1000u32.into();
		let (old_root, new_root, proof) = build_late_block_proof(source, 2, 3);

		let mut ext = Some(ValidationResultExtension::V4 {
			provides: None,
			requires: alloc::vec![(source, old_root)],
		});

		apply_messaging_proofs(&mut ext, alloc::vec![proof]);

		if let Some(ValidationResultExtension::V4 { ref requires, .. }) = ext {
			assert_eq!(
				requires[0].1, new_root,
				"requires should be updated to the new subtree root"
			);
		} else {
			panic!("extension should remain V4");
		}
	}

	#[test]
	fn apply_messaging_proofs_does_not_transform_on_invalid_extension() {
		let source: polkadot_primitives::Id = 1000u32.into();
		let (old_root, _, mut proof) = build_late_block_proof(source, 2, 3);

		// Tamper an appended leaf so `verify_incremental` fails.
		if let Some(ref mut ext) = proof.subtree_extension {
			ext.incremental[0] = H256::repeat_byte(0xAB);
		}

		let mut ext = Some(ValidationResultExtension::V4 {
			provides: None,
			requires: alloc::vec![(source, old_root)],
		});

		apply_messaging_proofs(&mut ext, alloc::vec![proof]);

		if let Some(ValidationResultExtension::V4 { ref requires, .. }) = ext {
			assert_eq!(requires[0].1, old_root, "requires should NOT be updated");
		} else {
			panic!("extension should remain V4");
		}
	}

	#[test]
	fn apply_messaging_proofs_identical_roots_need_no_extension() {
		let source: polkadot_primitives::Id = 1000u32.into();
		let root = H256::repeat_byte(7);
		let proof = LateBlockProof {
			source,
			old_subtree_root: root,
			new_subtree_root: root,
			subtree_extension: None,
		};

		let mut ext = Some(ValidationResultExtension::V4 {
			provides: None,
			requires: alloc::vec![(source, root)],
		});

		apply_messaging_proofs(&mut ext, alloc::vec![proof]);

		if let Some(ValidationResultExtension::V4 { ref requires, .. }) = ext {
			assert_eq!(requires[0].1, root);
		} else {
			panic!("extension should remain V4");
		}
	}
}
