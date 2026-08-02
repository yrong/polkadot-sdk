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

//! The receiver's finalized-consumption pruner: the upper bound on verified-pool
//! payload retention.
//!
//! The pool retains consumed-but-unfinalized payloads non-destructively (a
//! consuming ancestor may be reorged away, and dropping them would strand the
//! surviving branch on an unservable gap). That leaves growth bounded only by
//! the stream length until finality catches up. This worker supplies the finality
//! signal: on each *finalized* parachain block it reads the finalized consumption
//! frontier per stream (`SpecMsgApi::consumed_streams()` at the finalized block)
//! and drops the pool's payloads below it — no live fork descends below a
//! finalized block, so those payloads can never be handed again (see
//! [`SpecMsgPool::prune_finalized`]). Leaf hashes are retained for lift
//! generation, so only the bulk payload bytes are reclaimed.

use std::sync::Arc;

use futures::StreamExt;
use sc_client_api::BlockchainEvents;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_runtime::traits::Block as BlockT;

use cumulus_primitives_core::{ParaId, SpecMsgApi};
use cumulus_primitives_spec_messaging::ConsumedStream;

use crate::{pool::SpecMsgPool, LOG_TARGET};

/// Runs the finalized-consumption pruner until the parachain's finality stream
/// ends. Spawn it next to [`crate::run_relay_provides_monitor`] /
/// [`crate::run_spec_msg_fetcher`], sharing the same pool.
pub async fn run_finalized_pruner<Block, Client>(
	para_id: ParaId,
	parachain: Arc<Client>,
	pool: Arc<SpecMsgPool>,
) where
	Block: BlockT,
	Client: BlockchainEvents<Block> + ProvideRuntimeApi<Block>,
	Client::Api: SpecMsgApi<Block>,
{
	let mut finality = parachain.finality_notification_stream();
	while let Some(notification) = finality.next().await {
		let hash = notification.hash;
		// The version gate, exactly like the monitor/fetcher: no `SpecMsgApi`,
		// nothing is consumed, so nothing to prune.
		match parachain.runtime_api().has_api::<dyn SpecMsgApi<Block>>(hash) {
			Ok(true) => {},
			Ok(false) => continue,
			Err(error) => {
				tracing::debug!(
					target: LOG_TARGET,
					?error,
					"Finalized pruner: `has_api` check failed",
				);
				continue;
			},
		}
		let consumed = match parachain.runtime_api().consumed_streams(hash) {
			Ok(consumed) => consumed,
			Err(error) => {
				tracing::debug!(
					target: LOG_TARGET,
					?error,
					?hash,
					"Finalized pruner: `consumed_streams` read failed",
				);
				continue;
			},
		};
		for (source, streams) in &consumed {
			for stream in streams {
				// Only channel streams accumulate payloads; register reads are
				// already hard-bounded (`RETAINED_REGISTER_READS`).
				if let ConsumedStream::Channel { from, .. } = stream {
					pool.prune_finalized(*source, &stream.stream_id(para_id), from.0);
				}
			}
		}
	}
}
