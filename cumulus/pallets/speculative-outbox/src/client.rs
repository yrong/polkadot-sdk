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

//! Client-side (node) support for the speculative outbox.
//!
//! The consumed-ack inherent lets the sender prune payloads a receiver has provably consumed
//! (follow-up to #12350). The collator:
//!
//! 1. queries the relay `latest_requires_for_source(self)` runtime API for receivers that have
//!    acknowledged consumption,
//! 2. builds a relay state proof over their `ParaInclusion::LatestRequires(self, receiver)` keys
//!    (the keys are produced by [`crate::latest_requires_key`]),
//! 3. assembles a [`ConsumedAck`] and injects it here before block proposal.
//!
//! The runtime re-verifies the proof against the relay-parent state root and applies a K-deep
//! finality gate in [`crate::Pallet::note_consumed`].

use crate::ConsumedAck;
use sp_inherents::{InherentData, InherentIdentifier};

/// Inherent identifier for the consumed-ack inherent — matches [`crate::INHERENT_IDENTIFIER`].
pub const INHERENT_IDENTIFIER: InherentIdentifier = crate::INHERENT_IDENTIFIER;

/// Inject a [`ConsumedAck`] into the inherent data before block proposal.
///
/// Only call this with a non-empty `ack`; an empty one produces no inherent (the pallet's
/// `create_inherent` returns `None` for an empty receiver set anyway).
pub fn inject_consumed_acks(
	inherent_data: &mut InherentData,
	ack: &ConsumedAck,
) -> Result<(), sp_inherents::Error> {
	inherent_data.put_data(INHERENT_IDENTIFIER, ack)
}
