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
//! message ordering, continuity), updates `IncomingState`, records consumed source roots,
//! and dispatches payloads through the existing XCMP handler.
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

use codec::{Decode, Encode};
use scale_info::TypeInfo;
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
use polkadot_primitives::v9::{RequiresCommitment, SpeculativeIngress};

use mmr_lib::{Merge, Result as MmrResult};

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

/// Per-source tracking for incoming speculative messages.
#[derive(Clone, Encode, Decode, TypeInfo, Default)]
pub struct SourceState {
	/// Last processed message leaf index in the source's per-destination subtree.
	pub last_processed: u64,
	/// The source's top-level provides root for the latest batch we accepted.
	pub last_seen_provides_root: H256,
	/// The source's subtree root we last accepted.
	pub last_seen_subtree_root: H256,
	/// The current number of nodes in the receiver-local subtree MMR.
	pub mmr_size: u64,
	/// The peaks of the receiver-local subtree MMR.
	/// Storing only peaks ensures O(log N) storage per source.
	pub mmr_peaks: Vec<H256>,
}

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
		/// Reconstructed local subtree root does not match the batch's claimed subtree root.
		SubtreeRootMismatch,
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
				// 1. Verify subtree_inclusion_proof using binary-merkle-tree
				// Leaf is SCALE(destination_para_id, subtree_root)
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

				// 2. Load or init per-source state
				let mut state = IncomingState::<T>::get(&batch.source).unwrap_or_default();

				// 3. Verify message continuity and reconstruct local subtree
				for msg in &batch.messages {
					let expected_position =
						if state.mmr_size == 0 { 0 } else { state.last_processed + 1 };
					ensure!(msg.position == expected_position, Error::<T>::NonConsecutiveMessage);
					let msg_hash = Keccak256::hash(&msg.payload);
					state.mmr_peaks = append_leaf_to_peaks::<Keccak256Merge>(
						state.mmr_peaks,
						state.mmr_size,
						msg_hash,
					);
					state.mmr_size += 1;
					state.last_processed = msg.position;
				}

				// 4. Verify reconstructed MMR subtree root matches batch
				let computed_root = bag_peaks::<Keccak256Merge>(&state.mmr_peaks);
				if computed_root != batch.subtree_root {
					log::warn!(
						target: "speculative::inbox",
						"SubtreeRootMismatch for source={:?}: computed={:?} expected={:?}",
						batch.source, computed_root, batch.subtree_root,
					);
				}
				ensure!(computed_root == batch.subtree_root, Error::<T>::SubtreeRootMismatch);
				// 5. Enforce one distinct top-level provides root per source per block.
				// Gate on whether this source was already consumed in this block — not on
				// last_processed, which would skip the check for the very first message.
				if consumed.iter().any(|(source, _)| source == &batch.source) {
					ensure!(
						state.last_seen_provides_root == batch.provides_root,
						Error::<T>::MultipleRootsPerSourceInOneBlock,
					);
				}

				// 6. Update state
				state.last_seen_provides_root = batch.provides_root;
				state.last_seen_subtree_root = batch.subtree_root;
				IncomingState::<T>::insert(batch.source, state);
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
		IncomingState::<T>::get(&source)
			.map(|state| if state.mmr_size == 0 { 0 } else { state.last_processed + 1 })
			.unwrap_or(0)
	}

	/// Last seen provides root from `source`.
	pub fn last_seen_provides_root(source: ParaId) -> H256 {
		IncomingState::<T>::get(&source)
			.map(|state| state.last_seen_provides_root)
			.unwrap_or_default()
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

/// Verify that an MMR root R_old is an ancestor of R_new using an extension proof.
///
/// Replays the leaf appends in `ext.connecting_nodes` starting from `ext.old_peaks`
/// at `ext.old_leaf_count`. The resulting peaks must match `ext.new_peaks` and bag to `new_root`.
fn verify_mmr_extension(
	old_root: H256,
	new_root: H256,
	ext: &polkadot_primitives::v9::MMRExtensionProof,
) -> bool {
	if ext.old_peaks.is_empty() || ext.new_peaks.is_empty() || ext.connecting_nodes.is_empty() {
		return false;
	}
	let old_computed = bag_peaks::<Keccak256Merge>(&ext.old_peaks);
	if old_computed != old_root {
		return false;
	}
	// Replay each leaf append and confirm we arrive at ext.new_peaks.
	let mut peaks = ext.old_peaks.clone();
	let mut size = ext.old_leaf_count;
	for &leaf_hash in &ext.connecting_nodes {
		peaks = append_leaf_to_peaks::<Keccak256Merge>(peaks, size, leaf_hash);
		size += 1;
	}
	if peaks != ext.new_peaks {
		return false;
	}
	bag_peaks::<Keccak256Merge>(&ext.new_peaks) == new_root
}

/// Bag MMR peaks into a single root hash using mmr_lib's canonical merge order:
/// merge(right_peak, next_left_peak) from right to left.
fn bag_peaks<M: Merge>(peaks: &[M::Item]) -> M::Item
where
	M::Item: Default + Clone,
{
	if peaks.is_empty() {
		return Default::default();
	}
	let mut current = peaks.last().unwrap().clone();
	for peak in peaks[..peaks.len() - 1].iter().rev() {
		current = M::merge(&current, peak).unwrap_or(current);
	}
	current
}

fn append_leaf_to_peaks<M: Merge>(
	mut peaks: Vec<M::Item>,
	size: u64,
	leaf: M::Item,
) -> Vec<M::Item> {
	let mut current = leaf;
	let mut current_size = size;
	while current_size % 2 == 1 {
		if let Some(last_peak) = peaks.pop() {
			current = M::merge(&last_peak, &current).unwrap_or(current);
		}
		current_size /= 2;
	}
	peaks.push(current);
	peaks
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::H256;
	use sp_runtime::traits::Keccak256;

	#[test]
	fn test_mmr_root_single_leaf() {
		let leaf = Keccak256::hash(b"msg1");
		let peaks = append_leaf_to_peaks::<Keccak256Merge>(Vec::new(), 0, leaf);
		let root = bag_peaks::<Keccak256Merge>(&peaks);
		assert_eq!(root, leaf);
	}

	#[test]
	fn test_mmr_root_matches_after_multiple_pushes() {
		let leaves: Vec<H256> = (0..11u8).map(|i| Keccak256::hash(&[i])).collect();

		let store = mmr_lib::util::MemStore::<H256>::default();
		let mut mmr = mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);

		let mut peaks = Vec::new();
		let mut size = 0;
		for leaf in &leaves {
			mmr.push(*leaf).unwrap();
			peaks = append_leaf_to_peaks::<Keccak256Merge>(peaks, size, *leaf);
			size += 1;
		}
		let incremental_root = mmr.get_root().unwrap();

		let peak_root = bag_peaks::<Keccak256Merge>(&peaks);
		assert_eq!(peak_root, incremental_root);
	}

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
