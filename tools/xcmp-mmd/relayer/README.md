# XCMP MMD Relayer

Off-chain relayer for the XCMP MMD (Merkle Mountain Range based cross-chain messaging) POC.

## What it does

Watches a source parachain for outbound XCMP MMD messages, constructs a three-tier proof bundle,
and submits `MessageWithProof` to the destination parachain's `submit_xcmp_mmd` extrinsic.

### Proof construction flow

```
1. Monitor source chain for PreRuntime(*b"xmmd", ...) digest → outbox MMR root
2. Fetch payload from HrmpOutboundMessages at that block
3. Generate outbox MMR proof  → source runtime API XcmpMmdOutboxApi::generate_outbox_proof
4. Find relay block that included the source block head
5. Generate relay MMR proof   → relay chain mmr_generateProof RPC
6. Extract ParaHeadsRoot from BEEFY MMR leaf (last 32 bytes = leaf_extra)
7. Reconstruct para-heads Merkle proof from relay state (sorted by para_id, KeccakHasher)
8. Submit MessageWithProof to dest chain submit_xcmp_mmd extrinsic
```

## Building

```bash
cd tools/xcmp-mmd/relayer
# SKIP_WASM_BUILD is required because path deps reach into the polkadot-sdk
# workspace which contains parachain runtimes with WASM build scripts.
SKIP_WASM_BUILD=1 cargo build --release
```

## Configuration

Copy and edit `relayer.toml`:

```toml
source_ws    = "ws://127.0.0.1:9944"
dest_ws      = "ws://127.0.0.1:9955"
relay_ws     = "ws://127.0.0.1:9900"
source_para_id = 1000
dest_para_id   = 2000
signer_seed  = "//Alice"
lookback_blocks = 0
```

## Running

```bash
# With default relayer.toml in current directory
./target/release/xcmp-mmd-relayer

# Custom config path and verbose logging
./target/release/xcmp-mmd-relayer --config /path/to/relayer.toml --log-level debug
```

## Environment overrides

| Variable                 | Default | Description                                 |
|--------------------------|---------|---------------------------------------------|
| `XCMP_MMD_PALLET_INDEX`  | `0x80`  | Pallet index of `pallet-xcmp-mmd-inbox`     |
| `XCMP_MMD_CALL_INDEX`    | `0x00`  | Call index of `submit_xcmp_mmd` extrinsic   |

Set these to match your destination runtime's actual pallet position.

## Architecture

```
main.rs      - CLI (clap), loads Config, creates Relayer, calls run()
config.rs    - Config struct, TOML deserialization
types.rs     - Shared types: PendingMessage, MessageWithProof, OutboxLeaf, etc.
client.rs    - JSON-RPC wrappers
  SubstrateClient  - raw HTTP JSON-RPC (rpc_call, storage, call_runtime_api, ...)
  SourceClient     - source parachain: parse xmmd digest, hrmp_outbound_messages,
                     generate_outbox_proof (runtime API)
  RelayClient      - relay chain: sorted_para_heads, mmr_generateProof,
                     find_relay_block_for_source
  DestClient       - destination parachain: submit_extrinsic
relayer.rs   - Event loop: poll finalized head, discover messages, relay each one
proof.rs     - Proof construction: build_message_with_proof orchestrates all three tiers
```

## Limitations (POC)

- Uses HTTP JSON-RPC polling instead of WS subscriptions (simpler but higher latency)
- Extrinsic signing is a stub; production use needs proper SR25519 signed extrinsics via subxt
- Para-heads proof verification in the inbox pallet is simplified (returns placeholder header)
- `find_relay_block_for_source` scans only the last 100 relay blocks

## Dependencies

| Crate              | Purpose                                       |
|--------------------|-----------------------------------------------|
| `tokio`            | Async runtime                                 |
| `reqwest`          | HTTP JSON-RPC calls                           |
| `codec`            | SCALE encode/decode                           |
| `sp-core`          | H256, hashing (twox_128, twox_64, keccak_256) |
| `binary-merkle-tree` | Para-heads Merkle proof construction        |
| `mmr-lib`          | (available for outbox proof construction)     |
| `clap`             | CLI argument parsing                          |
| `anyhow`           | Error handling                                |
| `tracing`          | Structured logging                            |
| `serde_json`       | JSON-RPC response parsing                     |
| `hex`              | Hex encode/decode for RPC values              |
