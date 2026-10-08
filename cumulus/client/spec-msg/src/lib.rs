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

//! Speculative Messaging node side.
//!
//! A sending parachain's block commits to all its outbound streams with one `StreamsRoot`; the
//! payloads travel off-chain between collators. This crate is that off-chain half, starting with
//! the sender:
//!
//! - [`Archive`]: the own chain's sends by `(stream, position)`, with per-block boundaries indexed
//!   by the recomputed `StreamsRoot`. Serves the fetch protocol's two requests, each proven under
//!   the root the requester names.
//! - [`run_archiver`]: follows the own chain and keeps the archive current, including reorgs and
//!   retention.
//! - [`exchange_protocol_config`] and [`run_request_handler`]: the `/spec-msg/exchange/1`
//!   request-response protocol.
//!
//! The receiver side (relay monitor, fetcher, pool, inherent provider, lift assembler) builds on
//! these. Design: paritytech/polkadot-sdk#12659 (v0.5), sections Runtime API, Fetch Protocol,
//! Liftability, Archive Pruning and Networking.

#![warn(missing_docs)]

pub mod archive;
pub mod protocol;
pub mod store;
pub mod worker;

pub use archive::{Archive, ArchiveError, Tip};
pub use protocol::{exchange_protocol_config, run_request_handler, PROTOCOL_NAME};
pub use store::{ArchiveStore, AuxArchiveStore, Batch, MemoryStore};
pub use worker::{run_archiver, SERVING_HORIZON};

/// Log target of this crate.
pub const LOG_TARGET: &str = "spec-msg";

#[cfg(test)]
mod tests;
