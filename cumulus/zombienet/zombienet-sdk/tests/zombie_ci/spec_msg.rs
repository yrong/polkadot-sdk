// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Speculative messaging end to end: two penpal chains open a channel and deliver an XCM with no
//! HRMP channel between them.
//!
//! Each chain both sends and receives: the sender reads the receiver's `Ack` register, so each
//! collator names the other as its static `--spec-msg-peer`. Zombienet derives a node's key from
//! its name, so both peer ids are known before spawn.
//!
//! The delivery test logs one checkpoint per protocol step, each read from chain state: the
//! sender's committed root, the relay's provides window, the receiver's consumption record, and
//! the executed XCM. It then sends a batch larger than the receiver admits per block, which the
//! receiver consumes over several blocks cut short of what its node planned; stops the sender's
//! collator while the receiver holds a backlog; and fills the receiver's queue until the sender's
//! window is full, so the sender refuses sends and raises its fee.

use crate::utils::initialize_network;
use anyhow::anyhow;
use codec::{Decode, Encode};
use cumulus_primitives_core::ParaId;
use cumulus_primitives_spec_messaging::{
	streams_root::streams_root, ConsumptionRecord, InChannelId, InChannelState, MmrFrontier,
	OutChannelId, OutChannelState, StreamId, SPMS_ENGINE_ID,
};
use cumulus_zombienet_sdk_helpers::{assert_para_throughput, open_hrmp_channel};
use std::{collections::BTreeMap, future::Future, time::Duration};
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{
		self,
		backend::StreamOfResults,
		blocks::Block,
		config::{polkadot::PolkadotExtrinsicParamsBuilder, substrate::DigestItem},
		dynamic::Value,
		ext::scale_value::{At, Primitive, ValueDef},
		tx::TxStatus,
		utils::H256,
		OnlineClient, PolkadotConfig,
	},
	subxt_signer::sr25519::dev,
	AddCollatorOptions, LocalFileSystem, Network, NetworkConfig, NetworkConfigBuilder,
};

const SENDER: u32 = 2000;
const RECEIVER: u32 = 2001;
const SENDER_COLLATOR: &str = "charlie";
const RECEIVER_COLLATOR: &str = "dave";
const SENDER_P2P_PORT: u16 = 30_500;
const RECEIVER_P2P_PORT: u16 = 30_501;
const RELAY_RPC_PORT: u16 = 30_510;
const SENDER_RPC_PORT: u16 = 30_511;
const RECEIVER_RPC_PORT: u16 = 30_512;
/// A full node of the sender chain that joins late, and the only one the receiver fetches from.
const LATE_NODE: &str = "eve";
const LATE_P2P_PORT: u16 = 30_502;
/// The late node's state pruning window, in blocks: short, so the test passes it quickly.
const LATE_PRUNING: u32 = 16;

/// The XCM channel `SENDER -> RECEIVER`, as the receiver names it.
fn in_channel() -> InChannelId {
	InChannelId { sender: SENDER.into(), domain: 0, num: 0 }
}

/// The XCM channel `SENDER -> RECEIVER`, as the sender names it.
fn out_channel() -> OutChannelId {
	OutChannelId { recipient: RECEIVER.into(), domain: 0, num: 0 }
}

fn flag(name: &str) -> bool {
	std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Whether to run the delivery test's step `name`: every step unless `SPEC_MSG_STEPS` lists some,
/// comma-separated (`deliver`, `batch`, `limits`, `outage`, `backpressure`), to rerun only failing
/// ones.
fn step(name: &str) -> bool {
	std::env::var("SPEC_MSG_STEPS").map_or(true, |steps| steps.split(',').any(|s| s.trim() == name))
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_msg_delivers_xcm_without_hrmp() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let (network, relay, sender, receiver) = if flag("SPEC_MSG_ATTACH") {
		log::info!("Attaching to standby network on ports {RELAY_RPC_PORT}..={RECEIVER_RPC_PORT}");
		let connect =
			|port| OnlineClient::<PolkadotConfig>::from_url(format!("ws://127.0.0.1:{port}"));
		(
			None,
			connect(RELAY_RPC_PORT).await?,
			connect(SENDER_RPC_PORT).await?,
			connect(RECEIVER_RPC_PORT).await?,
		)
	} else {
		let network =
			initialize_network(build_network_config((SENDER_COLLATOR, SENDER_P2P_PORT), false)?)
				.await?;
		let relay: OnlineClient<PolkadotConfig> = network.get_node("alice")?.wait_client().await?;
		let sender: OnlineClient<PolkadotConfig> =
			network.get_node(SENDER_COLLATOR)?.wait_client().await?;
		let receiver: OnlineClient<PolkadotConfig> =
			network.get_node(RECEIVER_COLLATOR)?.wait_client().await?;

		let start = (finalized_number(&sender).await?, finalized_number(&receiver).await?);
		wait_for("both chains to finalize a block", || {
			advanced(&sender, &receiver, (start.0 + 1, start.1 + 1))
		})
		.await?;

		if flag("SPEC_MSG_STANDBY") {
			log::info!(
				"Standby ready: {SENDER} and {RECEIVER} are producing blocks. \
				 Run with SPEC_MSG_ATTACH=1 in another terminal (Ctrl-C to stop)."
			);
			tokio::signal::ctrl_c().await?;
			network.destroy().await?;
			return Ok(());
		}
		(Some(network), relay, sender, receiver)
	};

	if hrmp_channel_open(&relay, SENDER, RECEIVER).await? {
		return Err(anyhow!("an HRMP channel {SENDER} -> {RECEIVER} exists"));
	}
	log::info!("No HRMP channel {SENDER} -> {RECEIVER} on the relay chain");

	open_spec_msg_channel(&sender, &receiver).await?;

	if step("deliver") {
		send_and_follow(&relay, &sender, &receiver).await?;
	}
	if step("batch") {
		send_batch_and_follow(&sender, &receiver).await?;
	}
	if step("limits") {
		largest_and_follow(&sender, &receiver).await?;
	}
	if step("outage") {
		match &network {
			Some(network) => outage_and_follow(network, &sender, &receiver).await?,
			None => log::info!("Attached: no network handle to pause a collator, skipping it"),
		}
	}
	if step("backpressure") {
		backpressure_and_follow(&sender, &receiver).await?;
	}

	let now = (finalized_number(&sender).await?, finalized_number(&receiver).await?);
	wait_for("both chains to finalize one more block with messaging live", || {
		advanced(&sender, &receiver, (now.0 + 1, now.1 + 1))
	})
	.await?;

	Ok(())
}

/// Send an XCM `SENDER -> RECEIVER` over the open channel and follow it to execution, logging
/// checkpoints 2 to 6.
async fn send_and_follow(
	relay: &OnlineClient<PolkadotConfig>,
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	log::info!("Sending an XCM {SENDER} -> {RECEIVER}");
	// Subscribe first: the receiver may consume the message before the send is finalized. The
	// stream starts at the block finalized now, which predates the send. Skip every block up to the
	// best one now: a block that consumes the message comes later, and an earlier one that is not
	// yet finalized, such as the one that consumed the open signal, is not the message.
	let before_send = receiver.blocks().at(best_hash(receiver).await?).await?.number();
	let receiver_blocks = receiver.blocks().subscribe_finalized().await?;
	let sent = sender.blocks().at(sudo(sender, send_xcm_to(RECEIVER)).await?).await?;
	let root = committed_root(&sent)
		.ok_or_else(|| anyhow!("send block #{} commits no `StreamsRoot`", sent.number()))?;
	let id = event_field(&sent, "PolkadotXcm", "Sent", "message_id")
		.await?
		.map(|id| flatten(&id))
		.filter(|id| id.len() == 32)
		.map(|id| H256::from_slice(&id))
		.ok_or_else(|| anyhow!("no `PolkadotXcm::Sent` message id in the send block"))?;
	log::info!(
		"Checkpoint 2/6, send: {SENDER} block #{} holds `PolkadotXcm::Sent` with message id \
		 {id:?} and commits `StreamsRoot` {root:?} in its SPMS digest, its `Provides`",
		sent.number(),
	);
	let committed = committed_stream_roots(&sent).await?;
	let entries = committed.iter().map(|(stream, f)| (*stream, f.root().0)).collect();
	if streams_root(&entries).map(|r| r.0 .0) != Some(root.0) {
		return Err(anyhow!("{SENDER}'s stream state does not rebuild the committed root"));
	}

	// A bundle provides only its last root, so the relay records that root or a later one.
	let newest_from_send = || async {
		let newest: Option<H256> = call_at(
			relay,
			None,
			"ParachainHost_newest_included_provides",
			&ParaId::from(SENDER).encode(),
		)
		.await?;
		match newest {
			Some(newest) => committed_since(sender, newest, sent.number()).await,
			None => Ok(None),
		}
	};
	wait_for("the relay chain to hold a root from the send block on", || async {
		Ok(newest_from_send().await?.is_some())
	})
	.await?;
	let provided_by = newest_from_send().await?.unwrap_or_default();
	log::info!(
		"Checkpoint 3/6, relay: at relay block #{}, `newest_included_provides({SENDER})` is the \
		 root {SENDER} block #{provided_by} committed, at or after the send: its candidate was \
		 included and its `Provides` recorded",
		relay.blocks().at_latest().await?.number(),
	);

	follow_delivery(receiver, receiver_blocks, before_send, id, root, &committed).await?;
	Ok(())
}

/// XCMs in the batch: more than twice penpal's `MaxInboundMessagesPerItem`.
const BATCH: u64 = 20;
const MAX_PER_ITEM: u64 = 8;

/// Send `BATCH` XCMs `SENDER -> RECEIVER` in one block and follow them to execution. The
/// receiver's node binds the channel only at the batch's end; the runtime admits `MAX_PER_ITEM`
/// per block, so the blocks before the last end at cut endpoints whose lifts the node rebases.
/// Each of them is finalized, so the relay accepted its `Requires`.
async fn send_batch_and_follow(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	log::info!("Sending {BATCH} XCMs {SENDER} -> {RECEIVER} in one block");
	let stream = in_channel().sender_data_stream(RECEIVER.into()).1;
	let mut consumed = channel_map::<InChannelId, InChannelState>(receiver, "InChannels")
		.await?
		.get(&in_channel())
		.map(|state| state.frontier.leaf_count())
		.ok_or_else(|| anyhow!("{RECEIVER} has no inbound channel from {SENDER}"))?;
	let end = consumed + BATCH;
	let before_send = receiver.blocks().at(best_hash(receiver).await?).await?.number();
	let mut blocks = receiver.blocks().subscribe_finalized().await?;
	let calls = (0..BATCH).map(|_| send_xcm_to(RECEIVER)).collect::<Vec<_>>();
	let batch = Value::unnamed_variant(
		"Utility",
		[Value::named_variant("batch_all", [("calls", Value::unnamed_composite(calls))])],
	);
	let sent = sender.blocks().at(sudo(sender, batch).await?).await?;
	let mut ids = Vec::new();
	for event in sent.events().await?.iter() {
		let event = event?;
		if event.pallet_name() == "PolkadotXcm" && event.variant_name() == "Sent" {
			ids.extend(event.field_values()?.at("message_id").map(flatten));
		}
	}
	if ids.len() as u64 != BATCH {
		return Err(anyhow!(
			"{SENDER} block #{} sent {} XCMs, not {BATCH}",
			sent.number(),
			ids.len()
		));
	}
	log::info!("{SENDER} block #{} sent leaves {}..{end}", sent.number(), end - BATCH);

	let mut cuts = 0;
	let mut processed = Vec::new();
	let follow = async {
		while let Some(block) = blocks.next().await {
			let block = block?;
			if block.number() <= before_send {
				continue;
			}
			let record: ConsumptionRecord =
				call_at(receiver, Some(block.hash()), "SpecMsgApi_consumption_record", &[]).await?;
			let reached = record.entries.get(&ParaId::from(SENDER)).and_then(|streams| {
				streams
					.iter()
					.find_map(|(s, interval)| (*s == stream).then(|| interval.end.leaf_count()))
			});
			if let Some(reached) = reached {
				if reached - consumed > MAX_PER_ITEM {
					return Err(anyhow!(
						"{RECEIVER} block #{} consumed {consumed}..{reached}, over {MAX_PER_ITEM}",
						block.number(),
					));
				}
				if reached < end {
					cuts += 1;
				}
				log::info!(
					"{RECEIVER} block #{} consumed leaves {consumed}..{reached} and is finalized",
					block.number(),
				);
				consumed = reached;
			}
			for event in block.events().await?.iter() {
				let event = event?;
				if event.pallet_name() != "MessageQueue" || event.variant_name() != "Processed" {
					continue;
				}
				let values = event.field_values()?;
				let Some(id) = values.at("id").map(flatten).filter(|id| ids.contains(id)) else {
					continue;
				};
				let fields = format!("{values:?}");
				if !fields.contains("SpecMsg") || !fields.contains("Bool(true)") {
					return Err(anyhow!("XCM {id:?} not executed over spec-msg: {fields}"));
				}
				processed.push(id);
			}
			if processed.len() == ids.len() {
				return Ok(());
			}
		}
		Err(anyhow!("receiver block subscription ended"))
	};
	tokio::time::timeout(Duration::from_secs(300), follow)
		.await
		.map_err(|_| anyhow!("the batch was not executed within 5 minutes"))??;

	if processed != ids {
		return Err(anyhow!("the batch executed out of order"));
	}
	// `pallet-xcm` may send a small leaf after the batch, which the last block can consume too.
	if consumed < end || cuts < 2 {
		return Err(anyhow!("consumed up to {consumed} of {end} with {cuts} cut blocks"));
	}
	log::info!(
		"All {BATCH} XCMs executed in order over {} consuming blocks, {cuts} of them cut short",
		cuts + 1,
	);
	Ok(())
}

/// XCMs the receiver holds when the sender's collator stops: several blocks' worth.
const BACKLOG: u64 = 32;

/// Send `BACKLOG` XCMs, and once the receiver consumed the first of them, so its node has
/// fetched them all, stop the sender chain's only collator. The receiver consumes the rest from
/// what its node holds, rebasing each block's lift locally, and the relay chain accepts those
/// blocks against the sender root it included before. Then resume the collator.
async fn outage_and_follow(
	network: &Network<LocalFileSystem>,
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	let start = inbound_count(receiver).await?;
	let end = start + BACKLOG;
	let mut blocks = receiver.blocks().subscribe_finalized().await?;
	let calls = (0..BACKLOG).map(|_| send_xcm_to(RECEIVER)).collect::<Vec<_>>();
	let batch = Value::unnamed_variant(
		"Utility",
		[Value::named_variant("batch_all", [("calls", Value::unnamed_composite(calls))])],
	);
	let ids = sudo_sent(sender, batch).await?;
	log::info!("{SENDER} sent leaves {start}..{end}");

	// Pause once the receiver's best block consumed some: its node fetched the whole backlog.
	let collator = network.get_node(SENDER_COLLATOR)?;
	let mut paused_at = start;
	for _ in 0..120 {
		paused_at = inbound_count(receiver).await?;
		if paused_at > start {
			break;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	if paused_at == start || paused_at == end {
		return Err(anyhow!(
			"{RECEIVER} consumed up to {paused_at} of {start}..{end} before a pause"
		));
	}
	collator.pause().await?;
	log::info!(
		"Paused {SENDER_COLLATOR}, {SENDER}'s only collator, with leaf {paused_at} consumed"
	);

	let mut processed = Vec::new();
	let follow = async {
		while let Some(block) = blocks.next().await {
			let block = block?;
			processed.extend(processed_ids(&block, &ids).await?);
			if processed.len() == ids.len() {
				return Ok(block.number());
			}
		}
		Err(anyhow!("receiver block subscription ended"))
	};
	let followed = tokio::time::timeout(Duration::from_secs(240), follow).await;
	collator.resume().await?;
	let done = followed.map_err(|_| {
		anyhow!("{} of {BACKLOG} XCMs executed within 4 minutes of the outage", processed.len())
	})??;
	if processed != ids {
		return Err(anyhow!("the backlog executed out of order"));
	}
	log::info!(
		"While {SENDER_COLLATOR} was paused, {RECEIVER} consumed leaves {paused_at}..{end} from \
		 what its node held; all {BACKLOG} executed in order by block #{done}"
	);
	Ok(())
}

/// Penpal's `MaxMsgLen`: the largest leaf a channel carries.
const MAX_LEAF: usize = 102_400;
/// The remark length that makes [`send_large_xcm_to`]'s leaf exactly `MAX_LEAF` bytes: the `Data`
/// envelope (5 bytes), the versioned XCM around the `Transact` (10), the remark call (5), and the
/// `SetTopic` penpal's `WithUniqueTopic` router appends (33).
const LARGEST_REMARK: usize = MAX_LEAF - 53;
/// XCMs in the limits step: more than one fetch response holds (1 MiB), and several blocks of the
/// receiver node's default byte budget.
const LARGEST_COUNT: u64 = 12;
/// The receiver node's default byte budget per block (`Budget::max_bytes`): two largest leaves.
const NODE_MAX_BYTES: usize = 256 * 1024;

/// Send one XCM a byte over the largest the channel carries, which the router refuses, then
/// `LARGEST_COUNT` of the largest. Check each leaf is exactly `MAX_LEAF` bytes, and follow them:
/// every receiver block consumes at most what the node's byte budget holds, is finalized, so the
/// relay accepted its `Requires`, and the queue settles them all.
async fn largest_and_follow(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	let before = sent_count(sender).await?;
	let too_large = send_large_xcm_to(RECEIVER, 0, LARGEST_REMARK + 1);
	if !force_batch(sender, vec![too_large]).await?.is_empty() ||
		sent_count(sender).await? != before
	{
		return Err(anyhow!("a leaf of {} bytes was sent", MAX_LEAF + 1));
	}
	log::info!("{SENDER}'s router refused an XCM whose leaf is {} bytes", MAX_LEAF + 1);

	let stream = in_channel().sender_data_stream(RECEIVER.into()).1;
	let mut consumed = inbound_count(receiver).await?;
	let end = consumed + LARGEST_COUNT;
	let before_send = receiver.blocks().at(best_hash(receiver).await?).await?.number();
	let mut blocks = receiver.blocks().subscribe_finalized().await?;
	let calls = (0..LARGEST_COUNT).map(|i| send_large_xcm_to(RECEIVER, i as u8, LARGEST_REMARK));
	let batch = Value::unnamed_variant(
		"Utility",
		[Value::named_variant("batch_all", [("calls", Value::unnamed_composite(calls))])],
	);
	let (hash, ids) = submit_sudo(sender, batch, true).await?;
	let sends: Vec<(StreamId, Vec<Vec<u8>>)> =
		call_at(sender, Some(hash), "SpecMsgApi_outbound_messages", &[]).await?;
	let leaves = sends
		.iter()
		.find_map(|(s, leaves)| (*s == out_channel().data_stream()).then_some(leaves))
		.ok_or_else(|| anyhow!("the send block holds no leaves on the channel"))?;
	if ids.len() as u64 != LARGEST_COUNT || leaves.iter().any(|leaf| leaf.len() != MAX_LEAF) {
		return Err(anyhow!(
			"sent {} XCMs with leaves of {:?} bytes, not {LARGEST_COUNT} of {MAX_LEAF}",
			ids.len(),
			leaves.iter().map(Vec::len).collect::<Vec<_>>(),
		));
	}
	log::info!("{SENDER} sent leaves {}..{end} of exactly {MAX_LEAF} bytes", end - LARGEST_COUNT);

	let per_block = (NODE_MAX_BYTES / MAX_LEAF) as u64;
	let mut cuts = 0;
	let follow = async {
		while let Some(block) = blocks.next().await {
			let block = block?;
			if block.number() <= before_send {
				continue;
			}
			let record: ConsumptionRecord =
				call_at(receiver, Some(block.hash()), "SpecMsgApi_consumption_record", &[]).await?;
			let reached = record.entries.get(&ParaId::from(SENDER)).and_then(|streams| {
				streams
					.iter()
					.find_map(|(s, interval)| (*s == stream).then(|| interval.end.leaf_count()))
			});
			let Some(reached) = reached else { continue };
			// Only the batch's leaves are the largest: `pallet-xcm` may add a small one after them.
			let largest = reached.min(end) - consumed.min(end);
			if largest > per_block {
				return Err(anyhow!(
					"{RECEIVER} block #{} consumed {largest} of the largest leaves, over the \
					 node's budget of {per_block}",
					block.number(),
				));
			}
			if reached < end {
				cuts += 1;
			}
			log::info!(
				"{RECEIVER} block #{} consumed leaves {consumed}..{reached} and is finalized",
				block.number(),
			);
			consumed = reached;
			if consumed >= end {
				return Ok(());
			}
		}
		Err(anyhow!("receiver block subscription ended"))
	};
	tokio::time::timeout(Duration::from_secs(300), follow)
		.await
		.map_err(|_| anyhow!("consumed up to {consumed} of {end} within 5 minutes"))??;
	if cuts < 2 {
		return Err(anyhow!("only {cuts} blocks were cut short by the node's byte budget"));
	}
	wait_for("the receiver's queue to settle every XCM", || async {
		Ok(queued(receiver).await? == 0)
	})
	.await?;
	log::info!(
		"All {LARGEST_COUNT} largest XCMs consumed over {} finalized blocks, at most {per_block} \
		 each, and settled",
		cuts + 1,
	);
	Ok(())
}

/// Bytes of each large XCM in the backpressure step: over half a message queue page, so each
/// takes a page.
const LARGE_XCM: usize = 60 * 1024;
/// Large XCMs that fill penpal's `MaxQueueFootprint` (48 pages), and a few more.
const LARGE_COUNT: usize = 56;
/// Penpal's `MaxInFlight`.
const MAX_IN_FLIGHT: u64 = 128;

/// Pause the receiver's queue, fill it with large XCMs until its consumption stops, then fill the
/// sender's window with small ones until it refuses: the receiver's `Ack` stopped, so the sender
/// raises its fee. Resume, check that the receiver consumes the whole stream and its queue settles
/// every message, and that a send with the window drained lowers the fee.
async fn backpressure_and_follow(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	let queue = |call: &str| {
		Value::unnamed_variant(
			"SpecMessaging",
			[Value::unnamed_variant(
				call,
				[Value::u128(SENDER.into()), Value::u128(0), Value::u128(0)],
			)],
		)
	};
	sudo(receiver, queue("pause_inbound_channel")).await?;
	log::info!("{RECEIVER} paused its queue for {SENDER}'s channel");

	let mut large = 0;
	for chunk in 0..LARGE_COUNT / 8 {
		let calls = (0..8).map(|i| send_large_xcm_to(RECEIVER, (chunk * 8 + i) as u8, LARGE_XCM));
		large += force_batch(sender, calls.collect()).await?.len();
	}
	log::info!("{SENDER} sent {large} XCMs of {LARGE_XCM} bytes");

	let mut refused = false;
	for _ in 0..40 {
		let sent = force_batch(sender, (0..32).map(|_| send_xcm_to(RECEIVER)).collect()).await?;
		let in_flight = in_flight(sender).await?;
		log::info!("{SENDER} sent {} small XCMs, {in_flight} in flight", sent.len());
		if in_flight > MAX_IN_FLIGHT {
			return Err(anyhow!("{in_flight} in flight, over the window of {MAX_IN_FLIGHT}"));
		}
		refused = sent.is_empty() && in_flight == MAX_IN_FLIGHT;
		if refused {
			break;
		}
	}
	if !refused {
		return Err(anyhow!("the sender never refused a send"));
	}
	let raised = fee_factor(sender).await?;
	if raised <= FEE_FACTOR_ONE {
		return Err(anyhow!("the fee factor did not rise: {raised}"));
	}
	let held = queued(receiver).await?;
	if held == 0 {
		return Err(anyhow!("{RECEIVER}'s paused queue reads as empty"));
	}
	log::info!(
		"{SENDER} refuses sends with {MAX_IN_FLIGHT} in flight, fee factor {raised}; {RECEIVER} \
		 stopped consuming at leaf {} with {held} XCMs in its paused queue",
		inbound_count(receiver).await?,
	);

	// Ids of sends in a block that is later retracted change when the sends are included again,
	// so delivery is checked on the stream instead: the receiver consumes all of it, in order as
	// the PVF checks, and its queue for the channel settles every message.
	sudo(receiver, queue("resume_inbound_channel")).await?;
	log::info!("{RECEIVER} resumed its queue");
	let sent = sent_count(sender).await?;
	let mut drained = false;
	for _ in 0..50 {
		let (consumed, queued) = (inbound_count(receiver).await?, queued(receiver).await?);
		log::info!("{RECEIVER} consumed {consumed} of {sent} leaves, {queued} XCMs in its queue");
		if consumed == sent && queued == 0 {
			drained = true;
			break;
		}
		tokio::time::sleep(Duration::from_secs(6)).await;
	}
	if !drained {
		return Err(anyhow!(
			"{RECEIVER} did not consume and settle {SENDER}'s stream in 5 minutes"
		));
	}
	log::info!("{RECEIVER} consumed all {sent} leaves and settled every XCM in its queue");

	wait_for("the sender to read that its window drained", || async {
		Ok(in_flight(sender).await? <= MAX_IN_FLIGHT / 2)
	})
	.await?;
	sudo(sender, send_xcm_to(RECEIVER)).await?;
	let lowered = fee_factor(sender).await?;
	if lowered >= raised {
		return Err(anyhow!("the fee factor did not fall: {raised} then {lowered}"));
	}
	log::info!("A send with the window drained lowered the fee factor from {raised} to {lowered}");
	Ok(())
}

/// Dispatch `calls` as root in one `Utility::force_batch`, in a best block; the ids of the XCMs
/// it sent.
async fn force_batch(
	sender: &OnlineClient<PolkadotConfig>,
	calls: Vec<Value>,
) -> Result<Vec<Vec<u8>>, anyhow::Error> {
	let batch = Value::unnamed_variant(
		"Utility",
		[Value::named_variant("force_batch", [("calls", Value::unnamed_composite(calls))])],
	);
	sudo_sent(sender, batch).await
}

/// The XCMs that came over spec-msg and that `block`'s message queue settled, in order:
/// processed, failed or set aside as overweight. Each is its id if among `ids`, else `None`: an
/// XCM refused before execution, as the large ones are, is named by its hash, not its topic.
async fn settled_ids(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	ids: &[Vec<u8>],
) -> Result<Vec<Option<Vec<u8>>>, anyhow::Error> {
	let mut settled = Vec::new();
	for event in block.events().await?.iter() {
		let event = event?;
		let settles = ["Processed", "ProcessingFailed", "OverweightEnqueued"];
		if event.pallet_name() != "MessageQueue" || !settles.contains(&event.variant_name()) {
			continue;
		}
		let values = event.field_values()?;
		if !format!("{values:?}").contains("SpecMsg") {
			continue;
		}
		settled.push(values.at("id").map(flatten).filter(|id| ids.contains(id)));
	}
	Ok(settled)
}

/// The ids among `ids` that `block`'s message queue settled over spec-msg, in order.
async fn processed_ids(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	ids: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, anyhow::Error> {
	Ok(settled_ids(block, ids).await?.into_iter().flatten().collect())
}

/// The leaves the receiver consumed from the sender's channel, at its best block.
async fn inbound_count(receiver: &OnlineClient<PolkadotConfig>) -> Result<u64, anyhow::Error> {
	channel_map::<InChannelId, InChannelState>(receiver, "InChannels")
		.await?
		.get(&in_channel())
		.map(|state| state.frontier.leaf_count())
		.ok_or_else(|| anyhow!("{RECEIVER} has no inbound channel from {SENDER}"))
}

/// `FixedU128`'s one.
const FEE_FACTOR_ONE: u128 = 1_000_000_000_000_000_000;

/// The sender's delivery fee factor towards the receiver, at its best block.
async fn fee_factor(sender: &OnlineClient<PolkadotConfig>) -> Result<u128, anyhow::Error> {
	let query = subxt::dynamic::storage(
		"SpecMessaging",
		"DeliveryFeeFactor",
		vec![Value::u128(RECEIVER.into())],
	);
	let value = sender.storage().at(best_hash(sender).await?).fetch(&query).await?;
	match value {
		Some(value) => Ok(u128::decode(&mut &value.into_encoded()[..])?),
		None => Ok(FEE_FACTOR_ONE),
	}
}

/// The leaves the sender has sent on the channel, at its best block.
async fn sent_count(sender: &OnlineClient<PolkadotConfig>) -> Result<u64, anyhow::Error> {
	let stream = out_channel().data_stream();
	let frontiers: Vec<(StreamId, MmrFrontier)> =
		call_at(sender, Some(best_hash(sender).await?), "SpecMsgApi_outbound_frontiers", &[])
			.await?;
	Ok(frontiers
		.iter()
		.find_map(|(s, frontier)| (*s == stream).then(|| frontier.leaf_count()))
		.unwrap_or_default())
}

/// The messages the receiver's queue for the channel holds, at its best block.
async fn queued(receiver: &OnlineClient<PolkadotConfig>) -> Result<u64, anyhow::Error> {
	let origin = Value::unnamed_variant(
		"SpecMsg",
		[Value::named_composite([
			("sender", Value::u128(SENDER.into())),
			("domain", Value::u128(0)),
			("num", Value::u128(0)),
		])],
	);
	let query = subxt::dynamic::storage("MessageQueue", "BookStateFor", vec![origin]);
	let book = receiver.storage().at(best_hash(receiver).await?).fetch(&query).await?;
	let Some(book) = book else { return Ok(0) };
	let book = book.to_value()?;
	book.at("message_count")
		.and_then(|count| count.as_u128())
		.map(|count| count as u64)
		.ok_or_else(|| anyhow!("no `message_count` in the queue's book"))
}

/// The sender's leaves on the channel that the receiver has not confirmed, at its best block.
async fn in_flight(sender: &OnlineClient<PolkadotConfig>) -> Result<u64, anyhow::Error> {
	let sent = sent_count(sender).await?;
	let up_to = channel_map::<OutChannelId, OutChannelState>(sender, "OutChannels")
		.await?
		.get(&out_channel())
		.and_then(|state| state.register.as_ref().map(|applied| applied.register.up_to.0))
		.unwrap_or_default();
	Ok(sent.saturating_sub(up_to))
}

/// `PolkadotXcm::send` to `para` of a `Transact` of a `System::remark` of `len` bytes, which the
/// receiver weighs and then refuses at its barrier.
fn send_large_xcm_to(para: u32, tag: u8, len: usize) -> Value {
	// `System` is pallet 0 and `remark` its call 0 on penpal.
	let call = [vec![0u8, 0u8], vec![tag; len].encode()].concat();
	let transact = Value::named_variant(
		"Transact",
		[
			("origin_kind", Value::unnamed_variant("SovereignAccount", [])),
			("fallback_max_weight", Value::unnamed_variant("None", [])),
			("call", Value::named_composite([("encoded", Value::from_bytes(call))])),
		],
	);
	let message = Value::unnamed_variant("V5", [Value::unnamed_composite([transact])]);
	Value::unnamed_variant(
		"PolkadotXcm",
		[Value::named_variant("send", [("dest", sibling(para)), ("message", message)])],
	)
}

/// How the late node syncs the sender chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Join {
	/// Major sync, executing every block.
	Full,
	/// Warp sync: state only from the warp target on.
	Warp,
}

/// A late sender-chain node that prunes state, and alone serves the receiver, opens the channel:
/// the receiver consumes the `Hello` the sender sent before the node joined, then an XCM. The node
/// major-syncs and archives each block's sends while its import notification pins the state.
#[tokio::test(flavor = "multi_thread")]
async fn spec_msg_late_full_sync_node_serves_history() -> Result<(), anyhow::Error> {
	late_node_serves_history(Join::Full).await
}

/// As [`spec_msg_late_full_sync_node_serves_history`], but the node warp-syncs: it seeds its
/// archive at its finalized block and backfills the `Hello` from the sender collator.
#[tokio::test(flavor = "multi_thread")]
async fn spec_msg_late_warp_sync_node_serves_history() -> Result<(), anyhow::Error> {
	late_node_serves_history(Join::Warp).await
}

async fn late_node_serves_history(join: Join) -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);
	let mut network =
		initialize_network(build_network_config((LATE_NODE, LATE_P2P_PORT), false)?).await?;
	// Both late-node tests bind the same ports: free them before the next one starts.
	let result = late_node_steps(&mut network, join).await;
	network.destroy().await?;
	result
}

async fn late_node_steps(
	network: &mut Network<LocalFileSystem>,
	join: Join,
) -> Result<(), anyhow::Error> {
	let relay: OnlineClient<PolkadotConfig> = network.get_node("alice")?.wait_client().await?;
	let sender: OnlineClient<PolkadotConfig> =
		network.get_node(SENDER_COLLATOR)?.wait_client().await?;
	let receiver: OnlineClient<PolkadotConfig> =
		network.get_node(RECEIVER_COLLATOR)?.wait_client().await?;
	let start = (finalized_number(&sender).await?, finalized_number(&receiver).await?);
	wait_for("both chains to finalize a block", || {
		advanced(&sender, &receiver, (start.0 + 1, start.1 + 1))
	})
	.await?;

	// The receiver accepts and the sender sends `Hello`, which nothing serves the receiver yet.
	submit_open(&sender, &receiver).await?;
	wait_for("the sender to send `Hello`", || async {
		let (_, out_channels) = channel_states(&sender, &receiver).await?;
		Ok(out_channels.get(&out_channel()).is_some_and(|state| state.open.is_some()))
	})
	.await?;
	let hello = sender.blocks().at(best_hash(&sender).await?).await?.number();
	wait_for("the sender to finalize past the late node's pruning window", || async {
		Ok(finalized_number(&sender).await? > hello + LATE_PRUNING + 4)
	})
	.await?;

	log::info!("Adding {LATE_NODE}, a {join:?}-syncing full node of {SENDER}");
	let mut args: Vec<zombienet_sdk::Arg> = vec![
		("-lsync=info,spec-msg=debug").into(),
		("--enable-spec-msg").into(),
		("--state-pruning", LATE_PRUNING.to_string().as_str()).into(),
		("--spec-msg-peer", peer_arg(SENDER, SENDER_COLLATOR, SENDER_P2P_PORT)?.as_str()).into(),
		("--spec-msg-peer", peer_arg(RECEIVER, RECEIVER_COLLATOR, RECEIVER_P2P_PORT)?.as_str())
			.into(),
		("--relay-chain-rpc-urls", "{{ZOMBIE:alice:ws_uri}}").into(),
	];
	if join == Join::Warp {
		args.push(("--sync=warp").into());
	}
	let options = AddCollatorOptions {
		is_validator: false,
		args,
		p2p_port: Some(LATE_P2P_PORT),
		..Default::default()
	};
	network.add_collator(LATE_NODE, options, SENDER).await?;

	wait_opened(&sender, &receiver).await?;
	log::info!("{LATE_NODE} served the `Hello` from #{hello}: the channel is open");
	send_and_follow(&relay, &sender, &receiver).await?;

	// How the late node got the history: seeded and backfilled after a warp sync, else not.
	let node = network.get_node(LATE_NODE)?;
	let seeded = if join == Join::Warp { 1 } else { 0 };
	for (pattern, count) in [("seeded the archive", seeded), ("Backfilled Channel", seeded)] {
		let options = LogLineCountOptions::new(move |n| n == count, Duration::from_secs(10), false);
		if !node.wait_log_line_count_with_timeout(pattern, false, options).await?.success() {
			return Err(anyhow!("{LATE_NODE} logged \"{pattern}\" other than {count} times"));
		}
	}
	Ok(())
}

/// Samples per path.
const SAMPLES: usize = 10;

/// Samples per path: `SPEC_MSG_SAMPLES`, or [`SAMPLES`].
fn samples() -> usize {
	std::env::var("SPEC_MSG_SAMPLES")
		.ok()
		.and_then(|n| n.parse().ok())
		.unwrap_or(SAMPLES)
}

/// XCM delivery latency `SENDER -> RECEIVER`, spec-msg against HRMP, on one network.
///
/// Phase 1 sends over the spec-msg channel. Phase 2 force-opens an HRMP channel; the router then
/// prefers HRMP, so the same send goes over XCMP. Each phase sends one unmeasured message to
/// confirm the path, then [`SAMPLES`] messages one at a time, each processed before the next is
/// sent. A sample is the time from the sender block holding `PolkadotXcm::Sent` to the receiver
/// block holding the matching `MessageQueue::Processed`, in relay blocks between their relay
/// parents. Not in milliseconds: with slot-based authoring, block timestamps step in Aura slots.
///
/// A measurement, not a check: run it by hand with `--ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn spec_msg_vs_hrmp_latency() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network =
		initialize_network(build_network_config((SENDER_COLLATOR, SENDER_P2P_PORT), false)?)
			.await?;
	let relay: OnlineClient<PolkadotConfig> = network.get_node("alice")?.wait_client().await?;
	let sender: OnlineClient<PolkadotConfig> =
		network.get_node(SENDER_COLLATOR)?.wait_client().await?;
	let receiver: OnlineClient<PolkadotConfig> =
		network.get_node(RECEIVER_COLLATOR)?.wait_client().await?;
	assert_para_throughput(
		&relay,
		10,
		[(ParaId::from(SENDER), 5..11), (ParaId::from(RECEIVER), 5..11)],
		[],
	)
	.await?;

	let inclusions = track_inclusions(relay.clone());
	open_spec_msg_channel(&sender, &receiver).await?;
	let spec_msg = measure(&sender, &receiver, &inclusions, "SpecMsg").await?;

	log::info!("Force-opening HRMP {SENDER} -> {RECEIVER}");
	open_hrmp_channel(&relay, SENDER, RECEIVER, 8, 1024, &dev::alice(), 120).await?;
	wait_for("the HRMP channel to open on the relay chain", || {
		hrmp_channel_open(&relay, SENDER, RECEIVER)
	})
	.await?;
	let hrmp = measure(&sender, &receiver, &inclusions, "Sibling").await?;

	report("spec-msg", &spec_msg);
	report("HRMP", &hrmp);
	Ok(())
}

/// One delivery: relay blocks from send to processing.
#[derive(Clone, Copy, Debug)]
struct Sample {
	relay_blocks: u32,
	/// Relay parent of the send block.
	sent_relay: u32,
	/// Relay block that included the send block.
	included_relay: u32,
	/// Relay parent of the block that processed the message.
	processed_relay: u32,
	sent_block: u32,
	processed_block: u32,
}

/// Relay block number of each included parachain head, by head hash.
type Inclusions = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], u32>>>;

/// Record every `ParaInclusion::CandidateIncluded` on the relay chain's finalized blocks.
fn track_inclusions(relay: OnlineClient<PolkadotConfig>) -> Inclusions {
	let inclusions = Inclusions::default();
	let map = inclusions.clone();
	tokio::spawn(async move {
		let Ok(mut blocks) = relay.blocks().subscribe_finalized().await else { return };
		while let Some(Ok(block)) = blocks.next().await {
			let Ok(events) = block.events().await else { continue };
			for event in events.iter().flatten() {
				if event.pallet_name() != "ParaInclusion" ||
					event.variant_name() != "CandidateIncluded"
				{
					continue;
				}
				let Ok(fields) = event.field_values() else { continue };
				if let Some(head) = fields.at(1) {
					let hash = sp_crypto_hashing::blake2_256(&flatten(head));
					map.lock().unwrap().insert(hash, block.number());
				}
			}
		}
	});
	inclusions
}

/// The relay block that included `head`, waiting up to a minute for it to finalize.
async fn included_at(inclusions: &Inclusions, head: H256) -> Result<u32, anyhow::Error> {
	for _ in 0..60 {
		if let Some(number) = inclusions.lock().unwrap().get(&head.0) {
			return Ok(*number);
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	Err(anyhow!("no inclusion of {head:?} seen"))
}

/// The longest the fork test sends traffic while waiting for both rare events:
/// `SPEC_MSG_FORK_SECS`, 1200 by default.
fn fork_secs() -> u64 {
	std::env::var("SPEC_MSG_FORK_SECS")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(1200)
}

/// What the fork test saw on the relay chain.
#[derive(Default)]
struct ForkLog {
	/// Every relay block imported, with its number and the sender heads it included.
	imported: BTreeMap<H256, (u32, Vec<[u8; 32]>)>,
	finalized: std::collections::BTreeSet<H256>,
	finalized_number: u32,
	/// `newest_included_provides(SENDER)` at best blocks, each distinct root once, in order.
	roots: Vec<H256>,
	/// Best blocks whose newest root was one seen before the latest: the receiver's root went
	/// back.
	backs: u32,
}

/// Relay blocks below the finalized height a block counts as settled at. A fork often re-includes
/// an abandoned block's candidate a block or two later.
const SETTLED_DEPTH: u32 = 3;

impl ForkLog {
	/// Up to `SETTLED_DEPTH` below the last finalized block: the relay blocks seen, those
	/// abandoned, those of them that included a sender candidate, and the sender heads they
	/// included that the final chain never did, each a sender block its chain reorged away.
	fn counts(&self) -> (usize, usize, usize, usize) {
		let settled_at = self.finalized_number.saturating_sub(SETTLED_DEPTH);
		let settled: Vec<_> = self.imported.iter().filter(|(_, (n, _))| *n <= settled_at).collect();
		let (finalized, abandoned): (Vec<_>, Vec<_>) =
			settled.iter().partition(|(hash, _)| self.finalized.contains(hash));
		let canonical: std::collections::BTreeSet<_> =
			finalized.iter().flat_map(|(_, (_, heads))| heads.iter()).collect();
		let with_inclusion = abandoned.iter().filter(|(_, (_, heads))| !heads.is_empty()).count();
		let dropped: std::collections::BTreeSet<_> = abandoned
			.iter()
			.flat_map(|(_, (_, heads))| heads.iter())
			.filter(|head| !canonical.contains(head))
			.collect();
		(settled.len(), abandoned.len(), with_inclusion, dropped.len())
	}
}

/// Delivery through relay reorgs. Both relay validators claim most slots as primary, so the relay
/// chain forks at most heights and reorgs revert sender inclusions. The sender sends XCMs until
/// two rarer events have both happened: the receiver's newest sender root went back to an earlier
/// one, and the sender's chain reorged away a block a relay fork had included. Then the receiver
/// must consume and settle the whole stream: its frontier equals the sender's, so it consumed the
/// same leaves, in the same order.
#[tokio::test(flavor = "multi_thread")]
async fn spec_msg_survives_relay_forks() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);
	let network =
		initialize_network(build_network_config((SENDER_COLLATOR, SENDER_P2P_PORT), true)?).await?;
	let relay: OnlineClient<PolkadotConfig> = network.get_node("alice")?.wait_client().await?;
	let sender: OnlineClient<PolkadotConfig> =
		network.get_node(SENDER_COLLATOR)?.wait_client().await?;
	let receiver: OnlineClient<PolkadotConfig> =
		network.get_node(RECEIVER_COLLATOR)?.wait_client().await?;
	let start = (finalized_number(&sender).await?, finalized_number(&receiver).await?);
	wait_for("both chains to finalize a block", || {
		advanced(&sender, &receiver, (start.0 + 1, start.1 + 1))
	})
	.await?;
	open_spec_msg_channel(&sender, &receiver).await?;

	let log = std::sync::Arc::new(std::sync::Mutex::new(ForkLog::default()));
	let mut all = relay.blocks().subscribe_all().await?;
	let mut finalized = relay.blocks().subscribe_finalized().await?;
	let mut best = relay.blocks().subscribe_best().await?;
	let (l1, l2, l3, r3) = (log.clone(), log.clone(), log.clone(), relay.clone());
	tokio::spawn(async move {
		while let Some(Ok(block)) = all.next().await {
			let mut heads = Vec::new();
			if let Ok(events) = block.events().await {
				for event in events.iter().flatten() {
					if event.pallet_name() != "ParaInclusion" ||
						event.variant_name() != "CandidateIncluded"
					{
						continue;
					}
					let Ok(fields) = event.field_values() else { continue };
					let para = fields.at(0).and_then(|r| r.at("descriptor")).and_then(|d| {
						d.at("para_id")
							.and_then(|id| id.at(0).or(Some(id)))
							.and_then(|v| v.as_u128())
					});
					if para == Some(SENDER.into()) {
						if let Some(head) = fields.at(1) {
							heads.push(sp_crypto_hashing::blake2_256(&flatten(head)));
						}
					}
				}
			}
			l1.lock().unwrap().imported.insert(block.hash(), (block.number(), heads));
		}
	});
	tokio::spawn(async move {
		while let Some(Ok(block)) = finalized.next().await {
			let mut log = l2.lock().unwrap();
			log.finalized.insert(block.hash());
			log.finalized_number = log.finalized_number.max(block.number());
		}
	});
	tokio::spawn(async move {
		while let Some(Ok(block)) = best.next().await {
			let Ok(Some(root)) = call_at::<Option<H256>>(
				&r3,
				Some(block.hash()),
				"ParachainHost_newest_included_provides",
				&ParaId::from(SENDER).encode(),
			)
			.await
			else {
				continue;
			};
			let mut log = l3.lock().unwrap();
			if log.roots.last() == Some(&root) {
				continue;
			}
			if log.roots.contains(&root) {
				log.backs += 1;
				log::info!("Relay best #{}: the sender's newest root went back", block.number());
			} else {
				log.roots.push(root);
			}
		}
	});

	let report = |log: &ForkLog| {
		let (seen, abandoned, with_inclusion, dropped) = log.counts();
		format!(
			"{seen} relay blocks settled, {abandoned} abandoned, {with_inclusion} of those included \
			 a {SENDER} candidate, {dropped} {SENDER} heads reorged away; the newest root went back \
			 {} times",
			log.backs,
		)
	};
	let deadline = std::time::Instant::now() + Duration::from_secs(fork_secs());
	let mut batches = 0u32;
	loop {
		let covered = {
			let log = log.lock().unwrap();
			log.backs > 0 && log.counts().3 > 0
		};
		if covered {
			break;
		}
		if std::time::Instant::now() >= deadline {
			return Err(anyhow!(
				"no root went back and no sender block was reorged away within {} s: {}",
				fork_secs(),
				report(&log.lock().unwrap()),
			));
		}
		match force_batch(&sender, (0..3).map(|_| send_xcm_to(RECEIVER)).collect()).await {
			Ok(_) => batches += 1,
			Err(error) => log::warn!("A batch of sends failed: {error}"),
		}
		tokio::time::sleep(Duration::from_secs(6)).await;
	}
	log::info!("After {batches} batches of 3 XCMs: {}", report(&log.lock().unwrap()));

	let stream = out_channel().data_stream();
	let mut delivered = None;
	for _ in 0..60 {
		let frontiers: Vec<(StreamId, MmrFrontier)> =
			call_at(&sender, Some(best_hash(&sender).await?), "SpecMsgApi_outbound_frontiers", &[])
				.await?;
		let sent = frontiers.into_iter().find_map(|(s, f)| (s == stream).then_some(f));
		let consumed = channel_map::<InChannelId, InChannelState>(&receiver, "InChannels")
			.await?
			.remove(&in_channel())
			.map(|state| state.frontier);
		if sent.is_some() && sent == consumed && queued(&receiver).await? == 0 {
			delivered = sent;
			break;
		}
		tokio::time::sleep(Duration::from_secs(5)).await;
	}
	let Some(frontier) = delivered else {
		return Err(anyhow!("{RECEIVER} did not consume and settle {SENDER}'s stream"));
	};
	// A head counted as reorged away early may be included later: count again once every block
	// seen has settled.
	let last = log.lock().unwrap().imported.values().map(|(n, _)| *n).max().unwrap_or_default();
	wait_for("every relay block seen to settle", || async {
		Ok(finalized_number(&relay).await? >= last + SETTLED_DEPTH)
	})
	.await?;
	let log = log.lock().unwrap();
	if log.counts().3 == 0 {
		return Err(anyhow!("once settled, no sender block was reorged away: {}", report(&log)));
	}
	log::info!("Settled: {}", report(&log));
	log::info!(
		"{RECEIVER} consumed {SENDER}'s stream to leaf {}, the same frontier the sender holds, \
		 and settled every XCM",
		frontier.leaf_count(),
	);
	Ok(())
}

/// Send one warm-up message that must arrive under `origin`, then [`SAMPLES`] measured ones.
async fn measure(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
	inclusions: &Inclusions,
	origin: &str,
) -> Result<Vec<Sample>, anyhow::Error> {
	// The path can lag the channel state by a block or two: retry the warm-up until it arrives
	// under the expected origin.
	let mut warmed_up = false;
	for attempt in 1..=5 {
		let sample = deliver(sender, receiver, inclusions).await?;
		if sample.1 == origin {
			warmed_up = true;
			break;
		}
		log::info!("Warm-up {attempt} arrived under {}, expected {origin}", sample.1);
	}
	if !warmed_up {
		return Err(anyhow!("messages never arrived under {origin}"));
	}
	let mut samples = Vec::with_capacity(self::samples());
	for i in 1..=self::samples() {
		let (sample, arrived) = deliver(sender, receiver, inclusions).await?;
		if arrived != origin {
			return Err(anyhow!("sample {i} arrived under {arrived}, expected {origin}"));
		}
		log::info!(
			"{origin} sample {i}: {} relay blocks; sent #{} (relay parent #{}), included at relay #{}, \
			 processed #{} (relay parent #{})",
			sample.relay_blocks,
			sample.sent_block,
			sample.sent_relay,
			sample.included_relay,
			sample.processed_block,
			sample.processed_relay,
		);
		samples.push(sample);
	}
	Ok(samples)
}

/// Send one XCM and wait for it to be processed. Returns the sample and the origin it arrived
/// under.
async fn deliver(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
	inclusions: &Inclusions,
) -> Result<(Sample, String), anyhow::Error> {
	// Subscribe first: the message may be processed before the send is finalized.
	let mut arrivals = receiver.blocks().subscribe_finalized().await?;
	let sent_in = sender.blocks().at(sudo(sender, send_xcm_to(RECEIVER)).await?).await?;
	let id = event_field(&sent_in, "PolkadotXcm", "Sent", "message_id")
		.await?
		.map(|id| flatten(&id))
		.ok_or_else(|| anyhow!("no `PolkadotXcm::Sent` in the send block"))?;
	let sent_relay = relay_parent(&sent_in).await?;

	let wait = async {
		while let Some(block) = arrivals.next().await {
			let block = block?;
			for event in block.events().await?.iter() {
				let event = event?;
				if event.pallet_name() != "MessageQueue" || event.variant_name() != "Processed" {
					continue;
				}
				let fields = event.field_values()?;
				if fields.at("id").map(flatten) != Some(id.clone()) {
					continue;
				}
				if format!("{fields:?}").contains("Bool(false)") {
					return Err(anyhow!("message processed but failed: {fields:?}"));
				}
				let origin = match fields.at("origin").map(|origin| &origin.value) {
					Some(ValueDef::Variant(variant)) => variant.name.clone(),
					_ => return Err(anyhow!("no origin in {fields:?}")),
				};
				let processed_relay = relay_parent(&block).await?;
				let sample = Sample {
					relay_blocks: processed_relay.saturating_sub(sent_relay),
					sent_relay,
					included_relay: included_at(inclusions, sent_in.hash()).await?,
					processed_relay,
					sent_block: sent_in.number(),
					processed_block: block.number(),
				};
				return Ok((sample, origin));
			}
		}
		Err(anyhow!("receiver block subscription ended"))
	};
	tokio::time::timeout(Duration::from_secs(180), wait)
		.await
		.map_err(|_| anyhow!("message not processed within 3 minutes"))?
}

/// The first `pallet::variant` event's `field` in `block`.
async fn event_field(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	pallet: &str,
	variant: &str,
	field: &str,
) -> Result<Option<Value<u32>>, anyhow::Error> {
	for event in block.events().await?.iter() {
		let event = event?;
		if event.pallet_name() == pallet && event.variant_name() == variant {
			return Ok(event.field_values()?.at(field).cloned());
		}
	}
	Ok(None)
}

/// A block's relay parent number.
async fn relay_parent(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<u32, anyhow::Error> {
	let query = subxt::dynamic::storage(
		"ParachainSystem",
		"LastRelayChainBlockNumber",
		Vec::<Value>::new(),
	);
	let value = block
		.storage()
		.fetch(&query)
		.await?
		.ok_or_else(|| anyhow!("ParachainSystem::LastRelayChainBlockNumber is empty"))?;
	Ok(u32::decode(&mut &value.into_encoded()[..])?)
}

/// All integers in `value`, depth first, as bytes: a byte array or hash of any nesting.
fn flatten(value: &Value<u32>) -> Vec<u8> {
	match &value.value {
		ValueDef::Primitive(Primitive::U128(n)) => vec![*n as u8],
		ValueDef::Composite(composite) => composite.values().flat_map(flatten).collect(),
		_ => Vec::new(),
	}
}

/// Log min, median, p80 (nearest rank) and max of `samples`: the total by time and by relay
/// blocks, and the relay blocks split into sender inclusion (send relay parent to inclusion) and
/// receiver pickup (inclusion to the processing block's relay parent).
fn report(path: &str, samples: &[Sample]) {
	fn stats(name: &str, mut values: Vec<i64>) -> String {
		values.sort();
		let rank = |p: usize| values[(values.len() * p).div_ceil(100).max(1) - 1];
		format!(
			"{name} min {} p50 {} p80 {} max {} {values:?}",
			values[0],
			rank(50),
			rank(80),
			values[values.len() - 1]
		)
	}
	let of = |f: fn(&Sample) -> i64| samples.iter().map(f).collect::<Vec<_>>();
	log::info!("{path}: {}", stats("relay blocks", of(|s| s.relay_blocks.into())));
	log::info!(
		"{path}: {}",
		stats("inclusion", of(|s| s.included_relay as i64 - s.sent_relay as i64))
	);
	log::info!(
		"{path}: {}",
		stats("pickup", of(|s| s.processed_relay as i64 - s.included_relay as i64))
	);
}

/// Open the XCM channel `SENDER -> RECEIVER` and wait until it carries sends: the receiver has
/// consumed the open signal and the sender has read the receiver's register.
async fn open_spec_msg_channel(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	// A rerun against the same network finds the channel open: skip the open and send again.
	let (_, out_channels) = channel_states(sender, receiver).await?;
	if out_channels
		.get(&out_channel())
		.is_some_and(|state| state.open.is_some_and(|session| session.opened))
	{
		log::info!("Channel {SENDER} -> {RECEIVER} is already open: skipping the open");
	} else {
		submit_open(sender, receiver).await?;
		wait_opened(sender, receiver).await?;
	}

	let (in_channels, out_channels) = channel_states(sender, receiver).await?;
	if let (Some(in_state), Some(out_state)) =
		(in_channels.get(&in_channel()), out_channels.get(&out_channel()))
	{
		if let (Some(published), Some(read)) = (in_state.published, out_state.register) {
			log::info!(
				"Checkpoint 1/6, channel: {SENDER} -> {RECEIVER} is open ({RECEIVER} consumed \
				 {SENDER}'s stream up to leaf {}, {SENDER} read the `Ack` register at leaf {}: \
				 up to {}, session {})",
				published.up_to.0,
				read.position.0,
				read.register.up_to.0,
				out_state.session,
			);
		}
	}
	Ok(())
}

/// Submit the sender's `open_channel` and the receiver's `accept_open_channel` for the XCM
/// channel `SENDER -> RECEIVER`.
async fn submit_open(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	log::info!("Opening channel {SENDER} -> {RECEIVER}");
	// Either order works, so submit both at once; the wait below polls best blocks.
	let open = sudo_in_block(
		sender,
		Value::unnamed_variant(
			"SpecMessaging",
			[Value::named_variant(
				"open_channel",
				[
					("recipient", Value::u128(RECEIVER.into())),
					("domain", Value::u128(0)),
					("num", Value::u128(0)),
				],
			)],
		),
	);
	let accept = sudo_in_block(
		receiver,
		Value::unnamed_variant(
			"SpecMessaging",
			[Value::named_variant(
				"accept_open_channel",
				[
					("sender", Value::u128(SENDER.into())),
					("domain", Value::u128(0)),
					("num", Value::u128(0)),
				],
			)],
		),
	);
	tokio::try_join!(open, accept)?;

	Ok(())
}

/// Wait until the channel `SENDER -> RECEIVER` carries sends.
async fn wait_opened(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	// The handshake takes two register reads: the sender reads `Accepting` and sends `Hello`,
	// the receiver consumes it and publishes `Opened`, and the sender's read of that opens the
	// session. Best blocks, not finalized: finality adds about 30 s, and checkpoints 3 to 6
	// still prove inclusion, backing and finality of the send.
	wait_for("the sender to read the receiver's `Opened` register", || async {
		let (_, out_channels) = channel_states(sender, receiver).await?;
		Ok(out_channels
			.get(&out_channel())
			.is_some_and(|state| state.open.is_some_and(|session| session.opened)))
	})
	.await
}

/// The `VersionedLocation` of sibling `para`.
fn sibling(para: u32) -> Value {
	Value::unnamed_variant(
		"V5",
		[Value::named_composite([
			("parents", Value::u128(1)),
			(
				"interior",
				Value::unnamed_variant(
					"X1",
					[Value::unnamed_composite([Value::unnamed_variant(
						"Parachain",
						[Value::u128(para.into())],
					)])],
				),
			),
		])],
	)
}

/// `PolkadotXcm::send` of `[UnsubscribeVersion]` to sibling `para`. Penpal's barrier admits it
/// from any origin, and it executes without paying or sending anything back.
fn send_xcm_to(para: u32) -> Value {
	let dest = sibling(para);
	let message = Value::unnamed_variant(
		"V5",
		[Value::unnamed_composite([Value::unnamed_composite([Value::unnamed_variant(
			"UnsubscribeVersion",
			[],
		)])])],
	);
	Value::unnamed_variant(
		"PolkadotXcm",
		[Value::named_variant("send", [("dest", dest), ("message", message)])],
	)
}

/// Dispatch `call` as root with `Sudo::sudo`, signed by Alice, and check the inner result. Returns
/// the finalized block that holds it.
async fn sudo(client: &OnlineClient<PolkadotConfig>, call: Value) -> Result<H256, anyhow::Error> {
	Ok(submit_sudo(client, call, true).await?.0)
}

/// Dispatch `call` as root like [`sudo`], but return once the call is in a best block.
async fn sudo_in_block(
	client: &OnlineClient<PolkadotConfig>,
	call: Value,
) -> Result<H256, anyhow::Error> {
	Ok(submit_sudo(client, call, false).await?.0)
}

/// Dispatch `call` as root like [`sudo_in_block`]; the ids of the XCMs it sent. Only the call's
/// own events count: its block may hold an earlier call too.
async fn sudo_sent(
	client: &OnlineClient<PolkadotConfig>,
	call: Value,
) -> Result<Vec<Vec<u8>>, anyhow::Error> {
	Ok(submit_sudo(client, call, false).await?.1)
}

/// The block that holds the call, and the ids of the XCMs the call sent.
async fn submit_sudo(
	client: &OnlineClient<PolkadotConfig>,
	call: Value,
	finalized: bool,
) -> Result<(H256, Vec<Vec<u8>>), anyhow::Error> {
	let alice = dev::alice();
	// The nonce is read at the best block. subxt reads it at the finalized block, which an earlier
	// call that is only in a best block has not reached: the next call would reuse its nonce. The
	// best block can lag too, after a reorg, so it is at least one past the last one used here.
	static USED: std::sync::Mutex<BTreeMap<H256, u32>> = std::sync::Mutex::new(BTreeMap::new());
	let at_best: u32 = call_at(
		client,
		Some(best_hash(client).await?),
		"AccountNonceApi_account_nonce",
		&alice.public_key().0.encode(),
	)
	.await?;
	let nonce = {
		let mut used = USED.lock().expect("not poisoned; qed");
		let last = used.get(&client.genesis_hash());
		let nonce = last.map_or(at_best, |last| at_best.max(last + 1));
		used.insert(client.genesis_hash(), nonce);
		nonce
	};
	let params = PolkadotExtrinsicParamsBuilder::new().immortal().nonce(nonce.into()).build();
	let tx = subxt::dynamic::tx("Sudo", "sudo", vec![call]);
	let mut progress =
		client.tx().create_signed(&tx, &alice, params).await?.submit_and_watch().await?;
	while let Some(status) = progress.next().await {
		let in_block = match status? {
			TxStatus::InBestBlock(in_block) if !finalized => in_block,
			TxStatus::InFinalizedBlock(in_block) => in_block,
			TxStatus::Error { message } |
			TxStatus::Invalid { message } |
			TxStatus::Dropped { message } => return Err(anyhow!("sudo tx failed: {message}")),
			_ => continue,
		};
		let mut sent = Vec::new();
		for event in in_block.wait_for_success().await?.iter() {
			let event = event?;
			if event.pallet_name() == "PolkadotXcm" && event.variant_name() == "Sent" {
				sent.extend(event.field_values()?.at("message_id").map(flatten));
			}
			if event.pallet_name() == "Sudo" && event.variant_name() == "Sudid" {
				let fields = format!("{:?}", event.field_values()?);
				if fields.contains("Err") {
					return Err(anyhow!("sudo dispatch failed: {fields}"));
				}
				return Ok((in_block.block_hash(), sent));
			}
		}
		return Err(anyhow!("no `Sudo::Sudid` in block {:?}", in_block.block_hash()));
	}
	Err(anyhow!("sudo tx status stream ended"))
}

/// The receiver's inbound and the sender's outbound channel states at their best blocks.
async fn channel_states(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
) -> Result<
	(BTreeMap<InChannelId, InChannelState>, BTreeMap<OutChannelId, OutChannelState>),
	anyhow::Error,
> {
	let in_channels = channel_map(receiver, "InChannels").await?;
	let out_channels = channel_map(sender, "OutChannels").await?;
	Ok((in_channels, out_channels))
}

/// Every entry of a `SpecMessaging` channel map at the best block.
async fn channel_map<K: Decode + Ord, V: Decode>(
	client: &OnlineClient<PolkadotConfig>,
	map: &str,
) -> Result<BTreeMap<K, V>, anyhow::Error> {
	let query = subxt::dynamic::storage("SpecMessaging", map, Vec::<Value>::new());
	let storage = client.storage().at(best_hash(client).await?);
	let mut entries = storage.iter(query).await?;
	let mut out = BTreeMap::new();
	while let Some(entry) = entries.next().await {
		let entry = entry?;
		out.insert(map_key(&entry.key_bytes)?, V::decode(&mut &entry.value.into_encoded()[..])?);
	}
	Ok(out)
}

/// The key of a `Twox64Concat` map entry, which follows the 32-byte prefix and the 8-byte hash.
fn map_key<K: Decode>(key_bytes: &[u8]) -> Result<K, anyhow::Error> {
	let key = key_bytes.get(40..).ok_or_else(|| anyhow!("short storage key"))?;
	Ok(K::decode(&mut &key[..])?)
}

/// The hash of a chain's latest best block.
async fn best_hash(client: &OnlineClient<PolkadotConfig>) -> Result<H256, anyhow::Error> {
	let mut blocks = client.blocks().subscribe_best().await?;
	let block = blocks.next().await.ok_or_else(|| anyhow!("the best block stream ended"))??;
	Ok(block.hash())
}

/// The number of a chain's latest finalized block.
async fn finalized_number(client: &OnlineClient<PolkadotConfig>) -> Result<u32, anyhow::Error> {
	Ok(client.blocks().at_latest().await?.number())
}

/// Whether both chains have finalized at least the given block numbers.
async fn advanced(
	sender: &OnlineClient<PolkadotConfig>,
	receiver: &OnlineClient<PolkadotConfig>,
	(sender_at, receiver_at): (u32, u32),
) -> Result<bool, anyhow::Error> {
	Ok(finalized_number(sender).await? >= sender_at &&
		finalized_number(receiver).await? >= receiver_at)
}

/// Call a runtime API with SCALE-encoded `args` at block `at`, or the latest if `None`, and decode
/// its result.
async fn call_at<T: Decode>(
	client: &OnlineClient<PolkadotConfig>,
	at: Option<H256>,
	method: &str,
	args: &[u8],
) -> Result<T, anyhow::Error> {
	let api = match at {
		Some(hash) => client.runtime_api().at(hash),
		None => client.runtime_api().at_latest().await?,
	};
	let bytes = api.call_raw(method, Some(args)).await?;
	Ok(T::decode(&mut &bytes[..])?)
}

/// Whether the relay chain has an HRMP channel `from -> to`.
async fn hrmp_channel_open(
	relay: &OnlineClient<PolkadotConfig>,
	from: u32,
	to: u32,
) -> Result<bool, anyhow::Error> {
	let key = Value::named_composite([
		("sender", Value::u128(from.into())),
		("recipient", Value::u128(to.into())),
	]);
	let query = subxt::dynamic::storage("Hrmp", "HrmpChannels", vec![key]);
	Ok(relay.storage().at_latest().await?.fetch(&query).await?.is_some())
}

/// The `StreamsRoot` a block commits in its SPMS digest, if it sent anything.
fn committed_root(block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>) -> Option<H256> {
	block.header().digest.logs.iter().find_map(|item| match item {
		DigestItem::Consensus(id, payload) if *id == SPMS_ENGINE_ID => {
			H256::decode(&mut &payload[..]).ok()
		},
		_ => None,
	})
}

/// The number of the block on `client`'s best chain, at or after `from`, that committed `root`.
async fn committed_since(
	client: &OnlineClient<PolkadotConfig>,
	root: H256,
	from: u32,
) -> Result<Option<u32>, anyhow::Error> {
	let mut block = client.blocks().at(best_hash(client).await?).await?;
	loop {
		if block.number() < from {
			return Ok(None);
		}
		if committed_root(&block) == Some(root) {
			return Ok(Some(block.number()));
		}
		block = client.blocks().at(block.header().parent_hash).await?;
	}
}

/// The stream roots a block's `StreamsRoot` commits: each stream's stored frontier, which is the
/// parent's state, plus the leaves the block sent.
async fn committed_stream_roots(
	block: &Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
) -> Result<BTreeMap<StreamId, MmrFrontier>, anyhow::Error> {
	let storage = block.storage();
	let mut frontiers = BTreeMap::new();
	let query = subxt::dynamic::storage("SpecMessaging", "OutboundFrontier", Vec::<Value>::new());
	let mut entries = storage.iter(query).await?;
	while let Some(entry) = entries.next().await {
		let entry = entry?;
		let frontier = MmrFrontier::decode(&mut &entry.value.into_encoded()[..])?;
		frontiers.insert(map_key(&entry.key_bytes)?, frontier);
	}
	let query = subxt::dynamic::storage("SpecMessaging", "OutboundLeafHashes", Vec::<Value>::new());
	let mut entries = storage.iter(query).await?;
	while let Some(entry) = entries.next().await {
		let entry = entry?;
		let frontier = frontiers.entry(map_key::<StreamId>(&entry.key_bytes)?).or_default();
		for leaf in Vec::<[u8; 32]>::decode(&mut &entry.value.into_encoded()[..])? {
			frontier.append(leaf.into());
		}
	}
	Ok(frontiers)
}

/// Poll `check` until it holds, or fail.
async fn wait_for<F, Fut>(what: &str, check: F) -> Result<(), anyhow::Error>
where
	F: Fn() -> Fut,
	Fut: Future<Output = Result<bool, anyhow::Error>>,
{
	log::info!("Waiting for {what}");
	for _ in 0..50 {
		if check().await? {
			log::info!("Done: {what}");
			return Ok(());
		}
		tokio::time::sleep(Duration::from_secs(6)).await;
	}
	Err(anyhow!("timed out waiting for {what}"))
}

/// Follow the receiver's finalized blocks after `before_send` until the XCM executes, logging the
/// block that consumed the sender's stream and the one that executed the XCM. The consumption must
/// reach the stream entry that `root`, the send block's `StreamsRoot`, commits (`committed`), and
/// the executed XCM must carry the sent message `id`.
async fn follow_delivery(
	receiver: &OnlineClient<PolkadotConfig>,
	mut blocks: StreamOfResults<Block<PolkadotConfig, OnlineClient<PolkadotConfig>>>,
	before_send: u32,
	id: H256,
	root: H256,
	committed: &BTreeMap<StreamId, MmrFrontier>,
) -> Result<(), anyhow::Error> {
	log::info!("Following {RECEIVER}'s finalized blocks");
	let mut consumed = false;
	let follow = async {
		while let Some(block) = blocks.next().await {
			let block = block?;
			if block.number() <= before_send {
				continue;
			}
			if !consumed {
				let record: ConsumptionRecord =
					call_at(receiver, Some(block.hash()), "SpecMsgApi_consumption_record", &[])
						.await?;
				if let Some(streams) = record.entries.get(&ParaId::from(SENDER)) {
					for (stream, interval) in streams {
						// The sender may send again before the receiver consumes: the block then
						// reaches past the sent leaf, under a newer root.
						let reaches = committed.get(stream).is_some_and(|sent| {
							interval.end.leaf_count() > sent.leaf_count() ||
								interval.end.root() == sent.root()
						});
						if !reaches {
							return Err(anyhow!(
								"{RECEIVER} block #{} consumed {stream:?} up to leaf {}, short of \
								 the entry {root:?} commits",
								block.number(),
								interval.end.leaf_count(),
							));
						}
						log::info!(
							"Checkpoint 4/6, consume: {RECEIVER} block #{} consumed {stream:?} up \
							 to leaf {}, fetched from {SENDER}'s collator. It reaches the stream \
							 root `StreamsRoot` {root:?} commits; its consumption record becomes \
							 the candidate's `Requires`",
							block.number(),
							interval.end.leaf_count(),
						);
					}
					log::info!(
						"Checkpoint 5/6, backing: {RECEIVER} block #{} is finalized, so its \
						 candidate was backed and included: the relay matched its `Requires` \
						 against {SENDER}'s provides window",
						block.number(),
					);
					consumed = true;
				}
			}
			for event in block.events().await?.iter() {
				let event = event?;
				if event.pallet_name() != "MessageQueue" || event.variant_name() != "Processed" {
					continue;
				}
				let values = event.field_values()?;
				if values.at("id").map(flatten) != Some(id.0.to_vec()) {
					continue;
				}
				let fields = format!("{values:?}");
				if !fields.contains("SpecMsg") || !fields.contains("Bool(true)") {
					return Err(anyhow!("XCM {id:?} not executed over spec-msg: {fields}"));
				}
				if !consumed {
					return Err(anyhow!("XCM executed before any block consumed it"));
				}
				let field = |name| values.at(name).map(ToString::to_string).unwrap_or_default();
				log::info!(
					"Checkpoint 6/6, execute: {RECEIVER} block #{} `MessageQueue::Processed`: id \
					 {id:?}, the sent message id; origin {}, success {}",
					block.number(),
					field("origin"),
					field("success"),
				);
				return Ok(());
			}
		}
		Err(anyhow!("receiver block subscription ended"))
	};
	tokio::time::timeout(Duration::from_secs(300), follow)
		.await
		.map_err(|_| anyhow!("XCM not executed within 5 minutes"))?
}

/// `--spec-msg-peer` for the collator `name` of `para`, listening on `port`.
fn peer_arg(para: u32, name: &str, port: u16) -> Result<String, anyhow::Error> {
	let (_, peer_id) = zombienet_orchestrator::generators::generate_node_identity(name)
		.map_err(|e| anyhow!("peer id of {name}: {e}"))?;
	Ok(format!("{para}=/ip4/127.0.0.1/tcp/{port}/ws/p2p/{peer_id}"))
}

/// The network; the receiver fetches the sender's streams from `sender_node`, a node name and its
/// p2p port.
/// With `relay_forks`, both relay validators claim most slots as primary (BABE `c` = 99/100, which
/// must stay below 1), so the relay chain forks at most heights.
fn build_network_config(
	sender_node: (&str, u16),
	relay_forks: bool,
) -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let to_receiver = peer_arg(RECEIVER, RECEIVER_COLLATOR, RECEIVER_P2P_PORT)?;
	let to_sender = peer_arg(SENDER, sender_node.0, sender_node.1)?;
	let collator_args = |peer: &str| -> Vec<zombienet_sdk::Arg> {
		vec![
			("-lparachain=debug,spec-msg=debug,xcm=info,cumulus-collator=debug,\
			  runtime::spec-msg=debug,runtime::parachain-system=debug")
				.into(),
			("--force-authoring").into(),
			("--authoring", "slot-based").into(),
			("--enable-spec-msg").into(),
			("--spec-msg-peer", peer).into(),
		]
	};

	// Distinct `protocolId`s: chains sharing one negotiate the legacy block-announce name, fail its
	// genesis check and ban each other, refusing every spec-msg fetch for about a minute.
	// Network setup:
	// - relay chain: alice and bob, validators
	// - penpal 2000: charlie, collator, fetching from dave
	// - penpal 2001: dave, collator, fetching from charlie
	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r.with_chain("rococo-local");
			let r = if relay_forks {
				r.with_genesis_overrides(serde_json::json!({
					"babe": { "epochConfig": {
						"c": [99, 100],
						"allowed_slots": "PrimaryAndSecondaryVRFSlots",
					} },
				}))
			} else {
				r
			};
			r.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![
					("-lparachain=debug,runtime::inclusion=debug,runtime::inclusion-inherent=debug")
						.into(),
				])
				.with_validator(|node| node.with_name("alice").with_rpc_port(RELAY_RPC_PORT))
				.with_validator(|node| node.with_name("bob"))
		})
		.with_parachain(|p| {
			p.with_id(SENDER)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain(format!("penpal-rococo-{SENDER}").as_str())
				.with_raw_spec_override(
					serde_json::json!({ "protocolId": format!("penpal-{SENDER}") }),
				)
				.with_default_args(collator_args(&to_receiver))
				.with_collator(|n| {
					n.with_name(SENDER_COLLATOR)
						.with_p2p_port(SENDER_P2P_PORT)
						.with_rpc_port(SENDER_RPC_PORT)
				})
		})
		.with_parachain(|p| {
			p.with_id(RECEIVER)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain(format!("penpal-rococo-{RECEIVER}").as_str())
				.with_raw_spec_override(
					serde_json::json!({ "protocolId": format!("penpal-{RECEIVER}") }),
				)
				.with_default_args(collator_args(&to_sender))
				.with_collator(|n| {
					n.with_name(RECEIVER_COLLATOR)
						.with_p2p_port(RECEIVER_P2P_PORT)
						.with_rpc_port(RECEIVER_RPC_PORT)
				})
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})
}
