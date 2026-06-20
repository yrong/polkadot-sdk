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
use cumulus_pallet_speculative_outbox::{latest_requires_key, ConsumedAck};
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

/// Build the consumed-ack inherent for *this* parachain (`self_para_id`) as a sender: query the
/// relay for receivers that have acknowledged consumption and prove their
/// `ParaInclusion::LatestRequires(self, receiver)` entries against the relay parent.
///
/// Returns `None` when there are no acks or the relay query / proof fails (the collator then simply
/// builds no consumed-ack inherent this block). The runtime applies the K-deep finality gate and
/// re-verifies the proof, so this path only needs to be best-effort.
pub async fn fetch_consumed_ack<RClient>(
	relay_client: &RClient,
	self_para_id: ParaId,
	relay_parent: Hash,
) -> Option<ConsumedAck>
where
	RClient: RelayChainInterface,
{
	let acks = relay_client.latest_requires_for_source(self_para_id, relay_parent).await.ok()?;
	if acks.is_empty() {
		return None;
	}
	// Bound the batch to the runtime's `MAX_ACKS_PER_CALL` (64); the runtime gates K-deep finality.
	let receivers: Vec<ParaId> =
		acks.into_iter().map(|(receiver, _, _)| receiver).take(64).collect();
	let keys: Vec<Vec<u8>> = receivers
		.iter()
		.map(|receiver| latest_requires_key(self_para_id, *receiver))
		.collect();
	let proof = relay_client.prove_read(relay_parent, &keys).await.ok()?;
	Some(ConsumedAck { proof, receivers })
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

		// Fetch the relay's provides *window* for this `(source -> us)` pair: the recent
		// subtree roots the relay will accept (issue #12349). An empty window means the
		// sender has not yet been relay-enacted, or has not committed messages to us.
		let window: Vec<Hash> = relay_client
			.provides_window(*source, destination, relay_parent)
			.await
			.unwrap_or_default()
			.into_iter()
			.filter(|r| *r != Hash::default())
			.collect();

		// The newest window root is the preferred build/transform target — it is the
		// most recent root the relay knows and the most likely to still be in the window
		// at enactment.
		let newest_root = window.last().copied();

		// Prefer building the batch at the sender block that produced the newest window
		// root, so it lands on a relay-known root (no proof needed). Otherwise fall back
		// to the sender's best, which may be behind or ahead of the window.
		let mut fetch_at = sender_best;
		if let Some(target) = newest_root {
			if let Some(at_target) =
				sender.block_hash_for_subtree_root(sender_best, destination, target).await
			{
				fetch_at = at_target;
			} else {
				tracing::debug!(
					target: "aura::cumulus",
					source = ?source,
					?target,
					"no sender block for newest window root; using sender best",
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

		// Window-aware Late Block Proof decision (issue #12349):
		// - batch root already in the window → the relay matches it directly, no proof.
		// - batch root older than the window → fetch an LBP proving it extends to the newest window
		//   root, so the relay transforms `requires` and matches (§6.2).
		// - empty window → no relay anchor; skip (root guard).
		if window.contains(&batch.subtree_root) {
			tracing::debug!(
				target: "aura::cumulus",
				source = ?source,
				messages = batch.messages.len(),
				subtree_root = ?batch.subtree_root,
				"fetched speculative batch (root in window, no LBP required)",
			);
			batches.push(batch);
		} else if let Some(target) = newest_root {
			let lbp_at = sender
				.block_hash_for_subtree_root(sender_best, destination, target)
				.await
				.unwrap_or(sender_best);
			match sender.generate_late_block_proof(lbp_at, destination, batch.subtree_root).await {
				Some(proof) => {
					tracing::debug!(
						target: "aura::cumulus",
						source = ?source,
						messages = batch.messages.len(),
						batch_subtree_root = ?batch.subtree_root,
						?target,
						"fetched speculative batch + late block proof (out of window)",
					);
					batches.push(batch);
					late_block_proofs.push(proof);
				},
				None => {
					tracing::warn!(
						target: "aura::cumulus",
						source = ?source,
						batch_subtree_root = ?batch.subtree_root,
						?target,
						"could not generate late block proof; skipping batch",
					);
				},
			}
		} else {
			// Empty window: the relay has no committed subtree root for us yet, so the
			// receiver cannot pass `requires_satisfied` at inclusion. Skip the batch —
			// see the root guard discussion in the design doc.
			tracing::debug!(
				target: "aura::cumulus",
				source = ?source,
				batch_subtree_root = ?batch.subtree_root,
				"root guard: relay window is empty; skipping batch",
			);
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
