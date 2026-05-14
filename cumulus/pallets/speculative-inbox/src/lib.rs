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

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_inherents::InherentIdentifier;
use sp_runtime::{
	traits::{Hash as _, Keccak256},
	DispatchResult,
};

use frame_support::pallet_prelude::*;
use frame_system::{ensure_none, pallet_prelude::{BlockNumberFor, OriginFor}};

use cumulus_primitives_core::ParaId;
use polkadot_parachain_primitives::primitives::{
	XcmpMessageFormat, XcmpMessageHandler,
};
use polkadot_primitives::v10::{RequiresCommitment, SpeculativeIngress};

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
	/// Accumulated leaf hashes for the receiver-local subtree.
	/// The receiver verifies that the batch's subtree_root matches the Merkle root
	/// computed from these leaves.
	///
	/// TODO: MMR Storage Efficiency. For the PoC, we store all leaves; a production
	/// implementation must use an MMR storage adapter that stores only the peaks and
	/// the current leaf count to avoid unbounded storage growth.
	pub local_subtree_leaves: Vec<H256>,
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
		MessagesIngested {
			source: ParaId,
			provides_root: H256,
			message_count: u32,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The subtree inclusion proof did not verify against the provides root.
		InvalidSubtreeProof,
		/// Messages are not consecutive (gap or reorder from last processed + 1).
		NonConsecutiveMessage,
		/// Reconstructed local subtree root does not match the batch's claimed subtree root.
		SubtreeRootMismatch,
		/// The XCMP message format in an incoming batch payload is unrecognized.
		/// This may happen if the sender's or receiver's XcmpMessageHandler is misconfigured.
		InvalidXcmpMessageFormat,
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
	pub type ConsumedSourcesThisBlock<T: Config> =
		StorageValue<_, Vec<(ParaId, H256)>, ValueQuery>;

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
				ensure!(valid, Error::<T>::InvalidSubtreeProof);

				// 2. Load or init per-source state
				let mut state =
					IncomingState::<T>::get(&batch.source).unwrap_or_default();

				// 3. Verify message continuity and reconstruct local subtree
				for msg in &batch.messages {
					ensure!(
						msg.position == state.last_processed + 1,
						Error::<T>::NonConsecutiveMessage
					);
					let msg_hash = Keccak256::hash(&msg.payload);
					state.local_subtree_leaves.push(msg_hash);
					state.last_processed = msg.position;
				}

				// 4. Verify reconstructed MMR subtree root matches batch
				let computed_root = compute_mmr_root(&state.local_subtree_leaves);
				ensure!(
					computed_root == batch.subtree_root,
					Error::<T>::SubtreeRootMismatch
				);

				// 5. Enforce one distinct top-level provides root per source per block
				if state.last_processed > 0 {
					ensure!(
						state.last_seen_provides_root == batch.provides_root
							|| !consumed.iter().any(|(source, _)| source == &batch.source),
						Error::<T>::MultipleRootsPerSourceInOneBlock,
					);
				}

				// 6. Update state
				state.last_seen_provides_root = batch.provides_root;
				state.last_seen_subtree_root = batch.subtree_root;
				IncomingState::<T>::insert(batch.source, state);
				consumed.push((batch.source, batch.provides_root));

				// 7. Dispatch through the standard XCMP handler
				let encoded_batch = encode_xcmp_batch(
					batch.messages.iter().map(|msg| msg.payload.as_slice()),
				);
				let max_weight = T::ReservedXcmpWeight::get();
				T::XcmpMessageHandler::handle_xcmp_messages(
					core::iter::once((
						batch.source,
						batch.source_relay_parent_number,
						encoded_batch.as_slice(),
					)),
					max_weight,
				);

				Self::deposit_event(Event::MessagesIngested {
					source: batch.source,
					provides_root: batch.provides_root,
					message_count: batch.messages.len() as u32,
				});
			}

			ConsumedSourcesThisBlock::<T>::put(consumed);
			Ok(())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = sp_inherents::MakeFatalError<()>;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(data: &sp_inherents::InherentData) -> Option<Self::Call> {
			let ingress = data
				.get_data::<SpeculativeIngress>(&Self::INHERENT_IDENTIFIER)
				.ok()
				.flatten()?;
			Some(Call::ingest_verified_messages { ingress })
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::ingest_verified_messages { .. })
		}
	}
}

impl<T: Config> Pallet<T> {
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

/// Encode a batch of XCM payloads in the wire format expected by `handle_xcmp_messages`.
fn encode_xcmp_batch<'a>(payloads: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
	let mut page = XcmpMessageFormat::ConcatenatedVersionedXcm.encode();
	for payload in payloads {
		page.extend_from_slice(payload);
	}
	page
}

/// Compute the MMR root from an ordered list of leaf hashes.
///
/// Uses `mmr_lib::MemMMR` to accumulate leaves exactly the same way the sender
/// builds its per-destination subtree MMR. Both sides use the same `Keccak256Merge`,
/// so the receiver can independently reconstruct and verify the root against the
/// batch's `subtree_root`.
///
/// Uses `MemMMR` — which buffers writes in memory and computes the root from the
/// merged peak set — rather than `MerkleProof::calculate_root`, because the
/// receiver does not receive per-message MMR proofs, only the ordered message
/// payloads and the claimed `subtree_root`.
///
/// Note: this rebuilds the MMR from scratch each time (O(n) in total accumulated
/// messages). A production implementation should track the MMR state incrementally
/// in runtime storage (e.g., `StorageMap<u64, H256>` for MMR nodes + `mmr_size`)
/// and only push the new messages.
fn compute_mmr_root(leaves: &[H256]) -> H256 {
	if leaves.is_empty() {
		return H256::zero();
	}
	let store = mmr_lib::util::MemStore::<H256>::default();
	let mut mmr =
		mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
	for leaf in leaves {
		let _ = mmr.push(*leaf);
	}
	mmr.get_root().unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use super::*;
	use mmr_lib::helper::leaf_index_to_pos;
	use sp_core::H256;
	use sp_mmr_primitives::utils::NodesUtils;
	use sp_runtime::traits::Keccak256;

	#[test]
	fn test_mmr_root_single_leaf() {
		let leaf = Keccak256::hash(b"msg1");
		assert_eq!(compute_mmr_root(&[leaf]), leaf);
	}

	#[test]
	fn test_mmr_root_two_leaves_matches_merkle_proof() {
		// Rebuild the MMR from two leaves.
		let leaf1 = Keccak256::hash(b"msg1");
		let leaf2 = Keccak256::hash(b"msg2");
		let rebuilt_root = compute_mmr_root(&[leaf1, leaf2]);

		// Now verify independently: generate a MerkleProof for leaf 1
		// and use `calculate_root` (the same pattern as `verify_relay_mmr_proof`).
		let leaf_count: u64 = 2;
		let mmr_node_size = NodesUtils::new(leaf_count).size();
		let leaf_pos = leaf_index_to_pos(1); // second leaf (0-based index 1)

		// Build the MMR once to extract the proof for the second leaf.
		let store = mmr_lib::util::MemStore::<H256>::default();
		let mut mmr =
			mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
		let pos0 = mmr.push(leaf1).unwrap();
		let pos1 = mmr.push(leaf2).unwrap();
		let root_via_get_root = mmr.get_root().unwrap();
		assert_eq!(rebuilt_root, root_via_get_root);

		let proof = mmr.gen_proof(vec![pos1]).unwrap();
		let proof_items = proof.proof_items().to_vec();

		// Verify using MerkleProof::calculate_root — the same pattern
		// used in the reference `verify_relay_mmr_proof`.
		let merkle_proof =
			mmr_lib::MerkleProof::<H256, Keccak256Merge>::new(
				mmr_node_size,
				proof_items,
			);
		let calculated = merkle_proof
			.calculate_root(vec![(leaf_pos, leaf2)])
			.unwrap();

		assert_eq!(calculated, rebuilt_root);
	}

	#[test]
	fn test_mmr_root_matches_after_multiple_pushes() {
		// Regression: rebuilding from all leaves must match incremental push root.
		let leaves: Vec<H256> = (0..11u8)
			.map(|i| Keccak256::hash(&[i]))
			.collect();

		let store = mmr_lib::util::MemStore::<H256>::default();
		let mut mmr =
			mmr_lib::util::MemMMR::<H256, Keccak256Merge>::new(0, &store);
		for leaf in &leaves {
			mmr.push(*leaf).unwrap();
		}
		let incremental_root = mmr.get_root().unwrap();

		let rebuilt_root = compute_mmr_root(&leaves);
		assert_eq!(rebuilt_root, incremental_root);
		assert_ne!(rebuilt_root, H256::zero());
	}

	#[test]
	fn test_top_level_merkle_proof_roundtrip() {
		// The top-level tree is a plain binary Merkle tree (not MMR).
		let leaves: Vec<Vec<u8>> =
			vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];
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
