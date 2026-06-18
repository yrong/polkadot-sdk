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
use cumulus_primitives_core::{LateBlockProof, ParaId, SpeculativeInboxApi, SpeculativeIngress};
use cumulus_relay_chain_interface::RelayChainInterface;
use polkadot_primitives::{BlockNumber, Hash};
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

/// Fetch speculative ingress (and any required late block proofs) for the block being
/// built on `receiver_parent`.
///
/// When `sources` is empty, returns empty ingress / no LBPs (legacy behaviour). Otherwise
/// queries each sender's outbox at its best block and the receiver's expected
/// message cursor via [`SpeculativeInboxApi`].
///
/// The collator attaches the returned `Vec<LateBlockProof>` to
/// `ParachainBlockData::V2.late_block_proofs` so the PVF's `apply_messaging_proofs`
/// can transform `requires[source].expected_root` from the (older) batch root to
/// the relay-committed current root.
pub async fn fetch_ingress_for_block<Block, Client, RClient>(
	receiver: &Client,
	_receiver_parent: Hash,
	destination: ParaId,
	config: &SpeculativeMessageSources,
	relay_parent: Hash,
	relay_client: &RClient,
	relay_parent_number: BlockNumber,
) -> (SpeculativeIngress, Vec<LateBlockProof>)
where
	Block: BlockT<Hash = Hash>,
	Client: ProvideRuntimeApi<Block> + UsageProvider<Block>,
	Client::Api: SpeculativeInboxApi<Block>,
	RClient: RelayChainInterface,
{
	if config.sources.is_empty() {
		return (empty_speculative_ingress(), Vec::new());
	}

	let receiver_api = receiver.runtime_api();
	let mut batches = Vec::new();
	let mut late_block_proofs = Vec::new();

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

		let sender_best = sender.best_block_hash();

		// Fetch the relay's committed provides commitment for this source and look up
		// our (the receiver's) subtree root in it. `None` means the sender has not
		// yet been relay-enacted, or has not committed messages to us.
		let relay_subtree_root: Option<Hash> = relay_client
			.provides_root(*source, relay_parent)
			.await
			.ok()
			.flatten()
			.and_then(|set| set.get(destination).copied())
			.filter(|r| *r != Hash::default());

		// Pick a sender block for batch fetch. Prefer the block matching the relay's
		// committed subtree root so the batch lands on a relay-known root; otherwise
		// fall back to the sender's best, which may have advanced past the relay.
		let mut fetch_at = sender_best;
		let mut fetch_at_root: Option<Hash> = None;
		if let Some(relay_root) = relay_subtree_root {
			tracing::debug!(
				target: "aura::cumulus",
				source = ?source,
				?relay_root,
				"looking up sender block for relay-committed subtree root",
			);
			if let Some(at_relay) =
				sender.block_hash_for_subtree_root(sender_best, destination, relay_root).await
			{
				fetch_at = at_relay;
				fetch_at_root = Some(relay_root);
			} else {
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					?relay_root,
					"could not find sender block for relay subtree root; using sender best",
				);
			}
		}

		let batch = match build_message_batch_from_query(
			sender.as_ref(),
			fetch_at,
			*source,
			destination,
			relay_parent_number,
			from_position,
			config.max_messages_per_source,
		)
		.await
		{
			Some(b) => b,
			None => {
				tracing::trace!(
					target: "aura::cumulus",
					source = ?source,
					from_position,
					"no speculative messages available",
				);
				continue;
			},
		};

		// Determine whether the batch needs a late block proof. The PVF requires an
		// LBP whenever the batch's `subtree_root` differs from the relay-committed
		// current subtree root for this receiver: the LBP authorizes the runtime-side
		// `requires[source]` entry to be transformed from `old → new` so the relay's
		// `requires_satisfied` check passes at enactment.
		match (relay_subtree_root, fetch_at_root) {
			(Some(relay_root), Some(matched)) if matched == batch.subtree_root => {
				// Batch is built against the relay-current subtree root. No LBP needed.
				let _ = relay_root;
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					messages = batch.messages.len(),
					subtree_root = ?batch.subtree_root,
					"fetched speculative batch (no LBP required)",
				);
				batches.push(batch);
			},
			(Some(relay_root), _) => {
				// Batch is built against an older subtree root than the relay knows about.
				// Fetch an LBP from the sender at the block that produced the relay's
				// current root, proving the batch's root is an ancestor of relay_root.
				let lbp_at = sender
					.block_hash_for_subtree_root(sender_best, destination, relay_root)
					.await
					.unwrap_or(sender_best);
				match sender
					.generate_late_block_proof(lbp_at, destination, batch.subtree_root)
					.await
				{
					Some(proof) => {
						tracing::debug!(
							target: "aura::cumulus",
							source = ?source,
							messages = batch.messages.len(),
							batch_subtree_root = ?batch.subtree_root,
							?relay_root,
							"fetched speculative batch + late block proof",
						);
						batches.push(batch);
						late_block_proofs.push(proof);
					},
					None => {
						tracing::warn!(
							target: "aura::cumulus",
							source = ?source,
							batch_subtree_root = ?batch.subtree_root,
							?relay_root,
							"could not generate late block proof; skipping batch",
						);
					},
				}
			},
			(None, _) => {
				// Relay has no committed subtree root for this receiver yet (sender
				// hasn't been relay-enacted, or hasn't sent to us). Without a relay
				// anchor, the receiver cannot pass requires_satisfied at inclusion
				// time. Skip this batch — see the root guard discussion in the design doc.
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					batch_subtree_root = ?batch.subtree_root,
					"root guard: relay has not committed a subtree root for us; skipping batch",
				);
			},
		}
	}

	tracing::debug!(
		target: "aura::cumulus",
		total_batches = batches.len(),
		total_lbps = late_block_proofs.len(),
		"fetch_ingress_for_block: done",
	);

	(SpeculativeIngress { batches }, late_block_proofs)
}
