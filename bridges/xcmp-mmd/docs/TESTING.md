# XCMP MMD POC - Testing Guide

## Prerequisites

All binaries must be built in release mode:

```bash
# From polkadot-sdk repo root
cargo build -p polkadot --release
cargo build -p polkadot-parachain-bin --release
cd bridges/xcmp-mmd/relayer && SKIP_WASM_BUILD=1 cargo build --release
```

Verify binaries exist:
- `target/release/polkadot`
- `target/release/polkadot-parachain`
- `bridges/xcmp-mmd/relayer/target/release/xcmp-mmd-relayer`

## Running the Test Network

### 1. Start Zombienet

```bash
# From repo root
zombienet --provider native spawn bridges/xcmp-mmd/testing/zombienet/xcmp-mmd-poc.toml
```

This spawns:
- **Relay chain**: westend-local with 4 validators (BEEFY enabled, MMR active)
  - Alice: ws://127.0.0.1:9901 (RPC: 9900)
  - Bob, Charlie, Dave
- **Para 1000** (source): xcmp-mmd-outbox pallet
  - Alice collator: ws://127.0.0.1:9945
- **Para 2000** (dest): xcmp-mmd-inbox pallet
  - Alice collator: ws://127.0.0.1:9956

**Important flags on relay chain and source para collator:**
- `--enable-offchain-indexing=true` - Required for `mmr_generateProof` RPC
- `--pruning archive` - Keeps MMR leaf data in offchain DB

### 2. Run End-to-End Test

In a separate terminal:

```bash
cd bridges/xcmp-mmd/testing/zombienet
./e2e-test.sh
```

The script will:
1. Wait for relay chain finality
2. Wait for both parachains to finalize blocks
3. Prompt you to submit a test XCM send extrinsic
4. Scan source headers for `xmmd` digest
5. Start the relayer
6. Poll destination for `MessageReceived` event

### 3. Manual XCM Send (when prompted)

Use Polkadot.js Apps UI connected to `ws://127.0.0.1:9945` (source para):

**Option A: Using xcm.send**
```
Developer → Extrinsics → xcm.send(
  dest: { V4: { parents: 1, interior: { X1: [{ Parachain: 2000 }] } } },
  message: {
    V4: [
      { UnpaidExecution: { weightLimit: Unlimited } },
      { ClearOrigin: null },
    ]
  }
)
```

**Option B: Using xcmpQueue.sendXcm** (if available)
```
Developer → Extrinsics → xcmpQueue.sendXcm(
  dest: 2000,
  message: <encoded XCM>
)
```

Sign with Alice and submit.

### 4. Verify Success

**Check source para header for xmmd digest:**
```bash
curl -s -X POST http://127.0.0.1:9945 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getFinalizedHead","params":[]}' \
  | jq -r '.result' | read HASH

curl -s -X POST http://127.0.0.1:9945 \
  -H "Content-Type: application/json" \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"chain_getHeader\",\"params\":[\"$HASH\"]}" \
  | jq '.result.digest.logs[]' | grep -i xmmd
```

**Check destination events:**

Use Polkadot.js Apps connected to `ws://127.0.0.1:9956`:
- Navigate to **Network → Explorer**
- Look for events:
  - `xcmpMmdInbox.MessageReceived` ✅ Success
  - `xcmpMmdInbox.MessageAlreadySeen` (if replayed)

**Check relayer logs:**

The relayer should output:
```
[INFO] Building proof for message: para=1000 leaf_index=0
[INFO] Outbox proof: leaf_index=0, mmr_size=1, 0 proof items
[INFO] Found relay block #42 (0x789...)
[INFO] Relay MMR proof: leaf_index=41, leaf_count=42, 5 proof items
[INFO] Para-heads proof: 2 paras, source at index 0, 1 proof items
[INFO] Relayed message leaf_index=0 → tx 0xfed...
```

## Manual Relayer Run

If you want to run the relayer separately:

```bash
cd bridges/xcmp-mmd/relayer

# Optional: override pallet/call indices if needed
export XCMP_MMD_PALLET_INDEX=71  # XcmpMmdInbox position in penpal
export XCMP_MMD_CALL_INDEX=0     # submit_xcmp_mmd call index

./target/release/xcmp-mmd-relayer \
  --config relayer.toml \
  --log-level debug
```

## Troubleshooting

### No xmmd digest in source headers
- Check that `xcmp-mmd-outbox` pallet is integrated in source para runtime
- Verify `OutboundXcmpMessageSource = XcmpMmdOutbox` in `ParachainSystem` config
- Check source para logs for HRMP message queue activity

### mmr_generateProof returns empty proof
- Ensure relay chain has `--enable-offchain-indexing=true`
- Ensure relay chain has `--pruning archive`
- Wait a few blocks after the source block is included

### Relayer can't find relay block
- The relayer scans last 100 relay blocks for the source head
- If source block is too old, the relayer won't find it
- Start relayer soon after XCM send

### Extrinsic submission fails
- Check `XCMP_MMD_PALLET_INDEX` and `XCMP_MMD_CALL_INDEX` match destination runtime
- Verify Alice has sufficient balance on destination para
- Check destination para logs for dispatch errors

### MessageAlreadySeen event
- The inbox pallet tracks seen messages by `(source_para_id, mmr_leaf_index)`
- This is replay protection working correctly
- Send a new message to test again

## Expected Timeline

From XCM send to message delivery:
1. XCM send extrinsic: ~6s (1 source para block)
2. Source para block included in relay: ~6-12s (1-2 relay blocks)
3. Relay block finalized: ~12-18s (BEEFY finality)
4. Relayer detects and builds proof: ~1-2s
5. Relayer submits to destination: ~1s
6. Destination processes: ~6s (1 dest para block)

**Total: ~30-45 seconds** from send to delivery
