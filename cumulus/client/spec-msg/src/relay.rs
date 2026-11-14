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

//! The relay chain's view of each source's `StreamsRoot`s: the newest included one and any
//! pending-availability ones ahead of it.

use crate::LOG_TARGET;
use codec::Decode;
use cumulus_primitives_core::ParaId;
use cumulus_primitives_spec_messaging::StreamsRoot;
use cumulus_relay_chain_interface::{call_runtime_api, PHash, RelayChainInterface};
use polkadot_primitives::{
	runtime_api::ParachainHost, well_known_keys, CommittedCandidateReceiptV2,
};
use sp_api::RuntimeApiInfo;
use std::sync::Arc;

/// The relay runtime API giving a source's newest included `StreamsRoot`.
pub const NEWEST_INCLUDED_PROVIDES: &str = "ParachainHost_newest_included_provides";

/// The `ParachainHost` version that added [`NEWEST_INCLUDED_PROVIDES`].
pub const NEWEST_INCLUDED_PROVIDES_VERSION: u32 = 17;

/// The relay storage prefix of `Paras::ParaLifecycles` (`Twox128("Paras") ++
/// Twox128("ParaLifecycles")`).
const PARA_LIFECYCLES_PREFIX: [u8; 32] = [
	0xcd, 0x71, 0x0b, 0x30, 0xbd, 0x2e, 0xab, 0x03, 0x52, 0xdd, 0xcc, 0x26, 0x41, 0x7a, 0xa1, 0x94,
	0xdf, 0x16, 0x62, 0x02, 0x26, 0x2b, 0x9c, 0x55, 0x20, 0x21, 0x39, 0x2f, 0x9c, 0xc3, 0x19, 0x45,
];

/// A source's eligible roots at one relay block: its newest included root and the ordered
/// `Provides` roots of its candidates pending availability.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct SourceRoots {
	/// The newest root included by the relay chain.
	pub included: Option<StreamsRoot>,
	/// The `Provides` roots of candidates pending availability, oldest first, deduplicated and
	/// excluding `included`.
	pub pending: Vec<StreamsRoot>,
}

impl SourceRoots {
	/// A snapshot with only an included root.
	pub fn included(root: StreamsRoot) -> Self {
		Self { included: Some(root), pending: Vec::new() }
	}

	/// Whether the source has neither an included root nor any pending root.
	pub fn is_empty(&self) -> bool {
		self.included.is_none() && self.pending.is_empty()
	}

	/// The newest eligible root: the newest pending root if any, else the included root.
	pub fn newest(&self) -> Option<StreamsRoot> {
		self.pending.last().copied().or(self.included)
	}
}

/// Extract the eligible `Provides` roots from `candidates` in chain order, oldest first.
///
/// Matches the relay chain's own scan: a candidate with a valid UMP signal tail that carries no
/// `Provides` is skipped, whereas a candidate whose UMP signals fail to decode stops the scan and
/// excludes itself and all later candidates. Roots equal to `included` or an earlier pending root
/// are skipped so repeated commitments do not create bogus progress.
pub fn extract_pending_provides(
	included: Option<StreamsRoot>,
	candidates: &[CommittedCandidateReceiptV2],
) -> Vec<StreamsRoot> {
	let mut pending = Vec::new();
	for candidate in candidates {
		let Ok(signals) = candidate.commitments.ump_signals() else { break };
		if let Some(&root) = signals.provides() {
			if included != Some(root) && !pending.contains(&root) {
				pending.push(root);
			}
		}
	}
	pending
}

/// The relay storage key of `Paras::ParaLifecycles` for `para_id`.
pub(crate) fn para_lifecycle_key(para_id: ParaId) -> Vec<u8> {
	// `well_known_keys::para_head(para_id)` is `Twox128("Paras") ++ Twox128("Heads") ++
	// Twox64(para_id) ++ Encode(para_id)`; replace the 32-byte pallet+item prefix with
	// `PARA_LIFECYCLES_PREFIX`.
	let mut key = well_known_keys::para_head(para_id);
	key[..32].copy_from_slice(&PARA_LIFECYCLES_PREFIX);
	key
}

/// Whether `source` is a live (non-onboarding, non-offboarding) para at relay block `at`.
async fn is_live_para(
	relay: &(impl RelayChainInterface + ?Sized),
	at: PHash,
	source: ParaId,
) -> Result<bool, ()> {
	// `ParaLifecycle` variants in `polkadot_runtime_parachains::paras`:
	// 0 = Onboarding, 1 = Parathread, 2 = Parachain, 3 = UpgradingParathread,
	// 4 = DowngradingParachain, 5 = OffboardingParathread, 6 = OffboardingParachain.
	if let Some(raw) = relay
		.get_storage_by_key(at, &para_lifecycle_key(source))
		.await
		.map_err(|e| log::debug!(target: LOG_TARGET, "ParaLifecycles lookup failed: {e}"))?
	{
		let tag = u8::decode(&mut &raw[..]).map_err(|_| ())?;
		return Ok(matches!(tag, 1..=4));
	}
	// Fallback for test harnesses that populate `Heads` without `ParaLifecycles`.
	let head = relay
		.get_storage_by_key(at, &well_known_keys::para_head(source))
		.await
		.map_err(|e| log::debug!(target: LOG_TARGET, "Para head lookup failed: {e}"))?;
	Ok(head.is_some())
}

/// `source`'s newest included root as of relay block `at`. `None` also when the call failed or
/// the relay runtime predates the API.
pub async fn newest_included_provides(
	relay: &(impl RelayChainInterface + ?Sized),
	at: PHash,
	source: ParaId,
) -> Option<StreamsRoot> {
	answered_newest_included_provides(relay, at, source).await.ok().flatten()
}

/// Like [`newest_included_provides`], but `Err` when the call failed, so `Ok(None)` is the relay
/// runtime's own answer that `source` has no root.
pub(crate) async fn answered_newest_included_provides(
	relay: &(impl RelayChainInterface + ?Sized),
	at: PHash,
	source: ParaId,
) -> Result<Option<StreamsRoot>, ()> {
	call_runtime_api::<Option<StreamsRoot>>(relay, NEWEST_INCLUDED_PROVIDES, at, source)
		.await
		.map_err(|e| log::debug!(target: LOG_TARGET, "{NEWEST_INCLUDED_PROVIDES} failed: {e}"))
}

/// `source`'s eligible roots at relay block `at`: its newest included root and the `Provides`
/// roots of its candidates pending availability, queried at the same relay hash `at`.
///
/// `Ok(None)` is the relay runtime's own answer that `source` has no eligible roots (or is
/// offboarding); `Err` when a relay call failed.
pub async fn answered_source_roots(
	relay: &(impl RelayChainInterface + ?Sized),
	at: PHash,
	source: ParaId,
) -> Result<Option<SourceRoots>, ()> {
	let included = answered_newest_included_provides(relay, at, source).await?;
	let candidates = relay.candidates_pending_availability(at, source).await.map_err(
		|e| log::debug!(target: LOG_TARGET, "candidates_pending_availability failed: {e}"),
	)?;
	let pending = extract_pending_provides(included, &candidates);
	if included.is_none() {
		if pending.is_empty() {
			return Ok(None);
		}
		// `newest_included_provides` returns `None` both when `source` has not yet had a `Provides`
		// included and when `source` is offboarding. Check the lifecycle before accepting pending
		// roots without an included root.
		if !is_live_para(relay, at, source).await? {
			return Ok(None);
		}
	}
	Ok(Some(SourceRoots { included, pending }))
}

pub(crate) async fn receiver_roots(
	relay: &(impl RelayChainInterface + ?Sized),
	at: PHash,
	source: ParaId,
	optimistic: bool,
) -> Result<Option<SourceRoots>, ()> {
	if !relay_has_api(relay, at).await {
		return Err(());
	}
	if optimistic {
		answered_source_roots(relay, at, source).await
	} else {
		answered_newest_included_provides(relay, at, source)
			.await
			.map(|root| root.map(SourceRoots::included))
	}
}

/// Whether relay block `at` has [`NEWEST_INCLUDED_PROVIDES`].
pub async fn relay_has_api(relay: &(impl RelayChainInterface + ?Sized), at: PHash) -> bool {
	let id = <dyn ParachainHost<polkadot_primitives::Block>>::ID;
	match relay.version(at).await {
		Ok(version) => version
			.api_version(&id)
			.is_some_and(|version| version >= NEWEST_INCLUDED_PROVIDES_VERSION),
		Err(e) => {
			log::debug!(target: LOG_TARGET, "Relay runtime version at {at:?} unavailable: {e}");
			false
		},
	}
}

/// Each source's newest root included in a finalized relay block.
#[async_trait::async_trait]
pub trait FinalizedRoots: Send + Sync {
	/// `para`'s newest included root as of the finalized relay block; `None` if unknown.
	async fn finalized_included_root(&self, para: ParaId) -> Option<StreamsRoot>;
}

/// [`FinalizedRoots`] read from the relay chain.
pub struct RelayFinalizedRoots(pub Arc<dyn RelayChainInterface>);

#[async_trait::async_trait]
impl FinalizedRoots for RelayFinalizedRoots {
	async fn finalized_included_root(&self, para: ParaId) -> Option<StreamsRoot> {
		let at = self
			.0
			.finalized_block_hash()
			.await
			.map_err(|e| log::debug!(target: LOG_TARGET, "No finalized relay block: {e}"))
			.ok()?;
		if !relay_has_api(&*self.0, at).await {
			return None;
		}
		newest_included_provides(&*self.0, at, para).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Encode;
	use cumulus_primitives_core::relay_chain::vstaging::RelayParentInfo;
	use cumulus_relay_chain_interface::{
		ChildInfo, CommittedCandidateReceipt, CoreIndex, InboundDownwardMessage,
		InboundHrmpMessage, OccupiedCoreAssumption, OverseerHandle, PHeader,
		PersistedValidationData, RelayChainError, RelayChainResult, SessionIndex, StorageValue,
		ValidatorId,
	};
	use futures::{executor::block_on, Stream};
	use parking_lot::Mutex;
	use polkadot_primitives::{
		CandidateCommitments, CandidateDescriptorV2, CandidateEvent, ClaimQueueOffset,
		CoreSelector, CoreState, Hash, HeadData, NodeFeatures, UMPSignal, ValidationCodeHash,
		UMP_SEPARATOR,
	};
	use sc_client_api::StorageProof;
	use std::{
		collections::{BTreeMap, VecDeque},
		pin::Pin,
	};

	fn root(tag: u8) -> StreamsRoot {
		StreamsRoot(Hash::repeat_byte(tag))
	}

	fn receipt_with_ump(upward_messages: Vec<Vec<u8>>) -> CommittedCandidateReceiptV2 {
		CommittedCandidateReceiptV2 {
			descriptor: CandidateDescriptorV2::new_v1(
				ParaId::from(1000u32),
				Hash::zero(),
				Hash::zero(),
				Hash::zero(),
				Hash::zero(),
				Hash::zero(),
				ValidationCodeHash::from(Hash::zero()),
			),
			commitments: CandidateCommitments {
				upward_messages: upward_messages.try_into().unwrap(),
				horizontal_messages: Default::default(),
				new_validation_code: None,
				head_data: HeadData(Vec::new()),
				processed_downward_messages: 0,
				hrmp_watermark: 0,
			},
		}
	}

	fn receipt_provides(provides: StreamsRoot) -> CommittedCandidateReceiptV2 {
		receipt_with_ump(vec![UMP_SEPARATOR, UMPSignal::Provides(provides).encode()])
	}

	fn receipt_select_core_only() -> CommittedCandidateReceiptV2 {
		receipt_with_ump(vec![
			UMP_SEPARATOR,
			UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(1)).encode(),
		])
	}

	fn receipt_no_signals() -> CommittedCandidateReceiptV2 {
		receipt_with_ump(vec![vec![1, 2, 3]])
	}

	fn receipt_malformed_signals() -> CommittedCandidateReceiptV2 {
		// Two `SelectCore` signals violate `ump_signals()` validation.
		receipt_with_ump(vec![
			UMP_SEPARATOR,
			UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(1)).encode(),
			UMPSignal::SelectCore(CoreSelector(1), ClaimQueueOffset(1)).encode(),
		])
	}

	#[derive(Default)]
	struct MockRelay {
		included: Mutex<BTreeMap<(PHash, ParaId), Result<Option<StreamsRoot>, ()>>>,
		pending: Mutex<BTreeMap<(PHash, ParaId), Result<Vec<CommittedCandidateReceiptV2>, ()>>>,
		storage: Mutex<BTreeMap<(PHash, Vec<u8>), Vec<u8>>>,
		queried_included_at: Mutex<Vec<PHash>>,
		queried_pending_at: Mutex<Vec<PHash>>,
	}

	#[async_trait::async_trait]
	impl RelayChainInterface for MockRelay {
		async fn version(&self, _: PHash) -> RelayChainResult<sp_version::RuntimeVersion> {
			let mut version = sp_version::RuntimeVersion::default();
			version.apis = vec![(
				<dyn ParachainHost<polkadot_primitives::Block>>::ID,
				NEWEST_INCLUDED_PROVIDES_VERSION,
			)]
			.into();
			Ok(version)
		}

		async fn call_runtime_api(
			&self,
			method_name: &'static str,
			hash: PHash,
			payload: &[u8],
		) -> RelayChainResult<Vec<u8>> {
			assert_eq!(method_name, NEWEST_INCLUDED_PROVIDES);
			self.queried_included_at.lock().push(hash);
			let source = ParaId::decode(&mut &payload[..]).unwrap();
			match self.included.lock().get(&(hash, source)).cloned().unwrap_or(Ok(None)) {
				Ok(ans) => Ok(ans.encode()),
				Err(()) => Err(RelayChainError::GenericError("runtime api failed".into())),
			}
		}

		async fn candidates_pending_availability(
			&self,
			hash: PHash,
			para_id: ParaId,
		) -> RelayChainResult<Vec<CommittedCandidateReceipt>> {
			self.queried_pending_at.lock().push(hash);
			match self
				.pending
				.lock()
				.get(&(hash, para_id))
				.cloned()
				.unwrap_or_else(|| Ok(Vec::new()))
			{
				Ok(list) => Ok(list),
				Err(()) => Err(RelayChainError::GenericError("pending api failed".into())),
			}
		}

		async fn get_storage_by_key(
			&self,
			relay_parent: PHash,
			key: &[u8],
		) -> RelayChainResult<Option<StorageValue>> {
			Ok(self.storage.lock().get(&(relay_parent, key.to_vec())).cloned())
		}

		async fn retrieve_dmq_contents(
			&self,
			_: ParaId,
			_: PHash,
		) -> RelayChainResult<Vec<InboundDownwardMessage>> {
			unimplemented!()
		}

		async fn retrieve_all_inbound_hrmp_channel_contents(
			&self,
			_: ParaId,
			_: PHash,
		) -> RelayChainResult<BTreeMap<ParaId, Vec<InboundHrmpMessage>>> {
			unimplemented!()
		}

		async fn persisted_validation_data(
			&self,
			_: PHash,
			_: ParaId,
			_: OccupiedCoreAssumption,
		) -> RelayChainResult<Option<PersistedValidationData>> {
			unimplemented!()
		}

		async fn validation_code_hash(
			&self,
			_: PHash,
			_: ParaId,
			_: OccupiedCoreAssumption,
		) -> RelayChainResult<Option<ValidationCodeHash>> {
			unimplemented!()
		}

		async fn candidate_pending_availability(
			&self,
			_: PHash,
			_: ParaId,
		) -> RelayChainResult<Option<CommittedCandidateReceipt>> {
			unimplemented!()
		}

		async fn session_index_for_child(&self, _: PHash) -> RelayChainResult<SessionIndex> {
			unimplemented!()
		}

		async fn validators(&self, _: PHash) -> RelayChainResult<Vec<ValidatorId>> {
			unimplemented!()
		}

		async fn import_notification_stream(
			&self,
		) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
			unimplemented!()
		}

		async fn finality_notification_stream(
			&self,
		) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
			unimplemented!()
		}

		async fn best_block_hash(&self) -> RelayChainResult<PHash> {
			unimplemented!()
		}

		async fn finalized_block_hash(&self) -> RelayChainResult<PHash> {
			unimplemented!()
		}

		async fn is_major_syncing(&self) -> RelayChainResult<bool> {
			unimplemented!()
		}

		fn overseer_handle(&self) -> RelayChainResult<OverseerHandle> {
			unimplemented!()
		}

		async fn prove_read(&self, _: PHash, _: &Vec<Vec<u8>>) -> RelayChainResult<StorageProof> {
			unimplemented!()
		}

		async fn prove_child_read(
			&self,
			_: PHash,
			_: &ChildInfo,
			_: &[Vec<u8>],
		) -> RelayChainResult<StorageProof> {
			unimplemented!()
		}

		async fn wait_for_block(&self, _: PHash) -> RelayChainResult<()> {
			unimplemented!()
		}

		async fn new_best_notification_stream(
			&self,
		) -> RelayChainResult<Pin<Box<dyn Stream<Item = PHeader> + Send>>> {
			unimplemented!()
		}

		async fn header(
			&self,
			_: polkadot_primitives::BlockId,
		) -> RelayChainResult<Option<PHeader>> {
			unimplemented!()
		}

		async fn availability_cores(
			&self,
			_: PHash,
		) -> RelayChainResult<Vec<CoreState<Hash, polkadot_primitives::BlockNumber>>> {
			unimplemented!()
		}

		async fn claim_queue(
			&self,
			_: PHash,
		) -> RelayChainResult<BTreeMap<CoreIndex, VecDeque<ParaId>>> {
			unimplemented!()
		}

		async fn scheduling_lookahead(&self, _: PHash) -> RelayChainResult<u32> {
			unimplemented!()
		}

		async fn candidate_events(&self, _: PHash) -> RelayChainResult<Vec<CandidateEvent>> {
			unimplemented!()
		}

		async fn max_relay_parent_session_age(&self, _: PHash) -> RelayChainResult<u32> {
			unimplemented!()
		}

		async fn node_features(&self, _: PHash) -> RelayChainResult<NodeFeatures> {
			unimplemented!()
		}

		async fn ancestor_relay_parent_info(
			&self,
			_: PHash,
			_: SessionIndex,
			_: PHash,
		) -> RelayChainResult<Option<RelayParentInfo<PHash, polkadot_primitives::BlockNumber>>> {
			unimplemented!()
		}
	}

	#[test]
	fn pending_roots_require_explicit_optimistic_policy() {
		let relay = MockRelay::default();
		let source = ParaId::from(1000u32);
		let at = PHash::repeat_byte(0x40);
		relay.included.lock().insert((at, source), Ok(Some(root(1))));
		relay.pending.lock().insert((at, source), Ok(vec![receipt_provides(root(2))]));
		assert_eq!(
			block_on(receiver_roots(&relay, at, source, false)),
			Ok(Some(SourceRoots::included(root(1))))
		);
		assert!(relay.queried_pending_at.lock().is_empty());
		assert_eq!(
			block_on(receiver_roots(&relay, at, source, true)),
			Ok(Some(SourceRoots { included: Some(root(1)), pending: vec![root(2)] }))
		);
		relay.included.lock().insert((at, source), Ok(None));
		assert_eq!(block_on(receiver_roots(&relay, at, source, false)), Ok(None));
		relay.included.lock().insert((at, source), Err(()));
		assert_eq!(block_on(receiver_roots(&relay, at, source, false)), Err(()));
	}

	#[test]
	fn pending_receipts_preserve_order_and_skip_valid_non_provides_tails() {
		let r0 = root(0);
		let r1 = root(1);
		let r2 = root(2);
		let candidates = vec![
			receipt_no_signals(),
			receipt_provides(r1),
			receipt_select_core_only(),
			receipt_provides(r2),
		];
		assert_eq!(extract_pending_provides(Some(r0), &candidates), vec![r1, r2]);
	}

	#[test]
	fn malformed_signal_tail_stops_scan_and_excludes_later_roots() {
		let r1 = root(1);
		let r2 = root(2);
		let candidates =
			vec![receipt_provides(r1), receipt_malformed_signals(), receipt_provides(r2)];
		assert_eq!(extract_pending_provides(None, &candidates), vec![r1]);
	}

	#[test]
	fn duplicate_and_already_included_roots_are_deduplicated() {
		let r0 = root(0);
		let r1 = root(1);
		let r2 = root(2);
		let candidates = vec![
			receipt_provides(r0),
			receipt_provides(r1),
			receipt_provides(r1),
			receipt_provides(r2),
			receipt_provides(r1),
		];
		assert_eq!(extract_pending_provides(Some(r0), &candidates), vec![r1, r2]);
	}

	#[test]
	fn queries_included_and_pending_at_the_same_relay_hash() {
		let relay = MockRelay::default();
		let source = ParaId::from(1000u32);
		let at = PHash::repeat_byte(0x42);
		relay.included.lock().insert((at, source), Ok(Some(root(1))));
		relay
			.pending
			.lock()
			.insert((at, source), Ok(vec![receipt_provides(root(2)), receipt_provides(root(3))]));

		let roots = block_on(answered_source_roots(&relay, at, source)).unwrap().unwrap();
		assert_eq!(roots, SourceRoots { included: Some(root(1)), pending: vec![root(2), root(3)] },);
		assert_eq!(*relay.queried_included_at.lock(), vec![at]);
		assert_eq!(*relay.queried_pending_at.lock(), vec![at]);
	}

	#[test]
	fn first_ever_commitment_pending_without_included_root_is_accepted_when_live() {
		let relay = MockRelay::default();
		let source = ParaId::from(1000u32);
		let at = PHash::repeat_byte(0x11);
		relay.included.lock().insert((at, source), Ok(None));
		relay.pending.lock().insert((at, source), Ok(vec![receipt_provides(root(1))]));
		// Lifecycle 2 = `ParaLifecycle::Parachain`.
		relay.storage.lock().insert((at, para_lifecycle_key(source)), vec![2u8]);

		let roots = block_on(answered_source_roots(&relay, at, source)).unwrap().unwrap();
		assert_eq!(roots, SourceRoots { included: None, pending: vec![root(1)] });
		assert_eq!(roots.newest(), Some(root(1)));
	}

	#[test]
	fn offboarding_source_with_pending_receipt_returns_none() {
		let relay = MockRelay::default();
		let source = ParaId::from(1000u32);
		let at = PHash::repeat_byte(0x22);
		// `newest_included_provides` returns `None` when offboarding, even if a pending receipt
		// still sits on a core.
		relay.included.lock().insert((at, source), Ok(None));
		relay.pending.lock().insert((at, source), Ok(vec![receipt_provides(root(1))]));
		// Lifecycle 6 = `ParaLifecycle::OffboardingParachain`.
		relay.storage.lock().insert((at, para_lifecycle_key(source)), vec![6u8]);

		let roots = block_on(answered_source_roots(&relay, at, source)).unwrap();
		assert_eq!(roots, None);
	}

	#[test]
	fn api_error_is_distinguished_from_empty_answer() {
		let relay = MockRelay::default();
		let source = ParaId::from(1000u32);
		let at_empty = PHash::repeat_byte(0x33);
		let at_inc_err = PHash::repeat_byte(0x34);
		let at_pend_err = PHash::repeat_byte(0x35);

		relay.included.lock().insert((at_empty, source), Ok(None));
		relay.pending.lock().insert((at_empty, source), Ok(Vec::new()));
		assert_eq!(block_on(answered_source_roots(&relay, at_empty, source)), Ok(None));

		relay.included.lock().insert((at_inc_err, source), Err(()));
		assert_eq!(block_on(answered_source_roots(&relay, at_inc_err, source)), Err(()));

		relay.included.lock().insert((at_pend_err, source), Ok(Some(root(1))));
		relay.pending.lock().insert((at_pend_err, source), Err(()));
		assert_eq!(block_on(answered_source_roots(&relay, at_pend_err, source)), Err(()));
	}
}
