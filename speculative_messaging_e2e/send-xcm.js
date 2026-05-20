#!/usr/bin/env node
/**
 * Send an XCM from para 2000 (sender) to para 2001 (receiver) using
 * polkadotXcm.send + Transact(system.remarkWithEvent), then wait for
 * system.Remarked on the receiver to confirm delivery via speculative messaging.
 *
 * Env:
 *   SENDER_WS    — sender RPC  (default: ws://127.0.0.1:9955)
 *   RECEIVER_WS  — receiver RPC (default: ws://127.0.0.1:9966)
 *   SENDER_PARA  — sender para ID (default: 2000)
 *   DEST_PARA    — destination para ID (default: 2001)
 *   XCM_REMARK   — remark text (default: timestamp)
 *   XCM_EXEC_FEE — fee amount on dest (default: 1e13)
 *   SUDO_SEED    — sudo keypair URI (default: //Alice)
 *   REMARK_TIMEOUT_MS — how long to wait for dest remarked (default: 120000)
 */

const { ApiPromise, WsProvider, Keyring } = require('@polkadot/api');
const { u8aEq, u8aToHex, u8aConcat, stringToU8a, u8aFixLength } = require('@polkadot/util');
const { blake2AsU8a, xxhashAsU8a, cryptoWaitReady } = require('@polkadot/util-crypto');

const SENDER_WS       = process.env.SENDER_WS    || 'ws://127.0.0.1:9955';
const RECEIVER_WS     = process.env.RECEIVER_WS  || 'ws://127.0.0.1:9966';
const SENDER_PARA     = Number(process.env.SENDER_PARA || 2000);
const DEST_PARA       = Number(process.env.DEST_PARA   || 2001);
const SUDO_SEED       = process.env.SUDO_SEED    || '//Alice';
const REMARK          = process.env.XCM_REMARK   || `speculative-msg e2e ${new Date().toISOString()}`;
const EXEC_FEE        = BigInt(process.env.XCM_EXEC_FEE || '10000000000000');
const REMARK_TIMEOUT  = Number(process.env.REMARK_TIMEOUT_MS || 120_000);

function remarkHash(remark) {
  return blake2AsU8a(new TextEncoder().encode(remark), 256);
}

// Sovereign account of a sibling para on Penpal (uses "sibl" prefix, not "para").
function siblingAccount(api, paraId) {
  const seed = u8aConcat(stringToU8a('sibl'), api.registry.createType('ParaId', paraId).toU8a());
  return api.registry.createType('AccountId', u8aFixLength(seed, 256, true));
}

async function maybePrefund(destApi, signer, paraId) {
  const acc = siblingAccount(destApi, paraId);
  const { data } = await destApi.query.system.account(acc);
  const free = BigInt(data.free.toString());
  const min = EXEC_FEE * 2n;
  console.log(`  sovereign ${acc.toString()} free=${free} (need >=${min})`);
  if (free >= min) { console.log('  OK — no prefund needed'); return; }
  const topUp = EXEC_FEE * 10n;
  console.log(`  prefunding ${topUp}...`);
  await new Promise((resolve, reject) => {
    let unsub;
    destApi.tx.balances.transferKeepAlive(acc, topUp).signAndSend(signer, ({ status }) => {
      if (status.isInBlock) { unsub?.(); resolve(); }
      if (status.isError)   { unsub?.(); reject(new Error('prefund failed')); }
    }).then(u => unsub = u).catch(reject);
  });
  console.log('  prefund included');
}

// Compute the System.Events storage key (no map key — it's a plain StorageValue).
// Key = xxhash128("System") ++ xxhash128("Events")
function eventsStorageKey() {
  const prefix = u8aConcat(xxhashAsU8a('System', 128), xxhashAsU8a('Events', 128));
  return u8aToHex(prefix);
}

async function waitForRemarked(api, expected, timeoutMs) {
  const storageKey = eventsStorageKey();

  return new Promise((resolve, reject) => {
    let done = false;
    let unsub;
    const timer = setTimeout(() => {
      done = true; unsub?.();
      reject(new Error(`No system.Remarked within ${timeoutMs / 1000}s`));
    }, timeoutMs);

    const check = async (blockHash) => {
      if (done) return;
      try {
        // Query events storage directly — bypasses extrinsic decoding entirely.
        const raw = await api.rpc.state.getStorage(storageKey, blockHash);
        if (!raw || raw.isEmpty) return;
        const events = api.registry.createType('Vec<FrameSystemEventRecord>', raw.toU8a(true));
        for (const record of events) {
          const { event } = record;
          if (event.section === 'system' && event.method === 'Remarked') {
            if (u8aEq(event.data[1].toU8a(), expected)) {
              done = true; clearTimeout(timer); unsub?.();
              resolve({ blockHash: blockHash.toHex() });
              return;
            }
          }
        }
      } catch (e) {
        console.warn(`  [waitForRemarked] error checking block ${blockHash.toHex()}: ${e.message}`);
      }
    };

    api.rpc.chain.subscribeAllHeads(h => check(h.hash))
      .then(u => { unsub = u; if (done) unsub(); })
      .catch(e => { if (!done) reject(e); });
  });
}

async function main() {
  await cryptoWaitReady();

  console.log(`Connecting sender ${SENDER_WS} and receiver ${RECEIVER_WS}...`);
  const [senderApi, receiverApi] = await Promise.all([
    ApiPromise.create({ provider: new WsProvider(SENDER_WS) }),
    ApiPromise.create({ provider: new WsProvider(RECEIVER_WS) }),
  ]);
  console.log(`  sender:   ${(await senderApi.rpc.system.chain()).toString()}`);
  console.log(`  receiver: ${(await receiverApi.rpc.system.chain()).toString()}`);

  const keyring = new Keyring({ type: 'sr25519' });
  const alice = keyring.addFromUri(SUDO_SEED);
  console.log(`  signer: ${alice.address}`);

  // Ensure the sender's sovereign on the receiver has funds for BuyExecution.
  console.log('\nChecking sovereign balance on receiver:');
  await maybePrefund(receiverApi, alice, SENDER_PARA);

  // Build the XCM: WithdrawAsset + BuyExecution + Transact(remarkWithEvent).
  const callBytes = receiverApi.tx.system.remarkWithEvent(REMARK).method.toU8a();
  const weight = { refTime: 10_000_000_000n, proofSize: 2_000_000n };
  const asset = { id: { parents: 0, interior: { Here: null } }, fun: { Fungible: EXEC_FEE } };
  const xcm = senderApi.createType('XcmVersionedXcm', { V4: [
    { WithdrawAsset: [asset] },
    { BuyExecution: { fees: asset, weightLimit: { Limited: weight } } },
    { Transact: {
        originKind: 'SovereignAccount',
        requireWeightAtMost: weight,
        call: senderApi.createType('XcmDoubleEncoded', { encoded: u8aToHex(callBytes) }),
    }},
  ]});
  const dest = senderApi.createType('XcmVersionedLocation', {
    V4: { parents: 1, interior: { X1: [{ Parachain: DEST_PARA }] } },
  });

  // Wrap in sudo so the XCM origin is the parachain sovereign (not Alice's sub-account).
  const sendTx = senderApi.tx.sudo.sudo(senderApi.tx.polkadotXcm.send(dest, xcm));

  const expected = remarkHash(REMARK);
  console.log(`\nSending XCM to para ${DEST_PARA}`);
  console.log(`  remark: ${REMARK}`);
  console.log(`  expected hash: ${u8aToHex(expected)}`);

  const tSend = Date.now();

  // Start watching the receiver immediately so we don't miss speculative delivery
  // that may arrive before or concurrently with the sender's isInBlock event.
  console.log(`\nWaiting for system.Remarked on receiver (${REMARK_TIMEOUT / 1000}s timeout)...`);
  const remarkedPromise = waitForRemarked(receiverApi, expected, REMARK_TIMEOUT);

  await new Promise((resolve, reject) => {
    let unsub;
    sendTx.signAndSend(alice, ({ status, events }) => {
      if (events?.length) {
        events.forEach(({ event }) => console.log(`  ${event.section}.${event.method}`));
      }
      if (status.isInBlock) {
        console.log(`\n✓ XCM included in sender block ${status.asInBlock.toHex()}`);
        unsub?.(); resolve();
      } else if (status.isError) {
        unsub?.(); reject(new Error('XCM send failed'));
      }
    }).then(u => unsub = u).catch(reject);
  });
  const tSenderIncluded = Date.now();

  const result = await remarkedPromise;
  const tDelivered = Date.now();

  const sendToInclude  = ((tSenderIncluded - tSend)      / 1000).toFixed(2);
  const includeToDeliv = ((tDelivered - tSenderIncluded) / 1000).toFixed(2);
  const totalLatency   = ((tDelivered - tSend)           / 1000).toFixed(2);

  console.log(`\n✅ Delivered via speculative messaging! system.Remarked in block ${result.blockHash}`);
  console.log(`\nLatency breakdown:`);
  console.log(`  sign → sender included          : ${sendToInclude}s`);
  console.log(`  sender included → delivered     : ${includeToDeliv}s  (negative = speculative pre-delivery)`);
  console.log(`  total (sign → delivered)        : ${totalLatency}s`);

  await Promise.all([senderApi.disconnect(), receiverApi.disconnect()]);
}

main().catch(e => { console.error('Error:', e.message || e); process.exit(1); });
