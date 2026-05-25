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

//! Abstraction over sender-chain outbox access.
//!
//! [`OutboxQuery`] is an async trait implemented by [`RpcOutboxClient`] for
//! cross-process JSON-RPC access. Future implementations (e.g. an HTTP provider
//! client) add a new `OutboxQuery` impl without touching collator logic.

use std::sync::Arc;

use codec::{Decode, Encode};
use cumulus_primitives_core::ParaId;
use jsonrpsee::{core::client::ClientT, rpc_params, ws_client::WsClientBuilder};
use polkadot_primitives::{v9::ProvidesCommitment, BlockNumber, Hash};

/// Abstracts over RPC access to a sender chain's speculative outbox.
#[async_trait::async_trait]
pub trait OutboxQuery: Send + Sync + 'static {
	/// Best known block hash on the sender chain. Used as the default fetch point.
	fn best_block_hash(&self) -> Hash;

	async fn compute_provides_root(&self, at: Hash) -> Option<ProvidesCommitment>;

	async fn destination_state(&self, at: Hash, dest: ParaId) -> Option<(Hash, u64)>;

	async fn outbound_messages(
		&self,
		at: Hash,
		dest: ParaId,
		from: u64,
		max: u32,
	) -> Vec<(u64, Vec<u8>)>;

	async fn subtree_inclusion_proof(
		&self,
		at: Hash,
		dest: ParaId,
		root: Hash,
	) -> Option<(Vec<Hash>, u32, u32)>;

	async fn block_hash_for_provides_root(&self, at: Hash, root: Hash) -> Option<Hash>;
}

// ── RPC (cross-process) implementation ───────────────────────────────────────

/// Queries a sender chain running in a separate process via its JSON-RPC endpoint.
///
/// Connects over WebSocket. Each method encodes arguments as SCALE bytes and calls
/// `state_call` with the Substrate runtime API dispatch name.
pub struct RpcOutboxClient {
	client: jsonrpsee::ws_client::WsClient,
}

impl RpcOutboxClient {
	/// Connect to a sender node's WS-RPC endpoint (e.g. `"ws://127.0.0.1:9944"`).
	pub async fn connect(url: &str) -> Result<Arc<dyn OutboxQuery>, jsonrpsee::core::ClientError> {
		let client = WsClientBuilder::default().build(url).await?;
		Ok(Arc::new(Self { client }))
	}

	async fn state_call<R: Decode + Send>(
		&self,
		method: &str,
		at: Hash,
		args: Vec<u8>,
	) -> Option<R> {
		let result: sp_core::Bytes = match self
			.client
			.request("state_call", rpc_params![method, sp_core::Bytes(args), at])
			.await
		{
			Ok(r) => r,
			Err(e) => {
				tracing::warn!(
					target: "aura::cumulus",
					%method,
					?at,
					error = %e,
					"state_call RPC failed",
				);
				return None;
			},
		};
		match R::decode(&mut &result.0[..]) {
			Ok(v) => Some(v),
			Err(e) => {
				tracing::warn!(
					target: "aura::cumulus",
					%method,
					?at,
					error = %e,
					"state_call SCALE decode failed",
				);
				None
			},
		}
	}
}

#[async_trait::async_trait]
impl OutboxQuery for RpcOutboxClient {
	fn best_block_hash(&self) -> Hash {
		tokio::task::block_in_place(|| {
			tokio::runtime::Handle::current().block_on(async {
				// chain_getBlockHash with no params returns the best block hash.
				// chain_getHead returns the best block HEADER (not the hash) so must not be used.
				self.client
					.request::<Option<Hash>, _>("chain_getBlockHash", rpc_params![])
					.await
					.ok()
					.flatten()
					.unwrap_or_default()
			})
		})
	}

	async fn compute_provides_root(&self, at: Hash) -> Option<ProvidesCommitment> {
		self.state_call::<Option<ProvidesCommitment>>(
			"SpeculativeOutboxApi_compute_provides_root",
			at,
			vec![],
		)
		.await
		.flatten()
	}

	async fn destination_state(&self, at: Hash, dest: ParaId) -> Option<(Hash, u64)> {
		self.state_call::<Option<(Hash, u64)>>(
			"SpeculativeOutboxApi_destination_state",
			at,
			dest.encode(),
		)
		.await
		.flatten()
	}

	async fn outbound_messages(
		&self,
		at: Hash,
		dest: ParaId,
		from: u64,
		max: u32,
	) -> Vec<(u64, Vec<u8>)> {
		self.state_call("SpeculativeOutboxApi_outbound_messages", at, (dest, from, max).encode())
			.await
			.unwrap_or_default()
	}

	async fn subtree_inclusion_proof(
		&self,
		at: Hash,
		dest: ParaId,
		root: Hash,
	) -> Option<(Vec<Hash>, u32, u32)> {
		self.state_call::<Option<(Vec<Hash>, u32, u32)>>(
			"SpeculativeOutboxApi_subtree_inclusion_proof",
			at,
			(dest, root).encode(),
		)
		.await
		.flatten()
	}

	async fn block_hash_for_provides_root(&self, at: Hash, root: Hash) -> Option<Hash> {
		self.state_call::<Option<Hash>>(
			"SpeculativeOutboxApi_block_hash_for_provides_root",
			at,
			root.encode(),
		)
		.await
		.flatten()
	}
}

// ── build_message_batch helper ────────────────────────────────────────────────

/// Async equivalent of `cumulus_pallet_speculative_inbox::client::build_message_batch`
/// that uses `OutboxQuery` instead of a direct `ProvideRuntimeApi` client.
pub async fn build_message_batch_from_query(
	source: &dyn OutboxQuery,
	at: Hash,
	source_para_id: ParaId,
	destination: ParaId,
	source_block: Hash,
	source_relay_parent_number: BlockNumber,
	from_position: u64,
	max_messages: u32,
) -> Option<polkadot_primitives::v9::MessageBatch> {
	use polkadot_primitives::v9::{MessageBatch, OutgoingMessage};

	let provides = source.compute_provides_root(at).await?;
	let (subtree_root, _) = source.destination_state(at, destination).await?;
	let messages = source.outbound_messages(at, destination, from_position, max_messages).await;
	if messages.is_empty() {
		tracing::trace!(
			target: "aura::cumulus",
			source = ?source_para_id,
			dest = ?destination,
			from_position,
			"outbound_messages: empty",
		);
		return None;
	}

	tracing::debug!(
		target: "aura::cumulus",
		source = ?source_para_id,
		dest = ?destination,
		count = messages.len(),
		provides_root = ?provides.root,
		?subtree_root,
		"building message batch",
	);

	let (proof, number_of_destinations, leaf_index) =
		match source.subtree_inclusion_proof(at, destination, subtree_root).await {
			Some(p) => p,
			None => {
				tracing::warn!(
					target: "aura::cumulus",
					source = ?source_para_id,
					dest = ?destination,
					?subtree_root,
					provides_root = ?provides.root,
					"subtree_inclusion_proof returned None — batch dropped",
				);
				return None;
			},
		};

	Some(MessageBatch {
		source: source_para_id,
		source_block,
		source_relay_parent_number,
		provides_root: provides.root,
		subtree_root,
		subtree_inclusion_proof: proof,
		number_of_destinations,
		leaf_index,
		messages: messages
			.into_iter()
			.map(|(position, payload)| OutgoingMessage { position, payload })
			.collect(),
	})
}
