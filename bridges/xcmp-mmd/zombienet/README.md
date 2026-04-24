# XCMP MMD POC - Zombienet Test Setup

## Network topology

```
westend-local relay (4 validators, BEEFY enabled)
 ├── para 1000  source  (xcmp-mmd-outbox pallet)
 └── para 2000  dest    (xcmp-mmd-inbox pallet)
```

## Prerequisites

1. **Build the binaries** (from repo root):
   ```bash
   cargo build -p polkadot --release
   cargo build -p polkadot-parachain-bin --release
   ```
   The parachain binary must include the `xcmp-mmd-outbox` and `xcmp-mmd-inbox`
   pallets integrated into its runtime. Until these pallets are merged, build from
   this branch.

2. **Install zombienet** (v1.3+):
   ```bash
   npm install -g @zombienet/cli
   # or download from https://github.com/paritytech/zombienet/releases
   ```

3. **Build the relayer**:
   ```bash
   cd bridges/xcmp-mmd/relayer && SKIP_WASM_BUILD=1 cargo build --release
   ```

## Running the test network

```bash
# From repo root
zombienet --provider native spawn bridges/xcmp-mmd/zombienet/xcmp-mmd-poc.toml
```

Zombienet will print WebSocket endpoints for each node. The defaults match the
relayer config:
- Relay chain:  `ws://127.0.0.1:9901`
- Source para:  `ws://127.0.0.1:9945`
- Dest para:    `ws://127.0.0.1:9956`

## Running the end-to-end test

Once the network is running:

```bash
./bridges/xcmp-mmd/zombienet/e2e-test.sh
```

The script will:
1. Wait for both parachains to finalize blocks
2. Prompt you to submit a test XCM send extrinsic on the source para
3. Scan source headers for the `xmmd` PreRuntime digest
4. Start the relayer against the live network
5. Poll the destination for events

## Manual verification steps

### Check xmmd digest in source header

```bash
# Get finalized head of source para
curl -s -X POST http://127.0.0.1:9945 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getFinalizedHead","params":[]}' \
  | jq .result

# Get header and look for PreRuntime("xmmd", ...)
curl -s -X POST http://127.0.0.1:9945 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getHeader","params":["<HASH>"]}' \
  | jq '.result.digest.logs[]'
```

### Check destination events

Use Polkadot.js Apps (`https://polkadot.js.org/apps`) connected to `ws://127.0.0.1:9956`
and navigate to **Network → Explorer** to see events. Look for:

- `xcmpMmdInbox.MessageReceived` — successful delivery
- `xcmpMmdInbox.MessageAlreadySeen` — replay protection triggered

### Run relayer manually

```bash
cd bridges/xcmp-mmd/relayer
XCMP_MMD_PALLET_INDEX=<N> \
XCMP_MMD_CALL_INDEX=<M> \
./target/release/xcmp-mmd-relayer \
  --config relayer.toml \
  --log-level debug
```

Set `XCMP_MMD_PALLET_INDEX` and `XCMP_MMD_CALL_INDEX` to match the position of
`pallet-xcmp-mmd-inbox` and its `submit_xcmp_mmd` call in the destination runtime.

## What a passing test looks like

```
[10:01:23] Relay finalized head: 0xabc...
[10:01:25] Source para head: 0xdef...
[10:01:27] Dest para head: 0x123...
[10:01:45] Found xmmd digest in block 0xdef...
[10:01:45] Starting relayer...
[10:01:46] Relayer PID: 12345
[10:01:52] Building proof for message: para=1000 leaf_index=0
[10:01:52] Outbox proof: leaf_index=0, mmr_size=1, 0 proof items
[10:01:52] Found relay block #42 (0x789...)
[10:01:53] Relay MMR proof: leaf_index=41, leaf_count=42, 5 proof items
[10:01:53] Para-heads proof: 2 paras, source at index 0, 1 proof items
[10:01:53] Relayed message leaf_index=0 → tx 0xfed...
```

## Known limitations for the POC

- Para-heads proof verification in the inbox pallet is simplified (Step 3)
- Extrinsic signing in the relayer uses placeholder bytes (not production-ready)
- The relayer scans only the last 100 relay blocks when looking up source heads
- `find_relay_block_for_source` matches head bytes exactly — requires the relayer
  to be running when the source block is produced (no archive lookback)
