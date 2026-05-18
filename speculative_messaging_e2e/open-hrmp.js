#!/usr/bin/env node
/**
 * Open bidirectional HRMP channels between para 2000 and para 2001 via sudo.
 *
 * Required so polkadotXcm.send routes messages through XcmpQueue — speculative
 * messaging intercepts those messages for off-chain delivery, but the channel
 * must exist for the sender runtime to accept the XCM send call.
 *
 * Env:
 *   RELAY_WS            (default: ws://127.0.0.1:9900)
 *   SENDER_PARA         (default: 2000)
 *   RECEIVER_PARA       (default: 2001)
 *   MAX_CAPACITY        (default: 8)
 *   MAX_MESSAGE_SIZE    (default: 524288)
 *   SUDO_SEED           (default: //Alice)
 *   HRMP_VERIFY_MAX_BLOCKS    (default: 60)
 *   HRMP_VERIFY_POLL_MS       (default: 3000)
 *   HRMP_VERIFY_ABS_TIMEOUT_MS (default: 600000)
 */

const { ApiPromise, WsProvider, Keyring } = require("@polkadot/api");
const { cryptoWaitReady } = require("@polkadot/util-crypto");

function envInt(name, def) {
  const v = process.env[name];
  if (!v) return def;
  const n = Number(v);
  if (!Number.isFinite(n)) throw new Error(`Invalid ${name}=${v}`);
  return n;
}

async function signAndWait(api, signer, tx) {
  return new Promise((resolve, reject) => {
    let unsub;
    tx.signAndSend(signer, ({ status, dispatchError }) => {
      if (dispatchError) {
        if (dispatchError.isModule) {
          const decoded = api.registry.findMetaError(dispatchError.asModule);
          reject(
            new Error(
              `${decoded.section}.${decoded.name}: ${decoded.docs.join(" ")}`,
            ),
          );
        } else {
          reject(new Error(dispatchError.toString()));
        }
        unsub?.();
        return;
      }
      if (status.isInBlock) {
        console.log(`  ✓ included in ${status.asInBlock.toHex()}`);
        unsub?.();
        resolve();
      }
    })
      .then((u) => (unsub = u))
      .catch(reject);
  });
}

async function main() {
  await cryptoWaitReady();

  const relayWs = process.env.RELAY_WS || "ws://127.0.0.1:9901";
  const paraA = envInt("SENDER_PARA", 2000);
  const paraB = envInt("RECEIVER_PARA", 2001);
  const maxCap = envInt("MAX_CAPACITY", 8);
  const maxSize = envInt("MAX_MESSAGE_SIZE", 524288);
  const sudoSeed = process.env.SUDO_SEED || "//Alice";

  const api = await ApiPromise.create({ provider: new WsProvider(relayWs) });
  console.log(
    `Connected: ${(await api.rpc.system.chain()).toString()} @ ${relayWs}`,
  );

  const keyring = new Keyring({ type: "sr25519" });
  const sudo = keyring.addFromUri(sudoSeed);
  console.log(`Sudo: ${sudo.address}`);

  for (const [sender, recipient] of [
    [paraA, paraB],
    [paraB, paraA],
  ]) {
    console.log(`\nOpening ${sender} → ${recipient}…`);
    await signAndWait(
      api,
      sudo,
      api.tx.sudo.sudo(
        api.tx.hrmp.forceOpenHrmpChannel(sender, recipient, maxCap, maxSize),
      ),
    );
  }

  // Wait for channels to appear in storage.
  const maxBlocks = envInt("HRMP_VERIFY_MAX_BLOCKS", 60);
  const pollMs = envInt("HRMP_VERIFY_POLL_MS", 3000);
  const absTimeout = envInt("HRMP_VERIFY_ABS_TIMEOUT_MS", 600000);

  const startBlock = (await api.rpc.chain.getHeader()).number.toNumber();
  const deadline = Date.now() + absTimeout;
  const present = (ch) =>
    typeof ch?.isSome === "boolean" ? ch.isSome : ch?.toString() !== "0x";
  const checkOne = async (s, r) =>
    present(await api.query.hrmp.hrmpChannels([s, r])) ||
    present(await api.query.hrmp.hrmpChannels({ sender: s, recipient: r }));

  console.log(
    `\nWaiting for HRMP channels in storage (up to +${maxBlocks} blocks)…`,
  );
  while (true) {
    const okAB = await checkOne(paraA, paraB);
    const okBA = await checkOne(paraB, paraA);
    if (okAB && okBA) break;
    console.log(`  ${paraA}→${paraB}: ${okAB}  ${paraB}→${paraA}: ${okBA}`);

    const n = (await api.rpc.chain.getHeader()).number.toNumber();
    if (n - startBlock >= maxBlocks)
      throw new Error(
        `HRMP channels not present after ${maxBlocks} relay blocks`,
      );
    if (Date.now() > deadline)
      throw new Error(`HRMP channels not present after ${absTimeout}ms`);
    await new Promise((r) => setTimeout(r, pollMs));
  }

  console.log(`\n✓ HRMP channels open: ${paraA}↔${paraB}`);
  await api.disconnect();
}

main().catch((e) => {
  console.error("Error:", e.message || e);
  process.exit(1);
});
