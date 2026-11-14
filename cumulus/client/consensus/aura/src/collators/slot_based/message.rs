// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! The `builder -> collator` channel: the [`CollatorMessage`] payload and its
//! [`CollatorMessageBuilder`]. Pure data; the collation task builds and submits the collations.

use cumulus_primitives_core::SchedulingProof;
use polkadot_primitives::{
	CoreIndex, Hash as RelayHash, PersistedValidationData, ValidationCodeHash,
};
use sp_api::StorageProof;
use sp_runtime::traits::Block as BlockT;

/// A message from the block builder to the collation task: a single V2 collation, or a V3 segment.
pub(super) enum CollatorMessage<Block: BlockT> {
	/// A single V2 collation (no scheduling proof).
	Collation { core_index: CoreIndex, parts: CollationParts<Block> },
	/// A V3 segment: collations sharing a scheduling parent and target core.
	Segment {
		/// Shared by the whole segment; V3/V4-only, so always present.
		scheduling_proof: SchedulingProof,
		core_index: CoreIndex,
		/// Old bundles to submit before new work, oldest first.
		resubmittable: Vec<(CollationParts<Block>, SchedulingProof)>,
		/// The freshly-built bundle for this core, if any.
		bundle: Option<CollationParts<Block>>,
	},
}

/// The block builder's output for one collation: the built blocks plus the context the collation
/// task needs to turn them into a `SegmentCollation`.
#[derive(Clone)]
pub(super) struct CollationParts<Block: BlockT> {
	/// Relay block the parachain block(s) execute against.
	pub relay_parent: RelayHash,
	/// Parent of the first block.
	pub parent_header: Block::Header,
	/// The built blocks, in order.
	pub blocks: Vec<Block>,
	/// Storage proof covering all of `blocks`.
	pub proof: StorageProof,
	pub validation_code_hash: ValidationCodeHash,
	pub validation_data: PersistedValidationData,
}

/// Builds a [`CollatorMessage`] for one core: a `Collation` without a scheduling proof, a `Segment`
/// with one.
pub(super) struct CollatorMessageBuilder<Block: BlockT> {
	core_index: CoreIndex,
	bundle: Option<CollationParts<Block>>,
	scheduling_proof: Option<SchedulingProof>,
	resubmittable: Vec<(CollationParts<Block>, SchedulingProof)>,
}

impl<Block: BlockT> CollatorMessageBuilder<Block> {
	pub(super) fn new(
		core_index: CoreIndex,
		resubmittable: Vec<(CollationParts<Block>, SchedulingProof)>,
	) -> Self {
		Self { core_index, bundle: None, scheduling_proof: None, resubmittable }
	}

	/// Attach the freshly-built bundle.
	pub(super) fn with_bundle(mut self, bundle: CollationParts<Block>) -> Self {
		self.bundle = Some(bundle);
		self
	}

	/// Attach a V3 scheduling proof.
	pub(super) fn with_scheduling_proof(mut self, scheduling_proof: SchedulingProof) -> Self {
		self.scheduling_proof = Some(scheduling_proof);
		self
	}

	/// Build the message. `None` when there is nothing to submit for the core.
	pub(super) fn build(self) -> Option<CollatorMessage<Block>> {
		if self.resubmittable.len() + usize::from(self.bundle.is_some()) >
			polkadot_node_primitives::MAX_SEGMENT_LEN as usize
		{
			return None;
		}
		match self.scheduling_proof {
			Some(scheduling_proof) => {
				if self.bundle.is_none() && self.resubmittable.is_empty() {
					return None;
				}

				Some(CollatorMessage::Segment {
					scheduling_proof,
					core_index: self.core_index,
					resubmittable: self.resubmittable,
					bundle: self.bundle,
				})
			},
			None if !self.resubmittable.is_empty() => None,
			None => self
				.bundle
				.map(|parts| CollatorMessage::Collation { core_index: self.core_index, parts }),
		}
	}
}

impl<Block: BlockT> CollationParts<Block> {
	/// The new chain tip (last built block), used to chain the next core's PoV parent. `None` for
	/// the empty bundle, which the builder never produces.
	pub(super) fn tip_header(&self) -> Option<&Block::Header> {
		self.blocks.last().map(|block| block.header())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use cumulus_test_client::runtime::Block as TestBlock;
	use sp_runtime::traits::Header;
	fn header() -> <TestBlock as BlockT>::Header {
		Header::new(
			1,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		)
	}

	fn parts() -> CollationParts<TestBlock> {
		CollationParts {
			relay_parent: Default::default(),
			parent_header: header(),
			blocks: vec![TestBlock::new(header(), vec![])],
			proof: StorageProof::empty(),
			validation_code_hash: ValidationCodeHash::from(RelayHash::zero()),
			validation_data: PersistedValidationData {
				parent_head: Default::default(),
				relay_parent_number: 0,
				relay_parent_storage_root: Default::default(),
				max_pov_size: 1024,
			},
		}
	}

	fn proof() -> SchedulingProof {
		SchedulingProof {
			header_chain: Vec::new(),
			internal_scheduling_parent_header: Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				Default::default(),
			),
			signed_scheduling_info: None,
		}
	}

	#[test]
	fn old_bundles_can_form_a_segment_without_a_new_bundle() {
		let headers = vec![(parts(), proof()), (parts(), proof())];
		let message = CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), headers.clone())
			.with_scheduling_proof(proof())
			.build();
		let Some(CollatorMessage::Segment { core_index, resubmittable, bundle, .. }) = message
		else {
			panic!("expected a resubmission segment");
		};
		assert_eq!(core_index, CoreIndex(1));
		assert_eq!(resubmittable.len(), headers.len());
		assert_eq!(resubmittable[0].0.blocks, headers[0].0.blocks);
		assert!(bundle.is_none());
	}

	#[test]
	fn empty_segment_is_not_submitted() {
		assert!(CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), Vec::new())
			.with_scheduling_proof(proof())
			.build()
			.is_none());
	}

	#[test]
	fn old_bundles_do_not_enable_v3_without_a_scheduling_proof() {
		assert!(CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), vec![(parts(), proof())])
			.build()
			.is_none());
	}
	#[test]
	fn old_and_new_bundles_are_kept_separate() {
		let old = parts();
		let mut new = parts();
		let new_header = Header::new(
			2,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		);
		new.blocks = vec![TestBlock::new(new_header, vec![])];
		let message =
			CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), vec![(old.clone(), proof())])
				.with_bundle(new.clone())
				.with_scheduling_proof(proof())
				.build();
		let Some(CollatorMessage::Segment { resubmittable, bundle: Some(bundle), .. }) = message
		else {
			panic!("expected old and new bundles");
		};
		assert_eq!(resubmittable.len(), 1);
		assert_eq!(resubmittable[0].0.blocks, old.blocks);
		assert_eq!(bundle.blocks, new.blocks);
	}

	#[test]
	fn a_full_recovery_segment_does_not_fit_a_fresh_bundle() {
		let old = vec![(parts(), proof()); polkadot_node_primitives::MAX_SEGMENT_LEN as usize];
		assert!(CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), old.clone())
			.with_scheduling_proof(proof())
			.build()
			.is_some());
		assert!(CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), old)
			.with_bundle(parts())
			.with_scheduling_proof(proof())
			.build()
			.is_none());
	}

	#[test]
	fn fresh_v2_bundle_still_builds_without_a_scheduling_proof() {
		assert!(matches!(
			CollatorMessageBuilder::<TestBlock>::new(CoreIndex(1), vec![])
				.with_bundle(parts())
				.build(),
			Some(CollatorMessage::Collation { .. })
		));
	}
}
