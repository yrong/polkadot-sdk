// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Gas ceilings for dispatching a command on the Gateway contract, shared by the v1 and v2
//! `ConstantGasMeter`.
//!
//! Figures from `forge test --match-path test/Gateway.t.sol --gas-report`, buffered for the
//! EIP-150 63/64 rule and future EVM upgrades.
//!
//! Glamsterdam (EIP-8037) prices new state at 1530 gas/byte: 183_600 for a new account,
//! 97_920 for a new storage slot. It replaces the old allocation charges rather than adding
//! to them. Commands that allocate no new state move only for EIP-8038, which reprices state
//! access. These are floors, not measurements: re-benchmark on a Gloas execution client.

/// Writes one existing slot.
pub const SET_OPERATING_MODE: u64 = 60_000;

/// Proxy update before the initializer runs; `maximum_required_gas` is added on top.
pub const UPGRADE_BASE: u64 = 75_000;

/// Ether to a new account (183_600), or an ERC20 transfer to a fresh recipient (97_920),
/// plus AgentExecutor overhead.
pub const UNLOCK_NATIVE_TOKEN: u64 = 600_000;

/// Deploys a Token: 2336 code bytes (3_574_080) + new account (183_600) + five slots for
/// name, symbol, tokenAddressOf and the two TokenInfo slots (489_600) = 4_247_280, plus
/// roughly 100_000 execution.
pub const REGISTER_FOREIGN_TOKEN: u64 = 7_000_000;

/// A first mint allocates totalSupply and the recipient balance: two slots.
pub const MINT_FOREIGN_TOKEN: u64 = 400_000;

/// v1 only. Worst case: no refund for clearing the source slot, a fresh destination allocates
/// one slot (97_920), and transferFrom may do more than update balances.
pub const TRANSFER_TOKEN: u64 = 400_000;

/// Writes existing slots only. v1 only.
pub const SET_TOKEN_TRANSFER_FEES: u64 = 90_000;

/// Writes existing slots only. v1 only.
pub const SET_PRICING_PARAMETERS: u64 = 90_000;
