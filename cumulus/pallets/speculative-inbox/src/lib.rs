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

//! # Speculative Inbox Pallet
//!
//! Receiver-side pallet for the inclusion-based speculative messaging PoC.
//!
//! Verifies incoming `MessageBatch`es against on-chain state (subtree inclusion proof,
//! per-message MMR inclusion proof, message ordering), updates `IncomingState`,
//! records consumed source roots, and dispatches payloads through the existing XCMP
//! handler.
//!
//! Messages arrive via a mandatory inherent (`ingest_verified_messages`), so that the
//! same batches are deterministically re-validated by backing validators during PVF
//! execution. Off-chain fetch and precheck are performed by the collator before proposing
//! the block; the runtime only trusts data present in the block body.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
pub mod client;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod integration_tests;

extern crate alloc;
use alloc::vec::Vec;

use codec::Encode;
use sp_core::H256;
use sp_inherents::InherentIdentifier;
use sp_runtime::{
	traits::{Hash as _, Keccak256},
	DispatchResult,
};

use frame_support::pallet_prelude::*;
use frame_system::{
	ensure_none,
	pallet_prelude::{BlockNumberFor, OriginFor},
};

use cumulus_primitives_core::ParaId;
use polkadot_parachain_primitives::primitives::XcmpMessageHandler;
use polkadot_primitives::v9::{RequiresCommitment, SourceState, SpeculativeIngress};

use mmr_lib::{leaf_index_to_pos, Merge, MerkleProof, Result as MmrResult};

/// Keccak256 merge for MMR node construction.
struct Keccak256Merge;
impl Merge for Keccak256Merge {
	type Item = H256;
	fn merge(lhs: &Self::Item, rhs: &Self::Item) -> MmrResult<Self::Item> {
		let mut concat = [0u8; 64];
		concat[..32].copy_from_slice(lhs.as_ref());
		concat[32..].copy_from_slice(rhs.as_ref());
		Ok(Keccak256::hash(&concat))
	}
}

/// The inherent identifier for speculative ingress.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"specingr";

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The parachain's own ID.
		#[pallet::constant]
		type SelfParaId: Get<ParaId>;

		/// Handler for dispatching XCMP messages.
		type XcmpMessageHandler: XcmpMessageHandler;

		/// Reserved weight for XCMP message handling.
		#[pallet::constant]
		type ReservedXcmpWeight: Get<Weight>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Speculative messages were ingested from a source.
		MessagesIngested { source: ParaId, provides_root: H256, message_count: u32 },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The subtree inclusion proof did not verify against the provides root.
		InvalidSubtreeProof,
		/// Messages are not consecutive (gap or reorder from last processed + 1).
		NonConsecutiveMessage,
		/// The combined MMR inclusion proof did not verify against the batch's subtree root.
		InvalidMessagesProof,
		/// Multiple distinct provides roots for the same source in one block.
		MultipleRootsPerSourceInOneBlock,
	}

	/// Per-source tracking of incoming speculative messages.
	#[pallet::storage]
	pub type IncomingState<T: Config> = StorageMap<_, Twox64Concat, ParaId, SourceState>;

	/// Sources consumed during THIS block.
	/// Cleared in `on_initialize`, populated by `ingest_verified_messages`,
	/// then read after block execution to populate `CandidateCommitments.requires`.
	#[pallet::storage]
	pub type ConsumedSourcesThisBlock<T: Config> = StorageValue<_, Vec<(ParaId, H256)>, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			ConsumedSourcesThisBlock::<T>::kill();
			Weight::zero()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Ingest verified speculative message batches.
		///
		/// This is a mandatory inherent — the collator fetches batches off-chain,
		/// prechecks them locally, and embeds them in the block body. The runtime
		/// re-verifies deterministically against on-chain state.
		#[pallet::call_index(0)]
		#[pallet::weight((0, DispatchClass::Mandatory))]
		pub fn ingest_verified_messages(
			origin: OriginFor<T>,
			ingress: SpeculativeIngress,
		) -> DispatchResult {
			ensure_none(origin)?;

			// TODO: Late Block Proofs (§6.2). The current implementation does not verify
			// late block proofs from the PoV. This means the receiver will reject batches
			// if the source chain's top-level root has advanced beyond what the batch was
			// built against.

			log::debug!(
				target: "speculative::inbox",
				"ingest_verified_messages: {} batch(es)",
				ingress.batches.len(),
			);

			let mut consumed: Vec<(ParaId, H256)> = Vec::new();

			for batch in ingress.batches {
				// 1. Verify subtree_inclusion_proof against the batch's provides_root.
				// Leaf is SCALE(destination_para_id, subtree_root).
				let leaf = (T::SelfParaId::get(), batch.subtree_root).encode();
				let valid = binary_merkle_tree::verify_proof::<Keccak256, _, _>(
					&batch.provides_root,
					batch.subtree_inclusion_proof.iter().copied(),
					batch.number_of_destinations,
					batch.leaf_index,
					&leaf,
				);
				if !valid {
					log::warn!(
						target: "speculative::inbox",
						"InvalidSubtreeProof for source={:?} provides_root={:?}",
						batch.source, batch.provides_root,
					);
				}
				ensure!(valid, Error::<T>::InvalidSubtreeProof);

				// 2. Enforce one distinct top-level provides root per source per block.
				// Use the local `consumed` list — we don't need cross-block state for this:
				// the guard only matters within the current block.
				if let Some((_, prior_root)) =
					consumed.iter().find(|(source, _)| source == &batch.source)
				{
					ensure!(
						*prior_root == batch.provides_root,
						Error::<T>::MultipleRootsPerSourceInOneBlock,
					);
				}

				// 3. Load per-source state for continuity check.
				let mut next_expected = match IncomingState::<T>::get(&batch.source) {
					Some(state) => state.last_processed.saturating_add(1),
					None => 0,
				};

				// 4. Verify message continuity and collect MMR leaves for proof verification.
				let mut leaves: Vec<(u64, H256)> = Vec::with_capacity(batch.messages.len());
				for msg in &batch.messages {
					ensure!(msg.position == next_expected, Error::<T>::NonConsecutiveMessage);
					let leaf_hash = Keccak256::hash(&msg.payload);
					leaves.push((leaf_index_to_pos(msg.position), leaf_hash));
					next_expected = next_expected.saturating_add(1);
				}

				// 5. Verify the combined MMR inclusion proof against subtree_root.
				if !leaves.is_empty() {
					let proof = MerkleProof::<H256, Keccak256Merge>::new(
						batch.subtree_mmr_size,
						batch.messages_proof.clone(),
					);
					let verified =
						proof.verify(batch.subtree_root, leaves).unwrap_or(false);
					if !verified {
						log::warn!(
							target: "speculative::inbox",
							"InvalidMessagesProof for source={:?} subtree_root={:?}",
							batch.source, batch.subtree_root,
						);
					}
					ensure!(verified, Error::<T>::InvalidMessagesProof);
				}

				// 6. Update state: record last_processed if any messages were consumed.
				if let Some(last) = batch.messages.last() {
					IncomingState::<T>::insert(
						batch.source,
						SourceState { last_processed: last.position },
					);
				}
				consumed.push((batch.source, batch.provides_root));

				// 7. Dispatch through the standard XCMP handler.
				// Each payload is a full XCMP page from XcmpQueue::take_outbound_messages
				// (format_byte + versioned_xcm_bytes). Pass each page directly rather than
				// re-encoding to avoid prepending a duplicate format byte.
				let max_weight = T::ReservedXcmpWeight::get();
				for msg in &batch.messages {
					T::XcmpMessageHandler::handle_xcmp_messages(
						core::iter::once((
							batch.source,
							batch.source_relay_parent_number,
							msg.payload.as_slice(),
						)),
						max_weight,
					);
				}

				Self::deposit_event(Event::MessagesIngested {
					source: batch.source,
					provides_root: batch.provides_root,
					message_count: batch.messages.len() as u32,
				});
				log::debug!(
					target: "speculative::inbox",
					"ingested {} message(s) from source={:?} provides_root={:?}",
					batch.messages.len(), batch.source, batch.provides_root,
				);
			}

			ConsumedSourcesThisBlock::<T>::mutate(|v| v.extend(consumed));
			Ok(())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = sp_inherents::MakeFatalError<()>;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(data: &sp_inherents::InherentData) -> Option<Self::Call> {
			let ingress =
				data.get_data::<SpeculativeIngress>(&Self::INHERENT_IDENTIFIER).ok().flatten()?;
			Some(Call::ingest_verified_messages { ingress })
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::ingest_verified_messages { .. })
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Next message position the collator should fetch from `source`.
	pub fn next_expected_message_position(source: ParaId) -> u64 {
		match IncomingState::<T>::get(&source) {
			Some(state) => state.last_processed.saturating_add(1),
			None => 0,
		}
	}

	/// Get the requires commitments for this block (sources consumed + their provides roots).
	/// Called by the collator after block execution to populate `CandidateCommitments.requires`.
	pub fn get_requires_commitments() -> Vec<RequiresCommitment> {
		let mut consumed = ConsumedSourcesThisBlock::<T>::get();
		// Canonical ordering: sort by source, deduplicate
		consumed.sort_by_key(|(source, _)| *source);
		consumed.dedup_by_key(|(source, _)| *source);

		consumed
			.into_iter()
			.map(|(source, provides_root)| RequiresCommitment {
				source,
				expected_root: provides_root,
			})
			.collect()
	}
}

// ── Helpers ──

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_top_level_merkle_proof_roundtrip() {
		// The top-level tree is a plain binary Merkle tree (not MMR).
		let leaves: Vec<Vec<u8>> = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];
		let proof = binary_merkle_tree::merkle_proof::<Keccak256, _, _>(leaves.clone(), 1);
		assert!(binary_merkle_tree::verify_proof::<Keccak256, _, _>(
			&proof.root,
			proof.proof,
			leaves.len() as u32,
			proof.leaf_index,
			&proof.leaf,
		));
	}
}
