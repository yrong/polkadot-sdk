// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Gas ceilings for dispatching a command on the Gateway contract.
//!
//! Shared by the v1 and v2 [`crate::v1::ConstantGasMeter`] and [`crate::v2::ConstantGasMeter`],
//! whose command enums differ but whose handlers do the same work.
//!
//! The figures are extracted from this report:
//! > forge test --match-path test/Gateway.t.sol --gas-report
//!
//! A healthy buffer is added on top to account for:
//! * The EIP-150 63/64 rule
//! * Future EVM upgrades that may increase gas cost
//!
//! Glamsterdam (EIP-8037) prices new state at 1530 gas per byte: 120 bytes for a new account
//! (183_600) and 64 bytes for a new storage slot (97_920). It replaces rather than adds to the
//! old charges, leaving the 2900 SSTORE residual and the code hash as execution. Commands that
//! allocate state are sized from that; the rest move only for EIP-8038, which reprices state
//! access. These are floors, not measurements: re-benchmark against a Gloas execution client.

/// Writes one existing slot, so no new state.
pub const SET_OPERATING_MODE: u64 = 60_000;

/// Updating the proxy before the initializer is called. The initializer's own
/// `maximum_required_gas` is added on top.
pub const UPGRADE_BASE: u64 = 75_000;

/// Ether to a new account allocates one account (183_600); an ERC20 transfer to a fresh
/// recipient allocates one slot (97_920). Plus AgentExecutor overhead.
pub const UNLOCK_NATIVE_TOKEN: u64 = 600_000;

/// Deploys a Token: 2336 code bytes (3_574_080) + new account (183_600) + five new slots for
/// name, symbol, tokenAddressOf and the two TokenInfo slots (489_600), so 4_247_280 state gas,
/// plus roughly 100_000 execution. Held well clear of that floor: a failed registration burns
/// the fee with no retry path.
pub const REGISTER_FOREIGN_TOKEN: u64 = 7_000_000;

/// A first mint allocates totalSupply and the recipient balance: two slots.
pub const MINT_FOREIGN_TOKEN: u64 = 400_000;

/// Worst-case assumptions are important:
/// * No gas refund for clearing storage slot of source account in ERC20 contract
/// * A fresh destination account allocates one slot (97_920 state gas)
/// * ERC20.transferFrom possibly does other business logic besides updating balances
///
/// v1 only: v2 routes token transfers through [`UNLOCK_NATIVE_TOKEN`].
pub const TRANSFER_TOKEN: u64 = 400_000;

/// Writes existing slots only, so no new state. v1 only.
pub const SET_TOKEN_TRANSFER_FEES: u64 = 90_000;

/// Writes existing slots only, so no new state. v1 only.
pub const SET_PRICING_PARAMETERS: u64 = 90_000;
