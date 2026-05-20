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

//! Speculative messaging ingress fetch for Aura collators.

use std::sync::Arc;

use super::outbox_client::{build_message_batch_from_query, OutboxQuery};
use cumulus_pallet_speculative_inbox::client::empty_speculative_ingress;
use cumulus_primitives_core::{ParaId, SpeculativeInboxApi};
use cumulus_relay_chain_interface::RelayChainInterface;
use polkadot_primitives::{v10::SpeculativeIngress, BlockNumber, Hash};
use sc_client_api::UsageProvider;
use sp_api::ProvideRuntimeApi;
use sp_runtime::traits::Block as BlockT;

/// Default cap on messages fetched per source per block.
pub const DEFAULT_MAX_MESSAGES_PER_SOURCE: u32 = 32;

/// Sender parachain clients used to build speculative ingress for this collator.
///
/// Each entry holds a type-erased [`OutboxQuery`] that can be either an in-process
/// [`crate::collators::DirectOutboxClient`] or a cross-process
/// [`crate::collators::RpcOutboxClient`].
pub struct SpeculativeMessageSources {
	/// `(source_para_id, sender_outbox_query)`.
	pub sources: Vec<(ParaId, Arc<dyn OutboxQuery>)>,
	/// Maximum messages to pull from each source per block.
	pub max_messages_per_source: u32,
}

impl Clone for SpeculativeMessageSources {
	fn clone(&self) -> Self {
		Self {
			sources: self.sources.clone(),
			max_messages_per_source: self.max_messages_per_source,
		}
	}
}

impl Default for SpeculativeMessageSources {
	fn default() -> Self {
		Self { sources: Vec::new(), max_messages_per_source: DEFAULT_MAX_MESSAGES_PER_SOURCE }
	}
}

impl SpeculativeMessageSources {
	/// Create an empty configuration (no off-chain fetch).
	pub fn disabled() -> Self {
		Self::default()
	}

	/// Create a configuration with the default per-source message cap.
	pub fn with_sources(sources: Vec<(ParaId, Arc<dyn OutboxQuery>)>) -> Self {
		Self { sources, max_messages_per_source: DEFAULT_MAX_MESSAGES_PER_SOURCE }
	}
}

/// Fetch speculative ingress for the block being built on `receiver_parent`.
///
/// When `sources` is empty, returns empty ingress (legacy behaviour). Otherwise
/// queries each sender's outbox at its best block and the receiver's expected
/// message cursor via [`SpeculativeInboxApi`].
pub async fn fetch_ingress_for_block<Block, Client, RClient>(
	receiver: &Client,
	_receiver_parent: Hash,
	destination: ParaId,
	config: &SpeculativeMessageSources,
	relay_parent: Hash,
	relay_client: &RClient,
	relay_parent_number: BlockNumber,
) -> SpeculativeIngress
where
	Block: BlockT<Hash = Hash>,
	Client: ProvideRuntimeApi<Block> + UsageProvider<Block>,
	Client::Api: SpeculativeInboxApi<Block>,
	RClient: RelayChainInterface,
{
	if config.sources.is_empty() {
		return empty_speculative_ingress();
	}

	let receiver_api = receiver.runtime_api();
	let mut batches = Vec::new();

	// Use the finalized head for position tracking rather than the fork parent.
	// This prevents re-delivering messages that are already committed on the
	// canonical chain when the collator builds competing forks from older parents.
	let finalized_hash = receiver.usage_info().chain.finalized_hash;

	tracing::debug!(
		target: "aura::cumulus",
		%relay_parent_number,
		sources = config.sources.len(),
		"fetch_ingress_for_block: fetching speculative batches",
	);

	for (source, sender) in &config.sources {
		let from_position = receiver_api
			.next_expected_message_position(finalized_hash, *source)
			.unwrap_or(0);
		let expected_provides_root = receiver_api
			.last_seen_provides_root(finalized_hash, *source)
			.unwrap_or_default();

		let sender_best = sender.best_block_hash();

		// If the relay chain has a newer provides root for this source, find the matching
		// sender block so we fetch the right batch.
		let mut fetch_at = sender_best;
		if let Ok(Some(relay_provides_root)) =
			relay_client.provides_root(*source, relay_parent).await
		{
			if relay_provides_root != Hash::default() &&
				relay_provides_root != expected_provides_root
			{
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					?relay_provides_root,
					?expected_provides_root,
					"relay provides_root advanced; looking up sender block for new root",
				);
				if let Some(at_relay) =
					sender.block_hash_for_provides_root(sender_best, relay_provides_root).await
				{
					fetch_at = at_relay;
				} else {
					// Cannot locate the relay-root block — fall back to sender_best and let
					// the relay's enactment-time check decide. ProvidesRoots lags by 1-2
					// relay blocks, so a root mismatch at fetch time is normal and the
					// candidate may still succeed once the sender's block is enacted.
					tracing::debug!(
						target: "aura::cumulus",
						source = ?source,
						?relay_provides_root,
						"could not find sender block for relay provides_root; using sender best",
					);
				}
			}
		}

		match build_message_batch_from_query(
			sender.as_ref(),
			fetch_at,
			*source,
			destination,
			fetch_at,
			relay_parent_number,
			from_position,
			config.max_messages_per_source,
		)
		.await
		{
			Some(batch) => {
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					messages = batch.messages.len(),
					provides_root = ?batch.provides_root,
					"fetched speculative batch",
				);
				batches.push(batch);
			},
			None => {
				tracing::trace!(
					target: "aura::cumulus",
					source = ?source,
					from_position,
					"no speculative messages available",
				);
			},
		}
	}

	tracing::debug!(
		target: "aura::cumulus",
		total_batches = batches.len(),
		"fetch_ingress_for_block: done",
	);

	SpeculativeIngress { batches }
}
