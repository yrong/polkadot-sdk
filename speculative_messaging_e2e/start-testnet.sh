#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../" && pwd)"
TOML="${ROOT_DIR}/speculative_messaging_e2e/network.toml"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${PROFILE:-release}"

export POLKADOT_BINARY="${POLKADOT_BINARY:-"${ROOT_DIR}/target/${PROFILE}/polkadot"}"
export POLKADOT_PARACHAIN_BINARY="${POLKADOT_PARACHAIN_BINARY:-"${ROOT_DIR}/target/${PROFILE}/polkadot-parachain"}"

RELAY_WS="${RELAY_WS:-ws://127.0.0.1:9900}"
SENDER_WS="${SENDER_WS:-ws://127.0.0.1:9955}"
RECEIVER_WS="${RECEIVER_WS:-ws://127.0.0.1:9966}"

ZOMBIENET_LOG="${ZOMBIENET_LOG:-/tmp/speculative-messaging-poc.log}"
ZOMBIENET_DIR="${ZOMBIENET_DIR:-/tmp/speculative-messaging-poc}"

echo "[1/4] Stopping any previous run (best-effort)"
pkill -f "speculative_messaging_e2e" 2>/dev/null || true
rm -rf "${ZOMBIENET_DIR}"

echo "[2/4] Spawning zombienet: ${TOML}"
echo "  logs: ${ZOMBIENET_LOG}"
zombienet --provider native spawn --dir="${ZOMBIENET_DIR}" "${TOML}" >"${ZOMBIENET_LOG}" 2>&1 &
ZOMBIE_PID="$!"
echo "  zombienet pid: ${ZOMBIE_PID}"

echo "  installing JS deps..."
if [[ ! -d "${SCRIPT_DIR}/node_modules" ]]; then
  (cd "${SCRIPT_DIR}" && npm install --silent)
fi

echo "  waiting for relay (${RELAY_WS}) to accept connections..."
node - <<NODE
const { URL } = require('url');
const u = new URL(process.env.RELAY_WS || 'ws://127.0.0.1:9900');
const net = require('net');
(async () => {
  for (let i = 0; i < 120; i++) {
    const ok = await new Promise(res => {
      const s = net.createConnection({ host: u.hostname, port: Number(u.port || 80) });
      s.on('connect', () => { s.destroy(); res(true); });
      s.on('error', () => res(false));
      s.setTimeout(1000, () => { s.destroy(); res(false); });
    });
    if (ok) process.exit(0);
    await new Promise(r => setTimeout(r, 1000));
  }
  console.error('timeout waiting for relay RPC');
  process.exit(1);
})();
NODE

echo "  waiting for relay height > 2..."
(cd "${SCRIPT_DIR}" && node - <<NODE
const { ApiPromise, WsProvider } = require('@polkadot/api');
(async () => {
  const api = await ApiPromise.create({ provider: new WsProvider('${RELAY_WS}') });
  for (let i = 0; i < 120; i++) {
    const n = (await api.rpc.chain.getHeader()).number.toNumber();
    if (n > 2) { console.log('  relay best=' + n); await api.disconnect(); process.exit(0); }
    await new Promise(r => setTimeout(r, 1000));
  }
  console.error('timeout waiting for relay to advance'); process.exit(1);
})().catch(e => { console.error(e.message); process.exit(1); });
NODE
)

echo "[3/4] Waiting for sender parachain (${SENDER_WS})..."
(cd "${SCRIPT_DIR}" && node - <<NODE
const { ApiPromise, WsProvider } = require('@polkadot/api');
(async () => {
  for (let i = 0; i < 60; i++) {
    try {
      const api = await ApiPromise.create({ provider: new WsProvider('${SENDER_WS}'), throwOnConnect: true });
      const n = (await api.rpc.chain.getHeader()).number.toNumber();
      console.log('  sender best=' + n);
      await api.disconnect();
      process.exit(0);
    } catch (_) { await new Promise(r => setTimeout(r, 2000)); }
  }
  console.error('timeout waiting for sender'); process.exit(1);
})();
NODE
)

echo "[4/4] Opening HRMP channels 2000 ↔ 2001..."
(cd "${SCRIPT_DIR}" && RELAY_WS="${RELAY_WS}" node open-hrmp.js)

echo
echo "Network is up."
echo "  relay:    ${RELAY_WS}"
echo "  sender:   ${SENDER_WS}  (para 2000)"
echo "  receiver: ${RECEIVER_WS}  (para 2001)"
echo
echo "Next steps:"
echo "  Send XCM:  node ${SCRIPT_DIR}/send-xcm.js"
echo "  Observe:   node ${SCRIPT_DIR}/observe.js"
echo "  Full e2e:  ${SCRIPT_DIR}/run-e2e.sh"
