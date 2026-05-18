//! Sanity-check the speculative outbox JSON-RPC calls against a live sender node.
//!
//! Run while the zombienet testnet is up:
//!   cargo run --manifest-path speculative_messaging_e2e/rpc_check/Cargo.toml
//!
//! The test verifies that:
//! 1. chain_getBlockHash() returns a non-zero best block hash
//! 2. state_call for compute_provides_root decodes as Option<ProvidesCommitment>
//!    (previously broke due to SCALE Option discriminant being eaten into the hash)
//! 3. state_call for destination_state decodes as Option<(Hash, u64)>
//! 4. state_call for outbound_messages decodes as Vec<(u64, Vec<u8>)>
//! 5. state_call for subtree_inclusion_proof decodes as Option<(Vec<Hash>, u32, u32)>
//! 6. state_call for block_hash_for_provides_root returns a real block hash
//!    that the node recognises (i.e. NOT 0x0100..00 which was the pre-fix symptom)

use jsonrpsee::{core::client::ClientT, rpc_params, ws_client::WsClientBuilder};
use parity_scale_codec::Decode;

// Sender node WS-RPC endpoint (pinned in network.toml)
const SENDER_URL: &str = "ws://127.0.0.1:9955";
// Para 2001 is the receiver — used as destination for outbox queries
const DEST_PARA_ID: u32 = 2001;

type Hash = [u8; 32];

// Mirrors polkadot_primitives::v10::ProvidesCommitment
#[derive(Decode, Debug)]
struct ProvidesCommitment {
    root: Hash,
}

fn fmt_hash(h: &Hash) -> String {
    format!("0x{}", hex::encode(h))
}

/// Call a runtime API via state_call and SCALE-decode the result as `R`.
async fn state_call<R: Decode>(
    client: &impl ClientT,
    method: &str,
    at: &Hash,
    args: Vec<u8>,
) -> Result<R, String> {
    let at_hex = format!("0x{}", hex::encode(at));
    let args_hex = format!("0x{}", hex::encode(&args));

    let result: serde_json::Value = client
        .request("state_call", rpc_params![method, args_hex, at_hex])
        .await
        .map_err(|e| format!("RPC error: {e}"))?;

    let bytes_hex = result.as_str().ok_or_else(|| "result is not a string".to_string())?;
    let bytes = hex::decode(bytes_hex.trim_start_matches("0x"))
        .map_err(|e| format!("hex decode error: {e}"))?;

    R::decode(&mut &bytes[..]).map_err(|e| format!("SCALE decode error: {e}"))
}

/// Encode a u32 para id as SCALE (little-endian u32).
fn encode_para_id(id: u32) -> Vec<u8> {
    id.to_le_bytes().to_vec()
}

#[tokio::main]
async fn main() {
    println!("Speculative RPC sanity check");
    println!("Connecting to sender at {SENDER_URL}...\n");

    let client = WsClientBuilder::default()
        .build(SENDER_URL)
        .await
        .expect("Failed to connect to sender node — is the testnet running?");

    let mut passed = 0u32;
    let mut failed = 0u32;

    macro_rules! pass {
        ($label:expr, $msg:expr) => {{
            println!("  PASS  {}: {}", $label, $msg);
            passed += 1;
        }};
    }
    macro_rules! fail {
        ($label:expr, $msg:expr) => {{
            println!("  FAIL  {}: {}", $label, $msg);
            failed += 1;
        }};
    }

    // ── 1. Best block hash ───────────────────────────────────────────────────

    // chain_getBlockHash returns a hex string e.g. "0xabcd...", not a JSON array.
    let best_hex: Option<String> = client
        .request("chain_getBlockHash", rpc_params![])
        .await
        .expect("chain_getBlockHash RPC failed");

    let best_hash: Hash = match best_hex {
        Some(ref s) => {
            let bytes = hex::decode(s.trim_start_matches("0x"))
                .expect("chain_getBlockHash returned invalid hex");
            if bytes.len() != 32 {
                fail!("chain_getBlockHash", format!("expected 32 bytes, got {}", bytes.len()));
                println!("\n{passed} passed, {failed} failed");
                std::process::exit(1);
            }
            let mut h = [0u8; 32];
            h.copy_from_slice(&bytes);
            if h == [0u8; 32] {
                fail!("chain_getBlockHash", "returned H256::zero()");
                println!("\n{passed} passed, {failed} failed");
                std::process::exit(1);
            }
            pass!("chain_getBlockHash", format!("best={}", fmt_hash(&h)));
            h
        },
        None => {
            fail!("chain_getBlockHash", "returned null");
            println!("\n{passed} passed, {failed} failed");
            std::process::exit(1);
        },
    };

    // ── 2. compute_provides_root ─────────────────────────────────────────────
    // Runtime returns Option<ProvidesCommitment>; old bug decoded as
    // ProvidesCommitment directly, eating the 0x01 discriminant as data.

    let provides: Result<Option<ProvidesCommitment>, String> =
        state_call(&client, "SpeculativeOutboxApi_compute_provides_root", &best_hash, vec![])
            .await;

    match &provides {
        Ok(Some(p)) => {
            let root = p.root;
            // Pre-fix symptom: root starts with 0x01 and rest is zeros
            if root[0] == 0x01 && root[1..] == [0u8; 31] {
                fail!(
                    "compute_provides_root",
                    format!("root={} — SCALE discriminant corruption bug (0x01 eaten as data)", fmt_hash(&root))
                );
            } else {
                pass!("compute_provides_root", format!("Some(root={})", fmt_hash(&root)));
            }
        },
        Ok(None) => {
            pass!("compute_provides_root", "None (no messages in outbox yet)");
        },
        Err(e) => {
            fail!("compute_provides_root", e);
        },
    }

    // ── 3. destination_state ─────────────────────────────────────────────────

    let dest_state: Result<Option<(Hash, u64)>, String> = state_call(
        &client,
        "SpeculativeOutboxApi_destination_state",
        &best_hash,
        encode_para_id(DEST_PARA_ID),
    )
    .await;

    match dest_state {
        Ok(Some((root, leaf_count))) => pass!(
            "destination_state",
            format!("Some(root={}, leaf_count={})", fmt_hash(&root), leaf_count)
        ),
        Ok(None) => pass!("destination_state", "None (no messages to dest 2001 yet)"),
        Err(e) => fail!("destination_state", e),
    }

    // ── 4. outbound_messages ─────────────────────────────────────────────────
    // Returns Vec<(u64, Vec<u8>)> — no Option wrapper, correct as-is.

    let mut args = encode_para_id(DEST_PARA_ID);
    args.extend_from_slice(&0u64.to_le_bytes()); // from_position = 0
    args.extend_from_slice(&8u32.to_le_bytes()); // max = 8

    let messages: Result<Vec<(u64, Vec<u8>)>, String> =
        state_call(&client, "SpeculativeOutboxApi_outbound_messages", &best_hash, args).await;

    match messages {
        Ok(msgs) => pass!("outbound_messages", format!("{} message(s)", msgs.len())),
        Err(e) => fail!("outbound_messages", e),
    }

    // ── 5 & 6. subtree_inclusion_proof + block_hash_for_provides_root ────────
    // Only meaningful when compute_provides_root returned Some.

    if let Ok(Some(ref p)) = provides {
        let root = p.root;

        // 5. subtree_inclusion_proof
        let mut proof_args = encode_para_id(DEST_PARA_ID);
        proof_args.extend_from_slice(&root);

        let proof: Result<Option<(Vec<Hash>, u32, u32)>, String> = state_call(
            &client,
            "SpeculativeOutboxApi_subtree_inclusion_proof",
            &best_hash,
            proof_args,
        )
        .await;

        match proof {
            Ok(Some((hashes, num_dests, leaf_idx))) => pass!(
                "subtree_inclusion_proof",
                format!("Some(proof_len={}, num_dests={}, leaf_idx={})", hashes.len(), num_dests, leaf_idx)
            ),
            Ok(None) =>
                fail!("subtree_inclusion_proof", "None — expected Some for non-empty outbox"),
            Err(e) => fail!("subtree_inclusion_proof", e),
        }

        // 6. block_hash_for_provides_root
        let block_hash_result: Result<Option<Hash>, String> = state_call(
            &client,
            "SpeculativeOutboxApi_block_hash_for_provides_root",
            &best_hash,
            root.to_vec(),
        )
        .await;

        match block_hash_result {
            Ok(Some(bh)) => {
                if bh[0] == 0x01 && bh[1..] == [0u8; 31] {
                    fail!(
                        "block_hash_for_provides_root",
                        format!("got {} — SCALE discriminant corruption (pre-fix symptom)", fmt_hash(&bh))
                    );
                } else if bh == [0u8; 32] {
                    fail!(
                        "block_hash_for_provides_root",
                        "got H256::zero() — frame_system::block_hash returned zero (not yet stored?)"
                    );
                } else {
                    // Verify the returned hash is actually known to the node
                    let known: Result<Option<serde_json::Value>, _> =
                        client.request("chain_getHeader", rpc_params![fmt_hash(&bh)]).await;
                    match known {
                        Ok(Some(_)) => pass!(
                            "block_hash_for_provides_root",
                            format!("Some({}) — node recognises this block", fmt_hash(&bh))
                        ),
                        Ok(None) => fail!(
                            "block_hash_for_provides_root",
                            format!("got {} but node does not recognise it", fmt_hash(&bh))
                        ),
                        Err(e) => fail!(
                            "block_hash_for_provides_root",
                            format!("verification RPC error: {e}")
                        ),
                    }
                }
            },
            Ok(None) => fail!(
                "block_hash_for_provides_root",
                "None — provides root not in ProvidesRootIndex (not yet recorded?)"
            ),
            Err(e) => fail!("block_hash_for_provides_root", e),
        }
    } else {
        println!("  SKIP  subtree_inclusion_proof (no provides root)");
        println!("  SKIP  block_hash_for_provides_root (no provides root)");
    }

    // ── Summary ──────────────────────────────────────────────────────────────

    println!("\n{passed} passed, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}
