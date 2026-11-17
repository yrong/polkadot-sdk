// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Withhold a parachain's availability data and chunks while a local control file exists.

use crate::{interceptor::*, shared::MALUS};
use futures::channel::oneshot;
use polkadot_cli::{
	service::{
		AuxStore, Error, ExtendedOverseerGenArgs, Overseer, OverseerConnector, OverseerGen,
		OverseerGenArgs, OverseerHandle,
	},
	validator_overseer_builder, Cli,
};
use polkadot_node_primitives::ValidationResult;
use polkadot_node_subsystem::SpawnGlue;
use polkadot_node_subsystem_types::{ChainApiBackend, RuntimeApiSubsystemClient};
use polkadot_primitives::{BackedCandidate, CandidateHash, HeadData, Id as ParaId};
use sp_core::traits::SpawnNamed;
use std::{
	collections::BTreeMap,
	path::PathBuf,
	sync::{Arc, Mutex},
};

#[derive(Debug, clap::Parser)]
#[allow(missing_docs)]
pub struct WithholdAvailabilityOptions {
	#[arg(long)]
	pub withhold_para: u32,
	/// Control file gating availability. When empty, all candidates of `withhold_para` are gated;
	/// when non-empty, holds the baseline `HeadData` bytes that remain available.
	#[arg(long)]
	pub availability_gate: PathBuf,
	/// Also omit gated candidates from this validator's authored blocks when `<gate>.fork` exists.
	#[arg(long)]
	pub omit_backed_candidates: bool,
	#[command(flatten)]
	pub cli: Cli,
}

pub(crate) struct WithholdAvailability {
	pub para: ParaId,
	pub gate: PathBuf,
	pub omit_backed_candidates: bool,
}

#[derive(Clone)]
struct Fault {
	para: ParaId,
	gate: PathBuf,
	candidates: Arc<Mutex<BTreeMap<CandidateHash, HeadData>>>,
}

impl Fault {
	fn is_gated(baseline: &[u8], head: &HeadData) -> bool {
		baseline.is_empty() || baseline != head.0.as_slice()
	}

	fn filter_backed_candidates(&self, candidates: &mut Vec<BackedCandidate>) {
		let Ok(baseline) = std::fs::read(&self.gate) else { return };
		candidates.retain(|candidate| {
			candidate.descriptor().para_id() != self.para ||
				!Self::is_gated(&baseline, &candidate.candidate().commitments.head_data)
		});
	}

	fn blocked(&self, hash: &CandidateHash) -> bool {
		let Ok(baseline) = std::fs::read(&self.gate) else { return false };
		self.candidates
			.lock()
			.expect("fault state lock")
			.get(hash)
			.is_some_and(|head| Self::is_gated(&baseline, head))
	}

	fn filter(&self, msg: AvailabilityStoreMessage) -> Option<AvailabilityStoreMessage> {
		match msg {
			AvailabilityStoreMessage::QueryAvailableData(hash, tx) if self.blocked(&hash) => {
				let _ = tx.send(None);
			},
			AvailabilityStoreMessage::QueryDataAvailability(hash, tx) if self.blocked(&hash) => {
				let _ = tx.send(false);
			},
			AvailabilityStoreMessage::QueryChunk(hash, _, tx) if self.blocked(&hash) => {
				let _ = tx.send(None);
			},
			AvailabilityStoreMessage::QueryChunkSize(hash, tx) if self.blocked(&hash) => {
				let _ = tx.send(None);
			},
			AvailabilityStoreMessage::QueryAllChunks(hash, tx) if self.blocked(&hash) => {
				let _ = tx.send(Vec::new());
			},
			AvailabilityStoreMessage::QueryChunkAvailability(hash, _, tx)
				if self.blocked(&hash) =>
			{
				gum::info!(target: MALUS, candidate_hash = ?hash, "Withholding availability.");
				let _ = tx.send(false);
			},
			msg => return Some(msg),
		}
		None
	}
}

#[derive(Clone)]
struct OmitBackedCandidates<Spawner> {
	fault: Fault,
	spawner: Spawner,
}

impl<Sender, Spawner> MessageInterceptor<Sender> for OmitBackedCandidates<Spawner>
where
	Sender: overseer::ProvisionerSenderTrait + Clone + Send + 'static,
	Spawner: overseer::gen::Spawner + Clone + 'static,
{
	type Message = ProvisionerMessage;

	fn intercept_incoming(
		&self,
		_: &mut Sender,
		mut msg: FromOrchestra<Self::Message>,
	) -> Option<FromOrchestra<Self::Message>> {
		if let FromOrchestra::Communication {
			msg: ProvisionerMessage::RequestInherentData(_, response_sender),
		} = &mut msg
		{
			let fault = self.fault.clone();
			let (tx, rx) = oneshot::channel();
			let response = std::mem::replace(response_sender, tx);
			self.spawner.spawn(
				"malus-omit-backed-candidates",
				Some("malus"),
				Box::pin(async move {
					if let Ok(mut data) = rx.await {
						if fault.gate.with_extension("fork").exists() {
							fault.filter_backed_candidates(&mut data.backed_candidates);
						}
						let _ = response.send(data);
					}
				}),
			);
		}
		Some(msg)
	}
}

#[derive(Clone)]
struct NoteCandidate<Spawner> {
	fault: Fault,
	spawner: Spawner,
}

impl<Sender, Spawner> MessageInterceptor<Sender> for NoteCandidate<Spawner>
where
	Sender: overseer::CandidateValidationSenderTrait + Clone + Send + 'static,
	Spawner: overseer::gen::Spawner + Clone + 'static,
{
	type Message = CandidateValidationMessage;

	fn intercept_incoming(
		&self,
		_: &mut Sender,
		mut msg: FromOrchestra<Self::Message>,
	) -> Option<FromOrchestra<Self::Message>> {
		if let FromOrchestra::Communication {
			msg:
				CandidateValidationMessage::ValidateFromExhaustive {
					candidate_receipt,
					response_sender,
					..
				},
		} = &mut msg
		{
			if candidate_receipt.descriptor.para_id() == self.fault.para {
				let hash = candidate_receipt.hash();
				let fault = self.fault.clone();
				let (tx, rx) = oneshot::channel();
				let response = std::mem::replace(response_sender, tx);
				self.spawner.spawn(
					"malus-note-candidate",
					Some("malus"),
					Box::pin(async move {
						if let Ok(result) = rx.await {
							if let Ok(ValidationResult::Valid(commitments, _)) = &result {
								fault
									.candidates
									.lock()
									.expect("fault state lock")
									.insert(hash, commitments.head_data.clone());
							}
							let _ = response.send(result);
						}
					}),
				);
			}
		}
		Some(msg)
	}
}

impl<Sender> MessageInterceptor<Sender> for Fault
where
	Sender: overseer::AvailabilityStoreSenderTrait + Clone + Send + 'static,
{
	type Message = AvailabilityStoreMessage;

	fn intercept_incoming(
		&self,
		_: &mut Sender,
		msg: FromOrchestra<Self::Message>,
	) -> Option<FromOrchestra<Self::Message>> {
		match msg {
			FromOrchestra::Communication { msg } => {
				self.filter(msg).map(|msg| FromOrchestra::Communication { msg })
			},
			msg => Some(msg),
		}
	}
}

impl OverseerGen for WithholdAvailability {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<'_, Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<(Overseer<SpawnGlue<Spawner>, Arc<RuntimeClient>>, OverseerHandle), Error>
	where
		RuntimeClient: RuntimeApiSubsystemClient + ChainApiBackend + AuxStore + 'static,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		let fault =
			Fault { para: self.para, gate: self.gate.clone(), candidates: Default::default() };
		let note = NoteCandidate { fault: fault.clone(), spawner: SpawnGlue(args.spawner.clone()) };
		let omit =
			OmitBackedCandidates { fault: fault.clone(), spawner: SpawnGlue(args.spawner.clone()) };
		let builder =
			validator_overseer_builder(args, ext_args.expect("validator overseer arguments"))?
				.replace_candidate_validation(move |subsystem| {
					InterceptedSubsystem::new(subsystem, note)
				})
				.replace_availability_store(move |subsystem| {
					InterceptedSubsystem::new(subsystem, fault)
				});
		if self.omit_backed_candidates {
			builder
				.replace_provisioner(move |subsystem| InterceptedSubsystem::new(subsystem, omit))
				.build_with_connector(connector)
				.map_err(Into::into)
		} else {
			builder.build_with_connector(connector).map_err(Into::into)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::{channel::oneshot, executor::block_on};
	use polkadot_primitives::{
		CandidateCommitments, CandidateDescriptorV2, CommittedCandidateReceiptV2, CoreIndex, Hash,
		ValidatorIndex,
	};

	fn candidate(para: u32, head: u8) -> BackedCandidate {
		let commitments =
			CandidateCommitments { head_data: HeadData(vec![head]), ..Default::default() };
		let descriptor = CandidateDescriptorV2::new(
			para.into(),
			Hash::default(),
			CoreIndex(0),
			0,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
			Hash::zero().into(),
		);
		BackedCandidate::new(
			CommittedCandidateReceiptV2 { descriptor, commitments },
			vec![],
			Default::default(),
			CoreIndex(0),
		)
	}

	#[test]
	fn gated_inherents_omit_new_heads_or_all_when_empty() {
		let gate = std::env::temp_dir().join(format!("malus-inherent-{}", std::process::id()));
		let fault = Fault { para: 2000.into(), gate: gate.clone(), candidates: Default::default() };
		let candidates = vec![candidate(2000, 0), candidate(2000, 1), candidate(2001, 1)];
		std::fs::write(&gate, [0u8]).expect("test gate is writable; qed");
		let mut gated = candidates.clone();
		fault.filter_backed_candidates(&mut gated);
		assert_eq!(gated, vec![candidates[0].clone(), candidates[2].clone()]);

		std::fs::write(&gate, []).expect("test gate is writable; qed");
		let mut all_gated = candidates.clone();
		fault.filter_backed_candidates(&mut all_gated);
		assert_eq!(all_gated, vec![candidates[2].clone()]);

		std::fs::remove_file(&gate).expect("test gate exists; qed");
		let mut released = candidates.clone();
		fault.filter_backed_candidates(&mut released);
		assert_eq!(released, candidates);
	}

	#[test]
	fn availability_is_withheld_only_for_selected_candidates_while_the_gate_exists() {
		let gate = std::env::temp_dir().join(format!("malus-availability-{}", std::process::id()));
		let hash = CandidateHash(Hash::repeat_byte(1));
		let other = CandidateHash(Hash::repeat_byte(2));
		let fault = Fault {
			para: 2000.into(),
			gate: gate.clone(),
			candidates: Arc::new(Mutex::new(BTreeMap::from([
				(hash, HeadData(vec![1])),
				(other, HeadData(vec![0])),
			]))),
		};
		std::fs::write(&gate, [0u8]).expect("test operation succeeds");
		let (tx, rx) = oneshot::channel();
		assert!(fault
			.filter(AvailabilityStoreMessage::QueryChunkAvailability(hash, ValidatorIndex(0), tx))
			.is_none());
		assert!(!block_on(rx).expect("test operation succeeds"));
		let (tx, rx) = oneshot::channel();
		assert!(fault
			.filter(AvailabilityStoreMessage::QueryChunk(hash, ValidatorIndex(0), tx))
			.is_none());
		assert!(block_on(rx).expect("test operation succeeds").is_none());
		let (tx, _) = oneshot::channel();
		assert!(fault.filter(AvailabilityStoreMessage::QueryAvailableData(hash, tx)).is_none());
		let (tx, rx) = oneshot::channel();
		assert!(fault
			.filter(AvailabilityStoreMessage::QueryDataAvailability(hash, tx))
			.is_none());
		assert!(!block_on(rx).expect("fault answers"));
		let (tx, rx) = oneshot::channel();
		assert!(fault.filter(AvailabilityStoreMessage::QueryChunkSize(hash, tx)).is_none());
		assert!(block_on(rx).expect("fault answers").is_none());
		let (tx, rx) = oneshot::channel();
		assert!(fault.filter(AvailabilityStoreMessage::QueryAllChunks(hash, tx)).is_none());
		assert!(block_on(rx).expect("fault answers").is_empty());
		let (tx, _) = oneshot::channel();
		assert!(fault
			.filter(AvailabilityStoreMessage::QueryChunkAvailability(other, ValidatorIndex(0), tx))
			.is_some());
		std::fs::remove_file(&gate).expect("test operation succeeds");
		let (tx, _) = oneshot::channel();
		assert!(fault
			.filter(AvailabilityStoreMessage::QueryChunkAvailability(hash, ValidatorIndex(0), tx))
			.is_some());
	}
}
