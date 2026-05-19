#!/usr/bin/env bash
# Stop any running speculative messaging testnet and clean up temp files.
set -euo pipefail

ZOMBIENET_DIR="${ZOMBIENET_DIR:-/tmp/speculative-messaging-poc}"
ZOMBIENET_LOG="${ZOMBIENET_LOG:-/tmp/speculative-messaging-poc.log}"

echo "=== Stopping speculative messaging testnet ==="

# Kill zombienet and the node processes it spawned.
# zombienet itself: matched by the network.toml path or the spawn command.
if pkill -f "speculative_messaging_e2e/network.toml" 2>/dev/null; then
  echo "  stopped zombienet"
fi

# Kill any residual polkadot / polkadot-parachain processes from this testnet.
# Match on the data dir zombienet created so we don't kill unrelated nodes.
if pkill -f "${ZOMBIENET_DIR}" 2>/dev/null; then
  echo "  stopped child node processes"
fi

# Brief wait to let processes exit cleanly before removing their files.
sleep 1

echo "=== Removing temp files ==="
if [[ -d "${ZOMBIENET_DIR}" ]]; then
  rm -rf "${ZOMBIENET_DIR}"
  echo "  removed ${ZOMBIENET_DIR}"
fi
if [[ -f "${ZOMBIENET_LOG}" ]]; then
  rm -f "${ZOMBIENET_LOG}"
  echo "  removed ${ZOMBIENET_LOG}"
fi

echo "=== Cleanup complete ==="
