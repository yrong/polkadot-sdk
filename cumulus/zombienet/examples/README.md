# Zombienet Examples

## Prerequisites

Install the zombienet CLI:

```bash
cargo install zombie-cli
```

## Usage

```bash
./run.sh <network-file.toml>
```

The script will:
1. Build `polkadot`, `polkadot-prepare-worker`, `polkadot-execute-worker`, and `polkadot-parachain` in release mode
2. Add the release directory to `PATH`
3. Spawn the network using `zombie-cli`

## Speculative Messaging POC (`speculative_messaging_poc/network.toml`)

End-to-end test for the speculative messaging POC. Spawns:
- Rococo-local relay chain (Alice + Bob validators)
- Para 2000 — sender Penpal collator (`polkadot-parachain`, slot-based)
- Para 2001 — receiver Penpal collator (`polkadot-parachain`, slot-based, connected to sender via `--speculative-sender`)

### Build

```bash
cargo build -p polkadot --release
cargo build -p polkadot-parachain --release
```

### Run

```bash
zombienet --provider native spawn cumulus/zombienet/examples/speculative_messaging_poc/network.toml
```

### Trigger XCM from sender to receiver

Once the network is up, send an XCM message from para 2000 to para 2001 using
Polkadot.js Apps (connect to the sender's WS endpoint, e.g. `ws://127.0.0.1:9955`):

```
Extrinsics → polkadotXcm → send(dest, message)
  dest: V4 { parents: 1, interior: X1(Parachain(2001)) }
  message: [WithdrawAsset(...), BuyExecution(...), DepositAsset(...)]
```

Or use `polkadot-js-api` / a script to call `polkadotXcm.send` as `sudo`.

### What to observe

1. **Sender** (para 2000): after producing a block with outbound XCM, logs should show
   `compute_provides_root` returning a non-None commitment. The candidate receipt should
   have a v4 descriptor (once `SpeculativeMessaging` feature is enabled on the relay chain).

2. **Receiver** (para 2001): after each slot, logs should show:
   - `"Connected to speculative messaging sender para_id=2000"` at startup
   - `fetch_ingress_for_block` fetching batches from the sender
   - `ingest_verified_messages` executing with the received batch
   - `requires_commitments` returning a non-empty list

3. **Relay chain**: once the receiver's candidate is enacted, `ProvidesRoots[2001]`
   should be updated. The receiver's next candidate should match against it.

### Troubleshooting

- If the receiver log shows `"Failed to connect to speculative messaging sender"`, check
  that the sender's RPC port (9955) is reachable and that the sender started successfully.
- If no batches are fetched, confirm the sender produced a block with outbound XCM
  (check `SpeculativeOutbox::compute_provides_root` via RPC).
- v4 descriptors are only produced when `SpeculativeMessaging` feature bit is set in
  relay chain config and the collator produces non-empty `provides`/`requires`.
