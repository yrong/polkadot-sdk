// Test to verify MMR_ROOT_HASH storage key matches Westend runtime
use sp_io::hashing::twox_128;

#[test]
fn verify_mmr_root_hash_key_matches_westend() {
    // Westend runtime uses "Mmr" as the pallet name (see polkadot/runtime/westend/src/lib.rs:1950)
    let pallet_hash = twox_128(b"Mmr");
    let storage_hash = twox_128(b"RootHash");

    let mut calculated_key = Vec::new();
    calculated_key.extend_from_slice(&pallet_hash);
    calculated_key.extend_from_slice(&storage_hash);

    // This is the key we added to polkadot/primitives/src/v9/mod.rs
    let expected_key = hex_literal::hex!["a8c65209d47ee80f56b0011e8fd91f50d42f676807518c67bb427546ba406fa1"];

    assert_eq!(calculated_key.as_slice(), &expected_key[..],
        "MMR_ROOT_HASH storage key must match Westend's pallet_mmr::RootHash");

    println!("✓ Storage key verified: matches Westend runtime 'Mmr' pallet");
}
