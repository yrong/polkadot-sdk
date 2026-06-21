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
use cumulus_primitives_core::{ParaId, SpeculativeInboxApi, SpeculativeIngress};
use cumulus_relay_chain_interface::RelayChainInterface;
use polkadot_primitives::{BlockNumber, Hash};
use sc_client_api::UsageProvider;
use sp_api::ProvideRuntimeApi;
use sp_runtime::traits::Block as BlockT;

/// Default cap on messages fetched per source per block.
pub const DEFAULT_MAX_MESSAGES_PER_SOURCE: u32 = 32;

/// Sender parachain clients used to build speculative ingress for this collator.
///
/// Each entry holds a type-erased [`OutboxQuery`]. The only implementation today is the
/// cross-process [`crate::collators::RpcOutboxClient`]; the trait is the abstraction point for
/// future transports (e.g. an in-process client when a single process holds the sender chain's
/// runtime API).
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

/// Fetch speculative ingress for the block being built on `receiver_parent`.
///
/// When `sources` is empty, returns empty ingress (legacy behaviour). Otherwise queries each
/// sender's outbox at its best block and the receiver's expected message cursor via
/// [`SpeculativeInboxApi`], building a batch against a root the relay already has in its provides
/// window so the receiver's `requires` matches directly (no proof).
///
/// Note: this no longer produces Late Block Proofs. The build-time rewind already targets an
/// in-window root, so a batch root outside the window is only seen when the sender (as we see it
/// over RPC) lacks that root — exactly when a valid bridge can't be generated. Such batches are
/// skipped; staleness is absorbed by the provides window + resubmission. The LBP generation /
/// verification machinery is retained (`OutboxQuery::generate_late_block_proof`,
/// `apply_late_block_proofs`, `ParachainBlockData::V2.late_block_proofs`) for future use — to make
/// it load-bearing, build against the receiver's *consumed* root and always bridge forward.
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

		// The batch is only usable if its root is in the relay's provides window — then the relay
		// matches `requires` directly, no proof. Otherwise skip:
		// - empty window → the relay has no committed root for us yet (root guard);
		// - root out of window → the build-time rewind couldn't target an in-window root (the
		//   sender, as seen over RPC, lacks it), which is exactly the case a Late Block Proof can't
		//   bridge. Staleness is absorbed by the window + resubmission; see the doc note above on
		//   the retained (unused) LBP machinery.
		if window.contains(&batch.subtree_root) {
			tracing::debug!(
				target: "aura::cumulus",
				source = ?source,
				messages = batch.messages.len(),
				subtree_root = ?batch.subtree_root,
				"fetched speculative batch (root in window)",
			);
			batches.push(batch);
		} else {
			tracing::debug!(
				target: "aura::cumulus",
				source = ?source,
				batch_subtree_root = ?batch.subtree_root,
				window_empty = window.is_empty(),
				"batch root not in relay provides window; skipping (waiting for window/resubmission)",
			);
		}
	}

	tracing::debug!(
		target: "aura::cumulus",
		total_batches = batches.len(),
		"fetch_ingress_for_block: done",
	);

	SpeculativeIngress { batches }
}
