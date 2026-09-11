// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Snowfork <hello@snowfork.com>
//! Append-only Merkle Mountain Range over per-block commitment roots.
//!
//! The outbound queue commits one binary-Merkle root per block (see [`crate::merkle_root`]). That
//! root is only provable on Ethereum if the block's header is reachable through the relay's
//! parachain-heads root — which holds for the last block of a candidate, but not for earlier ones
//! when several parachain blocks share a candidate.
//!
//! Appending each block root to an MMR fixes that: a later MMR root already commits to every
//! earlier block root, so an unreachable header costs nothing. Only the most recent root has to be
//! provable.
//!
//! State is O(log n): the peaks, plus the leaf count that determines their heights.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_runtime::traits::Hash;

/// Hash an interior node. Children are concatenated left-to-right, with no domain tag, matching
/// [`crate::merkle_root`]'s node hashing so both trees verify identically in Solidity.
///
/// Public because any verifier of an MMR proof — including the Solidity side — must reproduce it.
pub fn node<H: Hash<Output = H256>>(left: &H256, right: &H256) -> H256 {
	let mut buf = [0u8; 64];
	buf[..32].copy_from_slice(left.as_bytes());
	buf[32..].copy_from_slice(right.as_bytes());
	<H as Hash>::hash(&buf)
}

/// Append `leaf`, merging peaks of equal height. Returns the new leaf count.
///
/// `peaks` holds one hash per occupied height, highest first; which heights are occupied is fully
/// determined by `leaf_count`'s binary representation, so the heights need not be stored.
pub fn append<H: Hash<Output = H256>>(peaks: &mut Vec<H256>, leaf_count: u64, leaf: H256) -> u64 {
	let mut carry = leaf;
	let mut n = leaf_count;
	// Each set low bit is a peak of the height we are carrying into; merge and carry up.
	while n & 1 == 1 {
		let left = match peaks.pop() {
			Some(left) => left,
			// Unreachable while `peaks`/`leaf_count` are updated together, but the runtime must
			// not panic: drop the carry rather than halt the block.
			None => return leaf_count.saturating_add(1),
		};
		carry = node::<H>(&left, &carry);
		n >>= 1;
	}
	peaks.push(carry);
	leaf_count.saturating_add(1)
}

/// Bag the peaks into a single root, right to left.
///
/// Empty returns the zero hash, matching [`crate::merkle_root`]'s empty case.
pub fn root<H: Hash<Output = H256>>(peaks: &[H256]) -> H256 {
	let mut iter = peaks.iter().rev();
	let mut acc = match iter.next() {
		Some(last) => *last,
		None => return H256::default(),
	};
	for peak in iter {
		acc = node::<H>(peak, &acc);
	}
	acc
}

/// A proof that a leaf is contained in an MMR with a given root.
///
/// A leaf lives inside exactly one peak — a perfect subtree. `path` reconstructs that peak, then
/// the surrounding peaks re-bag it into the root, so the whole proof is `O(log n)`.
#[derive(Encode, Decode, Debug, PartialEq, Eq, TypeInfo)]
pub struct MmrProof {
	/// Index of the proven leaf (0-based).
	pub leaf_index: u64,
	/// Total leaves in the MMR the proof was generated against.
	pub leaf_count: u64,
	/// Siblings within the containing peak, bottom-up.
	pub path: Vec<H256>,
	/// Peaks covering earlier leaves, in bagging order.
	pub peaks_left: Vec<H256>,
	/// Peaks covering later leaves, in bagging order.
	pub peaks_right: Vec<H256>,
}

/// `(start, size)` of each peak, highest first — the order [`root`] bags them in.
fn peak_ranges(leaf_count: u64) -> Vec<(u64, u64)> {
	let mut ranges = Vec::new();
	let mut start = 0u64;
	for height in (0..u64::BITS).rev() {
		if (leaf_count >> height) & 1 == 1 {
			let size = 1u64 << height;
			ranges.push((start, size));
			start = start.saturating_add(size);
		}
	}
	ranges
}

/// Root of a *peak*: a perfect subtree covering exactly `2^h` leaves.
///
/// Named for the shape rather than the role because the shape is the precondition — every level is
/// full, so leaves pair up exactly and there is none of the odd-leaf promotion [`crate::merkle_root`]
/// performs. [`peak_ranges`] only ever yields power-of-two sizes, but this must not panic in a
/// runtime if a future caller gets that wrong, so an odd level carries its last element up.
fn peak_root<H: Hash<Output = H256>>(leaves: &[H256]) -> Option<H256> {
	let mut level = leaves.to_vec();
	while level.len() > 1 {
		level = level
			.chunks(2)
			.map(|pair| match pair {
				[left, right] => node::<H>(left, right),
				// Unreachable for a power-of-two slice; promote rather than panic.
				[single] => *single,
				_ => H256::default(),
			})
			.collect();
	}
	level.first().copied()
}

/// Generate a proof for `leaf_index` against the MMR formed by `leaves`.
///
/// Returns `None` if the index is out of range. The caller supplies the leaves because the chain
/// keeps only the peaks; historical leaves are recovered off-chain (for the outbound queue, from
/// the per-block root in `MessagesCommitted`).
pub fn proof<H: Hash<Output = H256>>(leaves: &[H256], leaf_index: u64) -> Option<MmrProof> {
	let leaf_count = leaves.len() as u64;
	if leaf_index >= leaf_count {
		return None;
	}

	let ranges = peak_ranges(leaf_count);
	let mut peaks_left = Vec::new();
	let mut peaks_right = Vec::new();
	let mut path = Vec::new();
	let mut seen_owner = false;

	for (start, size) in ranges {
		let slice = &leaves[start as usize..(start + size) as usize];
		if !seen_owner && leaf_index < start + size {
			// The owning peak: record the sibling path instead of the peak itself.
			let mut local = leaf_index - start;
			let mut level = slice.to_vec();
			while level.len() > 1 {
				let sibling = if local & 1 == 0 { local + 1 } else { local - 1 };
				path.push(level[sibling as usize]);
				level = level.chunks(2).map(|pair| node::<H>(&pair[0], &pair[1])).collect();
				local >>= 1;
			}
			seen_owner = true;
		} else if seen_owner {
			peaks_right.push(peak_root::<H>(slice)?);
		} else {
			peaks_left.push(peak_root::<H>(slice)?);
		}
	}

	Some(MmrProof { leaf_index, leaf_count, path, peaks_left, peaks_right })
}

/// Verify that `leaf` sits at `proof.leaf_index` in the MMR with the given `root`.
///
/// Nothing semantic is taken from the proof: the caller supplies the leaf and the root, and the
/// side of each step is derived from `leaf_index`, so a proof cannot relocate a leaf.
pub fn verify<H: Hash<Output = H256>>(root_hash: &H256, leaf: &H256, proof: &MmrProof) -> bool {
	if proof.leaf_index >= proof.leaf_count {
		return false;
	}

	// Rebuild the owning peak from the leaf upwards.
	let mut acc = *leaf;
	let mut local = proof.leaf_index - peak_ranges(proof.leaf_count)
		.iter()
		.take(proof.peaks_left.len())
		.map(|(_, size)| size)
		.sum::<u64>();
	for sibling in &proof.path {
		acc = if local & 1 == 0 { node::<H>(&acc, sibling) } else { node::<H>(sibling, &acc) };
		local >>= 1;
	}

	// Bag the peaks to the right, then fold the ones to the left.
	if let Some((last, rest)) = proof.peaks_right.split_last() {
		let mut right = *last;
		for peak in rest.iter().rev() {
			right = node::<H>(peak, &right);
		}
		acc = node::<H>(&acc, &right);
	}
	for peak in proof.peaks_left.iter().rev() {
		acc = node::<H>(peak, &acc);
	}

	acc == *root_hash
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_crypto_hashing::keccak_256;
	use sp_runtime::traits::Keccak256;

	fn leaf(i: u64) -> H256 {
		keccak_256(&i.to_le_bytes()).into()
	}

	/// Append `n` leaves, returning the peaks and the leaf count.
	fn build(n: u64) -> (Vec<H256>, u64) {
		let mut peaks = Vec::new();
		let mut count = 0u64;
		for i in 0..n {
			count = append::<Keccak256>(&mut peaks, count, leaf(i));
		}
		(peaks, count)
	}

	#[test]
	fn empty_root_is_zero() {
		assert_eq!(root::<Keccak256>(&[]), H256::default());
	}

	#[test]
	fn single_leaf_root_is_the_leaf() {
		let (peaks, count) = build(1);
		assert_eq!(count, 1);
		assert_eq!(peaks, vec![leaf(0)]);
		assert_eq!(root::<Keccak256>(&peaks), leaf(0));
	}

	#[test]
	fn peaks_track_the_binary_representation_of_the_leaf_count() {
		// Each set bit of `count` is one peak, so the peak count is its population count.
		for n in 0..=32u64 {
			let (peaks, count) = build(n);
			assert_eq!(count, n);
			assert_eq!(peaks.len() as u32, n.count_ones(), "leaf count {n}");
		}
	}

	#[test]
	fn merges_pairs_and_bags_right_to_left() {
		let (peaks, _) = build(3);
		// 3 = 0b11: a height-1 peak over leaves 0,1 and a height-0 peak holding leaf 2.
		let merged = node::<Keccak256>(&leaf(0), &leaf(1));
		assert_eq!(peaks, vec![merged, leaf(2)]);
		assert_eq!(root::<Keccak256>(&peaks), node::<Keccak256>(&merged, &leaf(2)));
	}

	#[test]
	fn power_of_two_collapses_to_one_peak() {
		let (peaks, _) = build(4);
		assert_eq!(peaks.len(), 1);
		let left = node::<Keccak256>(&leaf(0), &leaf(1));
		let right = node::<Keccak256>(&leaf(2), &leaf(3));
		assert_eq!(root::<Keccak256>(&peaks), node::<Keccak256>(&left, &right));
	}

	#[test]
	fn proof_roundtrips_for_every_index_and_size() {
		for n in 1..=17u64 {
			let leaves: Vec<H256> = (0..n).map(leaf).collect();
			let (peaks, _) = build(n);
			let r = root::<Keccak256>(&peaks);
			for i in 0..n {
				let p = proof::<Keccak256>(&leaves, i).expect("index in range");
				assert!(verify::<Keccak256>(&r, &leaf(i), &p), "n={n} i={i}");
			}
		}
	}

	#[test]
	fn an_early_leaf_verifies_against_a_much_later_root() {
		// The property the outbound queue depends on: a block root committed long ago is still
		// provable against the current MMR root, so an unreachable header costs nothing.
		let n = 12u64;
		let leaves: Vec<H256> = (0..n).map(leaf).collect();
		let (peaks, count) = build(n);
		assert_eq!(count, n);
		let latest = root::<Keccak256>(&peaks);

		let p = proof::<Keccak256>(&leaves, 0).expect("first leaf");
		assert!(verify::<Keccak256>(&latest, &leaf(0), &p));
	}

	#[test]
	fn rejects_wrong_leaf_or_tampered_proof() {
		let n = 9u64;
		let leaves: Vec<H256> = (0..n).map(leaf).collect();
		let (peaks, _) = build(n);
		let r = root::<Keccak256>(&peaks);
		let p = proof::<Keccak256>(&leaves, 3).expect("index in range");

		// Right proof, wrong leaf.
		assert!(!verify::<Keccak256>(&r, &leaf(4), &p));
		// Right leaf, wrong root.
		assert!(!verify::<Keccak256>(&leaf(0), &leaf(3), &p));

		// Claiming a different index does not relocate the leaf: sides are derived from it.
		let mut moved = proof::<Keccak256>(&leaves, 3).unwrap();
		moved.leaf_index = 2;
		assert!(!verify::<Keccak256>(&r, &leaf(3), &moved));

		// A tampered sibling breaks the path.
		let mut tampered = proof::<Keccak256>(&leaves, 3).unwrap();
		if let Some(first) = tampered.path.first_mut() {
			*first = leaf(99);
		}
		assert!(!verify::<Keccak256>(&r, &leaf(3), &tampered));
	}

	#[test]
	fn out_of_range_index_yields_no_proof() {
		let leaves: Vec<H256> = (0..5u64).map(leaf).collect();
		assert!(proof::<Keccak256>(&leaves, 5).is_none());
		assert!(proof::<Keccak256>(&leaves, u64::MAX).is_none());
	}

	#[test]
	fn appending_changes_the_root_and_never_rewrites_history() {
		// The property the outbound queue relies on: every prefix root is reachable from the peaks
		// of any later state, so an earlier block root is never orphaned by a later append.
		let mut seen = Vec::new();
		let mut peaks = Vec::new();
		let mut count = 0u64;
		for i in 0..16u64 {
			count = append::<Keccak256>(&mut peaks, count, leaf(i));
			let r = root::<Keccak256>(&peaks);
			assert!(!seen.contains(&r), "root repeated after appending leaf {i}");
			seen.push(r);
		}
		assert_eq!(count, 16);
	}
}
