// Temporary script to calculate MMR RootHash storage key
use sp_io::hashing::twox_128;

fn main() {
    let pallet_hash = twox_128(b"Mmr");
    let storage_hash = twox_128(b"RootHash");

    print!("MMR_ROOT_HASH key: 0x");
    for byte in pallet_hash.iter().chain(storage_hash.iter()) {
        print!("{:02x}", byte);
    }
    println!();
}
