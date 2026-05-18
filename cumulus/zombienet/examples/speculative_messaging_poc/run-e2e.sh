#!/usr/bin/env bash
# Full speculative messaging e2e flow:
#   1. Build polkadot + polkadot-omni-node
#   2. Spawn zombienet (two Penpal parachains, Rococo-local relay)
#   3. Send XCM from para 2000 → para 2001 via speculative messaging
#   4. Confirm system.Remarked on receiver (delivery confirmed)
#
# Prerequisites: zombienet and node on PATH.
#
# Env (optional):
#   PROFILE=release|dev
#   SKIP_BUILD=1          — skip binary build if already done
#   RELAY_WS, SENDER_WS, RECEIVER_WS — override defaults
#   XCM_REMARK            — custom remark text
#   REMARK_TIMEOUT_MS     — how long to wait for delivery (default: 120000)
#
set -euo pipefail

SECONDS=0
E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PROFILE="${PROFILE:-release}"
export RELAY_WS="${RELAY_WS:-ws://127.0.0.1:9900}"
export SENDER_WS="${SENDER_WS:-ws://127.0.0.1:9955}"
export RECEIVER_WS="${RECEIVER_WS:-ws://127.0.0.1:9966}"

echo "=== [1/3] Build binaries (PROFILE=${PROFILE}) ==="
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  "${E2E_DIR}/build-binaries.sh"
else
  echo "  SKIP_BUILD=1 — skipping build"
fi

echo
echo "=== [2/3] Start testnet ==="
"${E2E_DIR}/start-testnet.sh"

echo
echo "=== [3/3] Send XCM and wait for delivery ==="
if [[ ! -d "${E2E_DIR}/node_modules" ]]; then
  (cd "${E2E_DIR}" && npm install --silent)
fi
(cd "${E2E_DIR}" && node send-xcm.js)

echo
echo "=== E2E complete in ${SECONDS}s ==="
