#!/usr/bin/env node
/**
 * Observe speculative messaging activity across all three chains.
 *
 * Watches:
 *   - Sender  (para 2000): SpeculativeOutbox.MessagesRecorded events,
 *                          compute_provides_root runtime API
 *   - Relay chain:         ProvidesRoots[2000] storage (updates when sender
 *                          candidate is enacted)
 *   - Receiver (para 2001): SpeculativeInbox events, requires_commitments
 *                           runtime API after each block
 *
 * Env:
 *   RELAY_WS     (default: ws://127.0.0.1:9900)
 *   SENDER_WS    (default: ws://127.0.0.1:9955)
 *   RECEIVER_WS  (default: ws://127.0.0.1:9966)
 *   SENDER_PARA  (default: 2000)
 *
 * Ctrl-C to stop.
 */

const { ApiPromise, WsProvider } = require('@polkadot/api');
const { u8aToHex } = require('@polkadot/util');
const { xxhashAsHex } = require('@polkadot/util-crypto');

const RELAY_WS    = process.env.RELAY_WS    || 'ws://127.0.0.1:9900';
const SENDER_WS   = process.env.SENDER_WS   || 'ws://127.0.0.1:9955';
const RECEIVER_WS = process.env.RECEIVER_WS || 'ws://127.0.0.1:9966';
const SENDER_PARA = Number(process.env.SENDER_PARA || 2000);

function ts() { return new Date().toISOString().slice(11, 23); }
function log(chain, msg) { console.log(`[${ts()}] [${chain.padEnd(8)}] ${msg}`); }

// Build the storage key for ProvidesRoots[para_id] in the inclusion pallet.
// Layout: Twox128("ParachainsInclusion") + Twox128("ProvidesRoots") + Twox64Concat(ParaId)
function providesRootsKey(api, paraId) {
  const palletHash = xxhashAsHex('ParachainsInclusion', 128).slice(2);
  const storageHash = xxhashAsHex('ProvidesRoots', 128).slice(2);
  // Twox64Concat: Twox64(encoded key) ++ encoded key
  const encoded = api.registry.createType('ParaId', paraId).toU8a();
  const { xxhashAsU8a } = require('@polkadot/util-crypto');
  const prefix = xxhashAsU8a(encoded, 64);
  const keyHex = u8aToHex(new Uint8Array([...prefix, ...encoded]));
  return '0x' + palletHash + storageHash + keyHex.slice(2);
}

async function watchSender(api) {
  log('sender', `Connected — watching para ${SENDER_PARA}`);

  // Subscribe to new blocks and log relevant events + runtime API state.
  await api.rpc.chain.subscribeNewHeads(async (header) => {
    const n = header.number.toNumber();
    const blockHash = header.hash;

    // Check events for this block.
    const apiAt = await api.at(blockHash);
    const events = await apiAt.query.system.events();
    for (const { event } of events) {
      if (event.section.toLowerCase().includes('speculative')) {
        log('sender', `  block #${n} event: ${event.section}.${event.method} ${event.data.toString()}`);
      }
    }

    // Call compute_provides_root runtime API.
    try {
      const result = await api.rpc.state.call(
        'SpeculativeOutboxApi_compute_provides_root',
        '0x',
        blockHash,
      );
      if (result && result.length > 2) {
        log('sender', `  block #${n} provides_root=${result.toString()}`);
      } else {
        log('sender', `  block #${n} provides_root=None`);
      }
    } catch (e) {
      log('sender', `  block #${n} provides_root error: ${e.message}`);
    }
  });
}

async function watchRelay(api) {
  log('relay', 'Connected — watching ProvidesRoots storage');

  const storageKey = providesRootsKey(api, SENDER_PARA);
  log('relay', `  storage key: ${storageKey}`);

  let lastRoot = null;
  await api.rpc.state.subscribeStorage([storageKey], (changes) => {
    for (const [, value] of changes) {
      const rootHex = value?.isSome ? value.unwrap().toHex() : null;
      if (rootHex !== lastRoot) {
        lastRoot = rootHex;
        log('relay', `  ProvidesRoots[${SENDER_PARA}] updated → ${rootHex || 'None'}`);
      }
    }
  });
}

async function watchReceiver(api) {
  log('receiver', 'Connected — watching para 2001');

  await api.rpc.chain.subscribeNewHeads(async (header) => {
    const n = header.number.toNumber();
    const blockHash = header.hash;

    // Check events.
    const apiAt = await api.at(blockHash);
    const events = await apiAt.query.system.events();
    for (const { event } of events) {
      if (event.section.toLowerCase().includes('speculative')) {
        log('receiver', `  block #${n} event: ${event.section}.${event.method} ${event.data.toString()}`);
      }
    }

    // Call requires_commitments runtime API.
    try {
      const result = await api.rpc.state.call(
        'SpeculativeInboxApi_requires_commitments',
        '0x',
        blockHash,
      );
      if (result && result.length > 2) {
        log('receiver', `  block #${n} requires_commitments=${result.toString()}`);
      }
    } catch (_) {
      // API not available on this runtime — silently skip.
    }
  });
}

async function main() {
  console.log('Connecting to all three chains...\n');

  const [senderApi, relayApi, receiverApi] = await Promise.all([
    ApiPromise.create({ provider: new WsProvider(SENDER_WS) }),
    ApiPromise.create({ provider: new WsProvider(RELAY_WS) }),
    ApiPromise.create({ provider: new WsProvider(RECEIVER_WS) }),
  ]);

  log('sender',   `chain: ${(await senderApi.rpc.system.chain()).toString()}`);
  log('relay',    `chain: ${(await relayApi.rpc.system.chain()).toString()}`);
  log('receiver', `chain: ${(await receiverApi.rpc.system.chain()).toString()}`);
  console.log();

  // Start all three watchers concurrently. None return normally.
  await Promise.all([
    watchSender(senderApi),
    watchRelay(relayApi),
    watchReceiver(receiverApi),
  ]);
}

main().catch(e => { console.error('Error:', e.message || e); process.exit(1); });
