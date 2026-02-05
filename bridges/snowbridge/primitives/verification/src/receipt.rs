// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use alloy_consensus::ReceiptEnvelope;
use alloy_primitives::{Bytes, B256};
use alloy_rlp::Decodable;
use alloy_trie::{nodes::TrieNode, proof::verify_proof, Nibbles};
use sp_core::H256;
use sp_std::prelude::*;

pub fn verify_receipt_proof(
	receipts_root: H256,
	receipt_index: u64,
	proof: &[Vec<u8>],
) -> Option<ReceiptEnvelope> {
	let key = receipt_trie_key(receipt_index);
	let value = extract_value_from_proof(&key, proof)?;
	let root = B256::from_slice(receipts_root.as_bytes());
	let proof_nodes: Vec<Bytes> = proof.iter().map(|node| Bytes::copy_from_slice(node)).collect();
	verify_proof(root, key, Some(value.clone()), proof_nodes.iter()).ok()?;
	ReceiptEnvelope::decode(&mut value.as_slice()).ok()
}

fn receipt_trie_key(receipt_index: u64) -> Nibbles {
	let encoded_index = rlp::encode(&receipt_index);
	Nibbles::unpack(encoded_index.as_ref())
}

fn extract_value_from_proof(key: &Nibbles, proof: &[Vec<u8>]) -> Option<Vec<u8>> {
	let mut walked_path = Nibbles::new();
	for node in proof {
		let trie_node = TrieNode::decode(&mut &node[..]).ok()?;
		match process_trie_node(trie_node, &mut walked_path, key)? {
			NodeDecodingResult::Node => continue,
			NodeDecodingResult::Value(value) => {
				if *key == walked_path {
					return Some(value);
				}
				return None;
			},
		}
	}
	None
}

enum NodeDecodingResult {
	Node,
	Value(Vec<u8>),
}

fn process_trie_node(
	node: TrieNode,
	walked_path: &mut Nibbles,
	key: &Nibbles,
) -> Option<NodeDecodingResult> {
	match node {
		TrieNode::Branch(branch) => process_branch(branch, walked_path, key),
		TrieNode::Extension(extension) => {
			walked_path.extend(&extension.key);
			if extension.child.is_hash() {
				return Some(NodeDecodingResult::Node);
			}
			process_trie_node(TrieNode::decode(&mut &extension.child[..]).ok()?, walked_path, key)
		},
		TrieNode::Leaf(leaf) => {
			walked_path.extend(&leaf.key);
			Some(NodeDecodingResult::Value(leaf.value))
		},
		TrieNode::EmptyRoot => None,
	}
}

fn process_branch(
	branch: alloy_trie::nodes::BranchNode,
	walked_path: &mut Nibbles,
	key: &Nibbles,
) -> Option<NodeDecodingResult> {
	let next = key.get(walked_path.len())?;
	let mut child_node = None;
	for (index, child) in branch.as_ref().children() {
		if index == next {
			child_node = child.cloned();
			break;
		}
	}
	let child = child_node?;
	walked_path.push(next);
	if child.is_hash() {
		return Some(NodeDecodingResult::Node);
	}
	let decoded = TrieNode::decode(&mut &child[..]).ok()?;
	match decoded {
		TrieNode::Branch(child_branch) => process_branch(child_branch, walked_path, key),
		TrieNode::Extension(child_extension) => {
			walked_path.extend(&child_extension.key);
			if child_extension.child.is_hash() {
				return Some(NodeDecodingResult::Node);
			}
			match TrieNode::decode(&mut &child_extension.child[..]).ok()? {
				TrieNode::Branch(extension_child_branch) =>
					process_branch(extension_child_branch, walked_path, key),
				TrieNode::Leaf(leaf) => {
					walked_path.extend(&leaf.key);
					Some(NodeDecodingResult::Value(leaf.value))
				},
				TrieNode::Extension(_) | TrieNode::EmptyRoot => None,
			}
		},
		TrieNode::Leaf(leaf) => {
			walked_path.extend(&leaf.key);
			Some(NodeDecodingResult::Value(leaf.value))
		},
		TrieNode::EmptyRoot => None,
	}
}
