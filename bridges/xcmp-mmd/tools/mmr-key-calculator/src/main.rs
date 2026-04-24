use sp_io::hashing::twox_128;

#[cfg(test)]
mod verify_test;

fn main() {
    let pallet_hash = twox_128(b"Mmr");
    let storage_hash = twox_128(b"RootHash");

    println!("\n// Storage key for pallet_mmr::RootHash");
    print!("pub const MMR_ROOT_HASH: &[u8] = &hex![\"");
    for byte in pallet_hash.iter().chain(storage_hash.iter()) {
        print!("{:02x}", byte);
    }
    println!("\"];\n");
}
