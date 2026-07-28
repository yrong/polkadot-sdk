// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Focused E2E: the spec-msg fetch pipeline *consumes* DHT-discovered peers.
//!
//! Spawns a rococo-local relay + two penpals. Each is told how to reach the
//! other's collators (`SourceDiscovery::set_source_genesis`), then a spec-msg
//! channel is opened A→B and accepted on B. The handshake alone drives both
//! fetchers: A's `OpenChannel` signal crosses to B (B fetches A's stream) and
//! B's acceptance register crosses back to A (A fetches B's ack stream). So the
//! test proves the full chain — discovery resolves peers → the fetcher reads the
//! *same* shared `PeerRegistry` and fetches from those peers — by asserting, on
//! both collators, both the discovery log and a non-empty fetch round.
//!
//! Dynamic calls (subxt runtime-metadata API), so no static-codegen/metadata
//! dependency — the runtime-integration proof the `spec_msg_penpal` static test
//! can't give in this workspace (subxt version drift).

use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as RelayParaId;
use std::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{dynamic::Value, tx::dynamic, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfigBuilder,
};

const PARA_A: u32 = 2000;
const PARA_B: u32 = 2001;

/// Sudo-wraps a runtime call built as a dynamic `Value` and waits for it to
/// finalize (governance-gated calls are root-only, so sudo like the real flow).
async fn sudo(
	client: &OnlineClient<PolkadotConfig>,
	runtime_call: Value,
) -> Result<(), anyhow::Error> {
	let call = dynamic("Sudo", "sudo", vec![runtime_call]);
	client
		.tx()
		.sign_and_submit_then_watch_default(&call, &dev::alice())
		.await?
		.wait_for_finalized_success()
		.await?;
	Ok(())
}

/// `SourceDiscovery::set_source_genesis(source, Some((genesis, None)))` — tells
/// this penpal how to reach `source`'s collators over the relay DHT.
async fn set_source_genesis(
	client: &OnlineClient<PolkadotConfig>,
	source: u32,
	genesis: [u8; 32],
) -> Result<(), anyhow::Error> {
	let none = Value::unnamed_variant("None", []);
	let info = Value::unnamed_variant(
		"Some",
		[Value::unnamed_composite([Value::from_bytes(genesis), none])],
	);
	let pallet_call = Value::named_variant(
		"set_source_genesis",
		[("source", Value::u128(source as u128)), ("info", info)],
	);
	sudo(client, Value::unnamed_variant("SourceDiscovery", [pallet_call])).await
}

/// `SpecMessaging::open_channel { recipient, domain: 0, num: 0 }` — the sender
/// half of the handshake (emits the `OpenChannel` signal onto the channel stream).
async fn open_channel(
	client: &OnlineClient<PolkadotConfig>,
	recipient: u32,
) -> Result<(), anyhow::Error> {
	let pallet_call = Value::named_variant(
		"open_channel",
		[
			("recipient", Value::u128(recipient as u128)),
			("domain", Value::u128(0)),
			("num", Value::u128(0)),
		],
	);
	sudo(client, Value::unnamed_variant("SpecMessaging", [pallet_call])).await
}

/// `SpecMessaging::accept_open_channel { sender, domain: 0, num: 0 }` — the
/// receiver half: `sender`'s data stream joins the consumed set, so the own
/// collators start fetching it.
async fn accept_open_channel(
	client: &OnlineClient<PolkadotConfig>,
	sender: u32,
) -> Result<(), anyhow::Error> {
	let pallet_call = Value::named_variant(
		"accept_open_channel",
		[
			("sender", Value::u128(sender as u128)),
			("domain", Value::u128(0)),
			("num", Value::u128(0)),
		],
	);
	sudo(client, Value::unnamed_variant("SpecMessaging", [pallet_call])).await
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_msg_consumes_discovered_peers() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let args = || vec![("-lspec-msg=info,source-discovery=trace,bootnodes=trace").into()];
	let images = zombienet_sdk::environment::get_images_from_env();
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
				.with_validator(|node| node.with_name("charlie"))
				.with_validator(|node| node.with_name("dave"))
		})
		.with_parachain(|p| {
			p.with_id(PARA_A)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("penpal-rococo-2000")
				.with_collator(|n| n.with_name("penpal-a").with_args(args()))
		})
		.with_parachain(|p| {
			p.with_id(PARA_B)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("penpal-rococo-2001")
				.with_collator(|n| n.with_name("penpal-b").with_args(args()))
		})
		.build()
		.map_err(|e| {
			anyhow!(
				"config errs: {}",
				e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ")
			)
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_client: OnlineClient<PolkadotConfig> =
		network.get_node("alice")?.wait_client().await?;
	let a_client: OnlineClient<PolkadotConfig> =
		network.get_node("penpal-a")?.wait_client().await?;
	let b_client: OnlineClient<PolkadotConfig> =
		network.get_node("penpal-b")?.wait_client().await?;

	log::info!("Waiting for both penpals to produce blocks");
	assert_para_throughput(
		&relay_client,
		15,
		[(RelayParaId::from(PARA_A), 2..40), (RelayParaId::from(PARA_B), 2..40)],
		[],
	)
	.await?;

	// Discovery config both ways — each side must reach the other to fetch its
	// half of the handshake.
	let genesis_a = a_client.genesis_hash();
	let genesis_b = b_client.genesis_hash();
	log::info!("Setting source genesis both directions");
	set_source_genesis(&b_client, PARA_A, genesis_a.0).await?;
	set_source_genesis(&a_client, PARA_B, genesis_b.0).await?;

	// Open A→B and accept on B. The handshake's signals cross both streams, so
	// both fetchers must consume the discovered peers to carry them.
	log::info!("Opening spec-msg channel A->B and accepting on B");
	open_channel(&a_client, PARA_B).await?;
	accept_open_channel(&b_client, PARA_A).await?;

	// Assert, on both collators: (1) discovery resolved ≥1 peer, and (2) a
	// non-empty fetch round completed — i.e. the fetcher read the shared registry
	// and fetched from those peers. `is_glob=false` → the patterns are regexes.
	// "Fetch round completed source=" matches only the success log; the empty
	// round renders "Fetch round completed (nothing new) source=".
	log::info!("Asserting discovery + fetch consumption on both collators");
	let penpal_a = network.get_node("penpal-a")?;
	let penpal_b = network.get_node("penpal-b")?;
	let found = LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(600), false);
	for (name, node) in [("penpal-a", &penpal_a), ("penpal-b", &penpal_b)] {
		assert!(
			node.wait_log_line_count_with_timeout(
				"Discovered source peers.*count=[1-9]",
				false,
				found.clone(),
			)
			.await?
			.success(),
			"{name} did not discover the source's peers",
		);
		assert!(
			node.wait_log_line_count_with_timeout(
				"Fetch round completed source=",
				false,
				found.clone(),
			)
			.await?
			.success(),
			"{name} did not fetch from the discovered peers (no non-empty fetch round)",
		);
	}

	log::info!("Spec-msg consume-discovered-peers E2E passed");
	Ok(())
}
