// SPDX-License-Identifier: Apache-2.0
// Diagnostic only: which spelling of AssetHub does the LIVE Ethereum-side
// Constants.ASSET_HUB_AGENT_ID (0x81c5ab2571199e3188135178f3c2c8e2d268be1313d029b30f534fa579b69b79)
// actually correspond to?

use crate::imports::*;
use snowbridge_core::AgentIdOf;
use xcm_executor::traits::ConvertLocation;

const LIVE_ASSET_HUB_AGENT_ID: [u8; 32] = hex_literal::hex!(
	"81c5ab2571199e3188135178f3c2c8e2d268be1313d029b30f534fa579b69b79"
);

#[test]
fn which_spelling_matches_the_live_agent_id() {
	// Live mainnet: GlobalConsensus(Polkadot) is a named NetworkId variant, not ByGenesis(hash) -
	// that's only how testnets (Westend/Rococo) without a dedicated variant are identified.
	let mainnet_global_consensus_form =
		Location::new(1, [GlobalConsensus(NetworkId::Polkadot), Parachain(1000)]);
	// Also check the Westend-genesis-hash form, in case the constant was computed against a
	// testnet deployment instead.
	let westend_global_consensus_form = Location::new(
		1,
		[GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(1000)],
	);
	let relative_form = Location::new(1, [Parachain(1000)]);

	let mainnet_agent_id = AgentIdOf::convert_location(&mainnet_global_consensus_form)
		.expect("mainnet global-consensus form resolves to an agent id");
	let westend_agent_id = AgentIdOf::convert_location(&westend_global_consensus_form)
		.expect("westend global-consensus form resolves to an agent id");
	let relative_agent_id = AgentIdOf::convert_location(&relative_form)
		.expect("relative form resolves to an agent id");

	let live = sp_core::H256(LIVE_ASSET_HUB_AGENT_ID);

	println!("live Constants.ASSET_HUB_AGENT_ID      : {:?}", live);
	println!("mainnet (Polkadot) global-consensus form: {:?}", mainnet_agent_id);
	println!("westend global-consensus form           : {:?}", westend_agent_id);
	println!("relative form                           : {:?}", relative_agent_id);
	println!("mainnet  == live ? {}", mainnet_agent_id == live);
	println!("westend  == live ? {}", westend_agent_id == live);
	println!("relative == live ? {}", relative_agent_id == live);

	panic!(
		"diagnostic: mainnet_matches={}, westend_matches={}, relative_matches={}",
		mainnet_agent_id == live,
		westend_agent_id == live,
		relative_agent_id == live
	);
}
