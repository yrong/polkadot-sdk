#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../../" && pwd)"
PROFILE="${PROFILE:-release}"

echo "[1/2] Building relay + omni-node binaries (profile=${PROFILE})"
cd "${ROOT_DIR}"
cargo build --profile "${PROFILE}" -p polkadot -p polkadot-parachain-bin

echo
echo "[2/2] Outputs"
echo "  polkadot:           ${ROOT_DIR}/target/${PROFILE}/polkadot"
echo "  polkadot-parachain: ${ROOT_DIR}/target/${PROFILE}/polkadot-parachain"
echo
echo "Tip: PROFILE=release ${BASH_SOURCE[0]}"
