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
//! the block before proposal.

use polkadot_primitives::v10::SpeculativeIngress;
use sp_inherents::{InherentData, InherentIdentifier};

/// The inherent identifier for speculative ingress — matches the constant
/// in the pallet runtime.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"specingr";

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
