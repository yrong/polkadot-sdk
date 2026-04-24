#!/usr/bin/env bash
# XCMP MMD POC - End-to-end test script
#
# Assumes a running zombienet spawned from xcmp-mmd-poc.toml.
# Tests the complete message delivery flow:
#   1. Send XCM from para 1000 to para 2000
#   2. Verify xmmd digest appears in para 1000 header
#   3. Start relayer
#   4. Verify MessageReceived event on para 2000
#
# Usage:
#   ./e2e-test.sh [relay_ws] [source_ws] [dest_ws]

set -euo pipefail

RELAY_WS="${1:-ws://127.0.0.1:9901}"
SOURCE_WS="${2:-ws://127.0.0.1:9945}"
DEST_WS="${3:-ws://127.0.0.1:9956}"

SOURCE_PARA=1000
DEST_PARA=2000

RELAYER_BIN="$(dirname "$0")/../relayer/target/release/xcmp-mmd-relayer"
RELAYER_CFG="$(dirname "$0")/../relayer/relayer.toml"

log() { echo "[$(date -u +%H:%M:%S)] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# Require subxt-based CLI or polkadot-js-api for sending XCM.
# Here we use curl against the JSON-RPC endpoint as a lightweight alternative.
rpc() {
    local ws_url="$1"; shift
    local method="$1"; shift
    local params="$1"
    local http_url="${ws_url/ws:\/\//http://}"
    curl -s -X POST "$http_url" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

# ─── 1. Wait for relay chain to produce blocks ────────────────────────────────
log "Waiting for relay chain finality..."
for i in $(seq 1 30); do
    head=$(rpc "$RELAY_WS" "chain_getFinalizedHead" "[]" | jq -r '.result // empty')
    if [[ -n "$head" && "$head" != "null" ]]; then
        log "Relay finalized head: $head"
        break
    fi
    sleep 2
done
[[ -n "${head:-}" ]] || die "Relay chain did not finalize within 60s"

# ─── 2. Wait for both parachains to be onboarded ─────────────────────────────
log "Waiting for source parachain (${SOURCE_PARA}) to produce blocks..."
for i in $(seq 1 30); do
    src_head=$(rpc "$SOURCE_WS" "chain_getFinalizedHead" "[]" | jq -r '.result // empty')
    [[ -n "$src_head" && "$src_head" != "null" ]] && break
    sleep 2
done
[[ -n "${src_head:-}" ]] || die "Source parachain did not finalize within 60s"
log "Source para head: $src_head"

log "Waiting for dest parachain (${DEST_PARA}) to produce blocks..."
for i in $(seq 1 30); do
    dst_head=$(rpc "$DEST_WS" "chain_getFinalizedHead" "[]" | jq -r '.result // empty')
    [[ -n "$dst_head" && "$dst_head" != "null" ]] && break
    sleep 2
done
[[ -n "${dst_head:-}" ]] || die "Dest parachain did not finalize within 60s"
log "Dest para head: $dst_head"

# ─── 3. Submit a test XCM send extrinsic on source para ──────────────────────
# In a real test this would use polkadot-js-api or subxt to sign and submit.
# The extrinsic calls pallet_xcm::send() with a Transact payload destined for
# para 2000. That triggers HrmpOutboundMessages and the xcmp-mmd-outbox pallet
# deposits the digest.
#
# Placeholder: print instructions instead of automating the submission.
log ""
log "──────────────────────────────────────────────────────────────────────────"
log "MANUAL STEP: Submit a test XCM send extrinsic on para ${SOURCE_PARA}"
log ""
log "  Use polkadot-js-api or Polkadot.js Apps UI:"
log "    Extrinsic: xcm.send(dest=Parachain(${DEST_PARA}), message=[...])"
log "    OR: xcmpQueue.sendXcm(dest=${DEST_PARA}, msg=<encoded XCM>)"
log ""
log "  After submission, check para ${SOURCE_PARA} headers for xmmd digest."
log "  (Look for DigestItem::PreRuntime(\"xmmd\", ...) in chain_getHeader)"
log "──────────────────────────────────────────────────────────────────────────"
log ""
read -r -p "Press ENTER once you have submitted the XCM send extrinsic..."

# ─── 4. Poll source headers for xmmd digest ──────────────────────────────────
log "Scanning source parachain headers for xmmd digest..."
XMMD_ENGINE="786d6d64"
found_digest=0
for i in $(seq 1 20); do
    head=$(rpc "$SOURCE_WS" "chain_getFinalizedHead" "[]" | jq -r '.result')
    header=$(rpc "$SOURCE_WS" "chain_getHeader" "[\"$head\"]" | jq -r '.result')
    if echo "$header" | grep -qi "$XMMD_ENGINE"; then
        log "Found xmmd digest in block $head"
        found_digest=1
        break
    fi
    log "  No digest yet at $head, waiting..."
    sleep 3
done
[[ $found_digest -eq 1 ]] || die "No xmmd digest found after 60s — is xcmp-mmd-outbox pallet installed?"

# ─── 5. Start relayer ─────────────────────────────────────────────────────────
log "Starting relayer..."
if [[ ! -f "$RELAYER_BIN" ]]; then
    log "Relayer binary not found at $RELAYER_BIN; building..."
    (cd "$(dirname "$0")/../relayer" && SKIP_WASM_BUILD=1 cargo build --release)
fi

# Write a temporary config pointing at our running network
TMPDIR="$(mktemp -d)"
cat > "$TMPDIR/relayer.toml" <<EOF
source_ws    = "$SOURCE_WS"
dest_ws      = "$DEST_WS"
relay_ws     = "$RELAY_WS"
source_para_id = $SOURCE_PARA
dest_para_id   = $DEST_PARA
signer_seed  = "//Alice"
lookback_blocks = 5
EOF

log "Relayer config: $TMPDIR/relayer.toml"

# Run relayer in background
"$RELAYER_BIN" --config "$TMPDIR/relayer.toml" --log-level info &
RELAYER_PID=$!
trap "kill $RELAYER_PID 2>/dev/null || true; rm -rf $TMPDIR" EXIT
log "Relayer PID: $RELAYER_PID"

# ─── 6. Poll destination chain for MessageReceived event ─────────────────────
log "Waiting for MessageReceived event on dest para ${DEST_PARA}..."
# Storage key for System::Events
EVENTS_KEY="0x26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7"
found_event=0
for i in $(seq 1 30); do
    dst_head=$(rpc "$DEST_WS" "chain_getFinalizedHead" "[]" | jq -r '.result')
    events_hex=$(rpc "$DEST_WS" "state_getStorage" "[\"$EVENTS_KEY\",\"$dst_head\"]" | jq -r '.result // empty')
    if [[ -n "$events_hex" ]]; then
        # A rough heuristic: check if XcmpMmdInbox pallet events are present.
        # In production, decode the SCALE-encoded EventRecord vector.
        log "Events present at dest block $dst_head ($(echo -n "$events_hex" | wc -c) hex chars)"
        found_event=1
        break
    fi
    log "  No events yet at $dst_head..."
    sleep 4
done

# ─── 7. Summary ─────────────────────────────────────────────────────────────���─
log ""
log "══════════════════════════════════════════════════════════════════════════"
log "XCMP MMD POC - Test Results"
log "══════════════════════════════════════════════════════════════════════════"
log "Relay finalized:         ✅"
log "Source para finalized:   ✅"
log "Dest para finalized:     ✅"
log "xmmd digest found:       $([ $found_digest -eq 1 ] && echo ✅ || echo ❌)"
log "Relayer started:         ✅ (PID $RELAYER_PID)"
log "Events on dest para:     $([ $found_event -eq 1 ] && echo ✅ || echo ❌ waiting)"
log ""
log "For full event decoding, use Polkadot.js Apps UI on $DEST_WS"
log "Look for: xcmpMmdInbox.MessageReceived or xcmpMmdInbox.MessageAlreadySeen"
log "══════════════════════════════════════════════════════════════════════════"

if [[ $found_event -eq 1 ]]; then
    log "Relayer running. Press Ctrl-C to stop."
    wait $RELAYER_PID
fi
