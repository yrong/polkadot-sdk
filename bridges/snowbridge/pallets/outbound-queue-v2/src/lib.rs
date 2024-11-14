// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Pallet for committing outbound messages for delivery to Ethereum
//!
//! # Overview
//!
//! Messages come either from sibling parachains via XCM, or BridgeHub itself
//! via the `snowbridge-pallet-system`:
//!
//! 1. `snowbridge_router_primitives::outbound::v2::EthereumBlobExporter::deliver`
//! 2. `snowbridge_pallet_system::Pallet::send_v2`
//!
//! The message submission pipeline works like this:
//! 1. The message is first validated via the implementation for
//!    [`snowbridge_core::outbound::v2::SendMessage::validate`]
//! 2. The message is then enqueued for later processing via the implementation for
//!    [`snowbridge_core::outbound::v2::SendMessage::deliver`]
//! 3. The underlying message queue is implemented by [`Config::MessageQueue`]
//! 4. The message queue delivers messages back to this pallet via the implementation for
//!    [`frame_support::traits::ProcessMessage::process_message`]
//! 5. The message is processed in `Pallet::do_process_message`: a. Assigned a nonce b. ABI-encoded,
//!    hashed, and stored in the `MessageLeaves` vector
//! 6. At the end of the block, a merkle root is constructed from all the leaves in `MessageLeaves`.
//! 7. This merkle root is inserted into the parachain header as a digest item
//! 8. Offchain relayers are able to relay the message to Ethereum after: a. Generating a merkle
//!    proof for the committed message using the `prove_message` runtime API b. Reading the actual
//!    message content from the `Messages` vector in storage
//!
//! On the Ethereum side, the message root is ultimately the thing being
//! verified by the Polkadot light client.
//!
//! # Message Priorities
//!
//! The processing of governance commands can never be halted. This effectively
//! allows us to pause processing of normal user messages while still allowing
//! governance commands to be sent to Ethereum.
//!
//! # Extrinsics
//!
//! * [`Call::set_operating_mode`]: Set the operating mode
//!
//! # Runtime API
//!
//! * `prove_message`: Generate a merkle proof for a committed message
//! * `dry_run`: Convert xcm to InboundMessage
#![cfg_attr(not(feature = "std"), no_std)]
pub mod api;
pub mod envelope;
pub mod process_message_impl;
pub mod send_message_impl;
pub mod types;
pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod test;

use alloy_primitives::FixedBytes;
use alloy_sol_types::SolValue;
use bridge_hub_common::{AggregateMessageOrigin, CustomDigestItem};
use codec::Decode;
use envelope::Envelope;
use frame_support::{
	storage::StorageStreamIter,
	traits::{tokens::Balance, EnqueueMessage, Get, ProcessMessageError},
	weights::{Weight, WeightToFee},
};
pub use pallet::*;
use snowbridge_core::{
	inbound::{Message as DeliveryMessage, VerificationError, Verifier},
	outbound::v2::{CommandWrapper, Fee, GasMeter, InboundMessage, InboundMessageWrapper, Message},
	BasicOperatingMode, RewardLedger, TokenId,
};
use snowbridge_merkle_tree::merkle_root;
use sp_core::{H160, H256};
use sp_runtime::{
	traits::{BlockNumberProvider, Hash, MaybeEquivalence},
	ArithmeticError, DigestItem,
};
use sp_std::prelude::*;
pub use types::{PendingOrder, ProcessMessageOriginOf};
pub use weights::WeightInfo;
use xcm::prelude::{Location, NetworkId};

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		type Hashing: Hash<Output = H256>;

		type MessageQueue: EnqueueMessage<AggregateMessageOrigin>;

		/// Measures the maximum gas used to execute a command on Ethereum
		type GasMeter: GasMeter;

		type Balance: Balance + From<u128>;

		/// Max bytes in a message payload
		#[pallet::constant]
		type MaxMessagePayloadSize: Get<u32>;

		/// Max number of messages processed per block
		#[pallet::constant]
		type MaxMessagesPerBlock: Get<u32>;

		/// Convert a weight value into a deductible fee based.
		type WeightToFee: WeightToFee<Balance = Self::Balance>;

		/// Weight information for extrinsics in this pallet
		type WeightInfo: WeightInfo;

		/// The verifier for delivery proof from Ethereum
		type Verifier: Verifier;

		/// Address of the Gateway contract
		#[pallet::constant]
		type GatewayAddress: Get<H160>;

		/// Reward leger
		type RewardLedger: RewardLedger<<Self as frame_system::Config>::AccountId, Self::Balance>;

		type ConvertAssetId: MaybeEquivalence<TokenId, Location>;

		type EthereumNetwork: Get<NetworkId>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Message has been queued and will be processed in the future
		MessageQueued {
			/// The message
			message: Message,
		},
		/// Message will be committed at the end of current block. From now on, to track the
		/// progress the message, use the `nonce` of `id`.
		MessageAccepted {
			/// ID of the message
			id: H256,
			/// The nonce assigned to this message
			nonce: u64,
		},
		/// Some messages have been committed
		MessagesCommitted {
			/// Merkle root of the committed messages
			root: H256,
			/// number of committed messages
			count: u64,
		},
		/// Set OperatingMode
		OperatingModeChanged { mode: BasicOperatingMode },
		/// Delivery Proof received
		MessageDeliveryProofReceived { nonce: u64 },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The message is too large
		MessageTooLarge,
		/// The pallet is halted
		Halted,
		/// Invalid Channel
		InvalidChannel,
		/// Invalid Envelope
		InvalidEnvelope,
		/// Message verification error
		Verification(VerificationError),
		/// Invalid Gateway
		InvalidGateway,
		/// No pending nonce
		PendingNonceNotExist,
	}

	/// Messages to be committed in the current block. This storage value is killed in
	/// `on_initialize`, so should never go into block PoV.
	///
	/// Is never read in the runtime, only by offchain message relayers.
	///
	/// Inspired by the `frame_system::Pallet::Events` storage value
	#[pallet::storage]
	#[pallet::unbounded]
	pub(super) type Messages<T: Config> = StorageValue<_, Vec<InboundMessage>, ValueQuery>;

	/// Hashes of the ABI-encoded messages in the [`Messages`] storage value. Used to generate a
	/// merkle root during `on_finalize`. This storage value is killed in
	/// `on_initialize`, so should never go into block PoV.
	#[pallet::storage]
	#[pallet::unbounded]
	#[pallet::getter(fn message_leaves)]
	pub(super) type MessageLeaves<T: Config> = StorageValue<_, Vec<H256>, ValueQuery>;

	/// The current nonce for the messages
	#[pallet::storage]
	pub type Nonce<T: Config> = StorageValue<_, u64, ValueQuery>;

	/// The current operating mode of the pallet.
	#[pallet::storage]
	#[pallet::getter(fn operating_mode)]
	pub type OperatingMode<T: Config> = StorageValue<_, BasicOperatingMode, ValueQuery>;

	/// Pending orders to relay
	#[pallet::storage]
	pub type PendingOrders<T: Config> =
		StorageMap<_, Identity, u64, PendingOrder<BlockNumberFor<T>>, OptionQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T>
	where
		T::AccountId: AsRef<[u8]>,
	{
		fn on_initialize(_: BlockNumberFor<T>) -> Weight {
			// Remove storage from previous block
			Messages::<T>::kill();
			MessageLeaves::<T>::kill();
			// Reserve some weight for the `on_finalize` handler
			T::WeightInfo::commit()
		}

		fn on_finalize(_: BlockNumberFor<T>) {
			Self::commit();
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Halt or resume all pallet operations. May only be called by root.
		#[pallet::call_index(0)]
		#[pallet::weight((T::DbWeight::get().reads_writes(1, 1), DispatchClass::Operational))]
		pub fn set_operating_mode(
			origin: OriginFor<T>,
			mode: BasicOperatingMode,
		) -> DispatchResult {
			ensure_root(origin)?;
			OperatingMode::<T>::put(mode);
			Self::deposit_event(Event::OperatingModeChanged { mode });
			Ok(())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::submit_delivery_proof())]
		pub fn submit_delivery_proof(
			origin: OriginFor<T>,
			message: DeliveryMessage,
		) -> DispatchResult {
			ensure_signed(origin)?;
			ensure!(!Self::operating_mode().is_halted(), Error::<T>::Halted);

			// submit message to verifier for verification
			T::Verifier::verify(&message.event_log, &message.proof)
				.map_err(|e| Error::<T>::Verification(e))?;

			// Decode event log into an Envelope
			let envelope = Envelope::<T>::try_from(&message.event_log)
				.map_err(|_| Error::<T>::InvalidEnvelope)?;

			// Verify that the message was submitted from the known Gateway contract
			ensure!(T::GatewayAddress::get() == envelope.gateway, Error::<T>::InvalidGateway);

			let nonce = envelope.nonce;
			ensure!(<PendingOrders<T>>::contains_key(nonce), Error::<T>::PendingNonceNotExist);

			let order = <PendingOrders<T>>::get(nonce).ok_or(Error::<T>::PendingNonceNotExist)?;

			// No fee for governance order
			if !order.fee.is_zero() {
				T::RewardLedger::deposit(envelope.reward_address, order.fee.into())?;
			}

			<PendingOrders<T>>::remove(nonce);

			Self::deposit_event(Event::MessageDeliveryProofReceived { nonce });

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Generate a messages commitment and insert it into the header digest
		pub(crate) fn commit() {
			let count = MessageLeaves::<T>::decode_len().unwrap_or_default() as u64;
			if count == 0 {
				return
			}

			// Create merkle root of messages
			let root = merkle_root::<<T as Config>::Hashing, _>(MessageLeaves::<T>::stream_iter());

			let digest_item: DigestItem = CustomDigestItem::SnowbridgeV2(root).into();

			// Insert merkle root into the header digest
			<frame_system::Pallet<T>>::deposit_log(digest_item);

			Self::deposit_event(Event::MessagesCommitted { root, count });
		}

		/// Process a message delivered by the MessageQueue pallet
		pub(crate) fn do_process_message(
			_: ProcessMessageOriginOf<T>,
			mut message: &[u8],
		) -> Result<bool, ProcessMessageError> {
			use ProcessMessageError::*;

			// Yield if the maximum number of messages has been processed this block.
			// This ensures that the weight of `on_finalize` has a known maximum bound.
			ensure!(
				MessageLeaves::<T>::decode_len().unwrap_or(0) <
					T::MaxMessagesPerBlock::get() as usize,
				Yield
			);

			// Decode bytes into versioned message
			let message: Message = Message::decode(&mut message).map_err(|_| Corrupt)?;

			let nonce = Nonce::<T>::get();

			let commands: Vec<CommandWrapper> = message
				.commands
				.into_iter()
				.map(|command| CommandWrapper {
					kind: command.index(),
					gas: T::GasMeter::maximum_dispatch_gas_used_at_most(&command),
					payload: command.abi_encode(),
				})
				.collect();

			// Construct the final committed message
			let inbound_message = InboundMessage {
				origin: message.origin,
				nonce,
				commands: commands.clone().try_into().map_err(|_| Corrupt)?,
			};

			let committed_message = InboundMessageWrapper {
				origin: FixedBytes::from(message.origin.as_fixed_bytes()),
				nonce,
				commands,
			};

			// ABI-encode and hash the prepared message
			let message_abi_encoded = committed_message.abi_encode();
			let message_abi_encoded_hash = <T as Config>::Hashing::hash(&message_abi_encoded);

			Messages::<T>::append(Box::new(inbound_message));
			MessageLeaves::<T>::append(message_abi_encoded_hash);

			<PendingOrders<T>>::try_mutate(nonce, |maybe_locked| -> DispatchResult {
				let mut locked = maybe_locked.clone().unwrap_or_else(|| PendingOrder {
					nonce,
					fee: 0,
					block_number: frame_system::Pallet::<T>::current_block_number(),
				});
				locked.fee =
					locked.fee.checked_add(message.fee).ok_or(ArithmeticError::Overflow)?;
				*maybe_locked = Some(locked);
				Ok(())
			})
			.map_err(|_| Unsupported)?;

			Nonce::<T>::set(nonce.checked_add(1).ok_or(Unsupported)?);

			Self::deposit_event(Event::MessageAccepted { id: message.id, nonce });

			Ok(true)
		}

		/// The local component of the message processing fees in native currency
		pub(crate) fn calculate_local_fee() -> T::Balance {
			T::WeightToFee::weight_to_fee(
				&T::WeightInfo::do_process_message().saturating_add(T::WeightInfo::commit_single()),
			)
		}
	}
}