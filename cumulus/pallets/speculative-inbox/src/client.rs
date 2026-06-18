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

//! Client-side (node) support for speculative messaging.
//!
//! Provides the inherent data provider that injects `SpeculativeIngress` into
//! the block before proposal, and helpers to build `MessageBatch`es from a
//! sender chain's `SpeculativeOutboxApi` (direct RPC alternative to an HTTP
//! provider for the PoC).

use alloc::vec::Vec;

use cumulus_primitives_core::{ParaId, SpeculativeOutboxApi};
use cumulus_primitives_spec_messaging::{MessageBatch, OutgoingMessage, SpeculativeIngress};
use polkadot_primitives::{BlockNumber, Hash};
use sp_api::ProvideRuntimeApi;
use sp_inherents::{InherentData, InherentIdentifier};
use sp_runtime::{traits::Block as BlockT, BoundedVec};

/// The inherent identifier for speculative ingress — matches the constant
/// in the pallet runtime.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"specingr";

/// Empty ingress for blocks that do not consume off-chain message batches yet.
pub fn empty_speculative_ingress() -> SpeculativeIngress {
	SpeculativeIngress { batches: Vec::new() }
}

/// Injects `SpeculativeIngress` into the inherent data.
///
/// Called by the collator before block proposal. The `ingress` should be
/// pre-fetched and prechecked from a provider.
pub fn inject_speculative_ingress(
	inherent_data: &mut InherentData,
	ingress: SpeculativeIngress,
) -> Result<(), sp_inherents::Error> {
	inherent_data.put_data(INHERENT_IDENTIFIER, &ingress)
}

/// Build a `MessageBatch` from the sender runtime's outbox state at `at`.
///
/// Returns `None` when there are no new outbound messages for `destination`
/// starting at `from_position`, or when the outbox APIs are unavailable.
pub fn build_message_batch<Block, Client>(
	client: &Client,
	at: <Block as BlockT>::Hash,
	source: ParaId,
	destination: ParaId,
	source_relay_parent_number: BlockNumber,
	from_position: u64,
	max_messages: u32,
) -> Option<MessageBatch>
where
	Block: BlockT<Hash = Hash>,
	Client: ProvideRuntimeApi<Block>,
	Client::Api: SpeculativeOutboxApi<Block>,
{
	let api = client.runtime_api();
	// Flat commitment: the relay chain matches `subtree_root` directly, so the
	// batch carries no top-level provides root or subtree-inclusion proof.
	let (subtree_root, _) = api.destination_state(at, destination).ok()??;
	let (messages, subtree_mmr_size, messages_proof) = api
		.outbound_messages_with_proof(at, destination, from_position, max_messages)
		.ok()??;
	if messages.is_empty() {
		return None;
	}

	let batch = MessageBatch {
		source,
		source_relay_parent_number,
		subtree_root,
		subtree_mmr_size,
		messages_proof,
		messages: messages
			.into_iter()
			.map(|(position, payload)| {
				let payload = BoundedVec::try_from(payload).unwrap_or_default();
				OutgoingMessage::new(source, destination, position, payload)
			})
			.collect(),
	};

	Some(batch)
}

/// Fetch batches from one or more sender chains and assemble ingress.
///
/// Each entry is `(source_para_id, sender_client, sender_block_hash, relay_parent_number,
/// from_position)`. Failures for individual sources are skipped (collator continues
/// without that source).
pub fn fetch_speculative_ingress<Block, Client>(
	sources: &[(ParaId, &Client, <Block as BlockT>::Hash, BlockNumber, u64)],
	destination: ParaId,
	max_messages_per_source: u32,
) -> SpeculativeIngress
where
	Block: BlockT<Hash = Hash>,
	Client: ProvideRuntimeApi<Block>,
	Client::Api: SpeculativeOutboxApi<Block>,
{
	let mut batches = Vec::new();
	for (source, client, at, relay_parent_number, from_position) in sources {
		if let Some(batch) = build_message_batch::<Block, Client>(
			client,
			*at,
			*source,
			destination,
			*relay_parent_number,
			*from_position,
			max_messages_per_source,
		) {
			batches.push(batch);
		}
	}
	SpeculativeIngress { batches }
}
