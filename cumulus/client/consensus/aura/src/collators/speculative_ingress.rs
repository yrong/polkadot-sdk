// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

//! Speculative messaging ingress fetch for Aura collators.

use std::sync::Arc;

use cumulus_pallet_speculative_inbox::client::{
	empty_speculative_ingress,
	fetch_speculative_ingress as fetch_batches_from_senders,
};
use cumulus_primitives_core::{ParaId, SpeculativeInboxApi, SpeculativeOutboxApi};
use polkadot_primitives::{
	v10::SpeculativeIngress, BlockNumber, Hash,
};
use sc_client_api::UsageProvider;
use sp_api::ProvideRuntimeApi;
use sp_runtime::traits::Block as BlockT;

/// Default cap on messages fetched per source per block.
pub const DEFAULT_MAX_MESSAGES_PER_SOURCE: u32 = 32;

/// Sender parachain clients used to build speculative ingress for this collator.
#[derive(Clone)]
pub struct SpeculativeMessageSources<Client> {
	/// `(source_para_id, sender_chain_client)`.
	pub sources: Vec<(ParaId, Arc<Client>)>,
	/// Maximum messages to pull from each source per block.
	pub max_messages_per_source: u32,
}

impl<Client> Default for SpeculativeMessageSources<Client> {
	fn default() -> Self {
		Self {
			sources: Vec::new(),
			max_messages_per_source: DEFAULT_MAX_MESSAGES_PER_SOURCE,
		}
	}
}

impl<Client> SpeculativeMessageSources<Client> {
	/// Create an empty configuration (no off-chain fetch).
	pub fn disabled() -> Self {
		Self::default()
	}

	/// Create a configuration with the default per-source message cap.
	pub fn with_sources(sources: Vec<(ParaId, Arc<Client>)>) -> Self {
		Self { sources, max_messages_per_source: DEFAULT_MAX_MESSAGES_PER_SOURCE }
	}
}

/// Fetch speculative ingress for the block being built on `receiver_parent`.
///
/// When `sources` is empty, returns empty ingress (legacy behaviour). Otherwise
/// queries each sender's outbox at its best block and the receiver's expected
/// message cursor via [`SpeculativeInboxApi`].
pub fn fetch_ingress_for_block<Block, Client>(
	receiver: &Client,
	receiver_parent: Hash,
	destination: ParaId,
	config: &SpeculativeMessageSources<Client>,
	relay_parent_number: BlockNumber,
) -> SpeculativeIngress
where
	Block: BlockT<Hash = Hash>,
	Client: ProvideRuntimeApi<Block> + UsageProvider<Block>,
	Client::Api: SpeculativeInboxApi<Block> + SpeculativeOutboxApi<Block>,
{
	if config.sources.is_empty() {
		return empty_speculative_ingress();
	}

	let receiver_api = receiver.runtime_api();
	let mut fetch_list = Vec::with_capacity(config.sources.len());

	for (source, sender) in &config.sources {
		let from_position = receiver_api
			.next_expected_message_position(receiver_parent, *source)
			.unwrap_or(0);
		let sender_best = sender.as_ref().usage_info().chain.best_hash;
		fetch_list.push((
			*source,
			sender.as_ref(),
			sender_best,
			relay_parent_number,
			from_position,
		));
	}

	fetch_batches_from_senders(
		&fetch_list,
		destination,
		config.max_messages_per_source,
	)
}
