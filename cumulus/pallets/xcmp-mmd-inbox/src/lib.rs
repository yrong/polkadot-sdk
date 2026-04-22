// Copyright (C) Parity Technologies (UK) Ltd.
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

//! # XCMP MMD Inbox Pallet
//!
//! This pallet receives and verifies cross-chain messages using the XCMP MMD protocol.
//!
//! ## Overview
//!
//! The pallet provides an extrinsic `submit_xcmp_mmd` that accepts messages with proofs.
//! Each message undergoes 8 verification steps:
//!
//! 1. Get relay MMR root from RelayChainStateProof
//! 2. Verify relay MMR proof and extract ParaHeadsRoot
//! 3. Verify para-heads proof against ParaHeadsRoot
//! 4. Extract source outbox MMR root from header digest
//! 5. Verify outbox MMR proof
//! 6. Verify payload hash and destination
//! 7. Replay protection check
//! 8. Dispatch to XcmpMessageHandler
//!
//! ## Storage
//!
//! - `SeenMessages`: Tracks processed messages for replay protection

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use pallet::*;

pub mod types;
pub mod verification;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use alloc::vec::Vec;
	use cumulus_primitives_core::XcmpMessageHandler;
	use frame_support::{pallet_prelude::*, traits::Get};
	use frame_system::pallet_prelude::*;
	use polkadot_parachain_primitives::primitives::Id as ParaId;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + cumulus_pallet_parachain_system::Config {
		/// Handler for dispatching verified XCMP messages.
		type XcmpMessageHandler: cumulus_primitives_core::XcmpMessageHandler;

		/// This parachain's ID.
		#[pallet::constant]
		type SelfParaId: Get<ParaId>;

		/// Maximum number of relay MMR proof items.
		#[pallet::constant]
		type MaxRelayMmrProofItems: Get<u32>;

		/// Maximum number of para-heads proof items.
		#[pallet::constant]
		type MaxParaHeadsProofItems: Get<u32>;

		/// Maximum number of outbox MMR proof items.
		#[pallet::constant]
		type MaxOutboxMmrProofItems: Get<u32>;

		/// Maximum payload size in bytes.
		#[pallet::constant]
		type MaxPayloadBytes: Get<u32>;
	}

	/// Tracks seen messages for replay protection.
	/// Maps (source_para_id, mmr_leaf_index) -> ()
	#[pallet::storage]
	pub type SeenMessages<T: Config> = StorageMap<_, Twox64Concat, (u32, u64), ()>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A message was successfully verified and dispatched.
		MessageDispatched {
			source: ParaId,
			mmr_leaf_index: u64,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Failed to read relay MMR root from state proof.
		FailedToReadRelayMmrRoot,
		/// Relay MMR proof verification failed.
		InvalidRelayMmrProof,
		/// Para-heads proof verification failed.
		InvalidParaHeadsProof,
		/// Failed to decode source parachain header.
		FailedToDecodeSourceHeader,
		/// Failed to extract outbox MMR root from source header digest.
		FailedToExtractOutboxMmrRoot,
		/// Outbox MMR proof verification failed.
		InvalidOutboxMmrProof,
		/// Payload hash mismatch.
		PayloadHashMismatch,
		/// Destination mismatch.
		DestinationMismatch,
		/// Message already seen (replay protection).
		MessageAlreadySeen,
		/// Payload exceeds maximum size.
		PayloadTooLarge,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit one or more XCMP messages with proofs for verification and dispatch.
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::from_parts(10_000, 0))] // TODO: proper weight calculation
		pub fn submit_xcmp_mmd(
			origin: OriginFor<T>,
			messages: Vec<types::MessageWithProof>,
		) -> DispatchResult {
			ensure_signed(origin)?;

			for message in messages {
				Self::verify_and_dispatch_message(message)?;
			}

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Verify and dispatch a single message.
		fn verify_and_dispatch_message(message: types::MessageWithProof) -> DispatchResult {
			log::debug!(
				target: "xcmp-mmd-inbox",
				"Verifying message from {:?}, leaf_index: {}",
				message.source,
				message.mmr_leaf_index
			);

			// Check payload size
			ensure!(
				message.payload.len() <= T::MaxPayloadBytes::get() as usize,
				Error::<T>::PayloadTooLarge
			);

			// Step 1: Get relay MMR root from RelayChainStateProof
			let relay_mmr_root = verification::read_mmr_root_from_relay_proof::<T>()?;

			// Step 2: Verify relay MMR proof and extract ParaHeadsRoot
			let para_heads_root = verification::verify_relay_mmr_proof::<T>(
				relay_mmr_root,
				message.relay_mmr_leaf_index,
				message.relay_mmr_size,
				&message.relay_mmr_leaf,
				&message.relay_mmr_proof,
			)?;

			// Step 3: Verify para-heads proof against ParaHeadsRoot
			let source_header_bytes = verification::verify_para_heads_proof::<T>(
				para_heads_root,
				message.source.into(),
				&message.para_heads_proof,
			)?;

			// Step 4: Extract source outbox MMR root from header digest
			let source_header = verification::decode_source_header::<T>(&source_header_bytes)?;
			let outbox_mmr_root = verification::extract_outbox_mmr_root::<T>(&source_header)?;

			// Step 5: Verify outbox MMR proof
			verification::verify_outbox_mmr_proof::<T>(
				outbox_mmr_root,
				message.mmr_leaf_index,
				message.outbox_mmr_size,
				&message.outbox_leaf,
				&message.outbox_mmr_proof,
			)?;

			// Step 6: Verify payload hash and destination
			verification::verify_payload_hash::<T>(&message.payload, message.outbox_leaf.payload_hash)?;
			let dest_u32: u32 = message.dest.into();
			ensure!(message.outbox_leaf.dest == dest_u32, Error::<T>::DestinationMismatch);

			// Step 7: Replay protection
			let key: (u32, u64) = (message.source.into(), message.mmr_leaf_index);
			ensure!(!SeenMessages::<T>::contains_key(key), Error::<T>::MessageAlreadySeen);
			SeenMessages::<T>::insert(key, ());

			// Step 8: Dispatch to XcmpMessageHandler
			// TODO: Get actual relay block number from relay state
			let relay_block_number = 0u32;
			let messages_iter = core::iter::once((
				message.source,
				relay_block_number,
				message.payload.as_slice(),
			));
			let _weight = <T as Config>::XcmpMessageHandler::handle_xcmp_messages(
				messages_iter,
				frame_support::weights::Weight::MAX,
			);

			// Emit event
			Self::deposit_event(Event::MessageDispatched {
				source: message.source,
				mmr_leaf_index: message.mmr_leaf_index,
			});

			Ok(())
		}
	}
}
