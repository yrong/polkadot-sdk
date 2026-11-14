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

//! Withhold a parachain's new commitments relative to a root in a local control file.

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
use polkadot_primitives::{CandidateHash, Id as ParaId, StreamsRoot};
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
	/// File containing the baseline StreamsRoot that remains available.
	#[arg(long)]
	pub availability_gate: PathBuf,
	#[command(flatten)]
	pub cli: Cli,
}

pub(crate) struct WithholdAvailability {
	pub para: ParaId,
	pub gate: PathBuf,
}

#[derive(Clone)]
struct Fault {
	para: ParaId,
	gate: PathBuf,
	candidates: Arc<Mutex<BTreeMap<CandidateHash, StreamsRoot>>>,
}

impl Fault {
	fn blocked(&self, hash: &CandidateHash) -> bool {
		let Ok(baseline) = std::fs::read(&self.gate) else { return false };
		self.candidates
			.lock()
			.expect("fault state lock")
			.get(hash)
			.is_some_and(|root| baseline.as_slice() != root.0.as_bytes())
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
					"malus-note-provides",
					Some("malus"),
					Box::pin(async move {
						if let Ok(result) = rx.await {
							if let Ok(ValidationResult::Valid(commitments, _)) = &result {
								if let Ok(signals) = commitments.ump_signals() {
									if let Some(root) = signals.provides() {
										fault
											.candidates
											.lock()
											.expect("fault state lock")
											.insert(hash, *root);
									}
								}
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
		validator_overseer_builder(args, ext_args.expect("validator overseer arguments"))?
			.replace_candidate_validation(move |subsystem| {
				InterceptedSubsystem::new(subsystem, note)
			})
			.replace_availability_store(move |subsystem| {
				InterceptedSubsystem::new(subsystem, fault)
			})
			.build_with_connector(connector)
			.map_err(Into::into)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::{channel::oneshot, executor::block_on};
	use polkadot_primitives::{Hash, ValidatorIndex};

	#[test]
	fn availability_is_withheld_only_for_selected_candidates_while_the_gate_exists() {
		let gate = std::env::temp_dir().join(format!("malus-availability-{}", std::process::id()));
		let hash = CandidateHash(Hash::repeat_byte(1));
		let other = CandidateHash(Hash::repeat_byte(2));
		let fault = Fault {
			para: 2000.into(),
			gate: gate.clone(),
			candidates: Arc::new(Mutex::new(BTreeMap::from([
				(hash, StreamsRoot(Hash::repeat_byte(1))),
				(other, StreamsRoot(Hash::zero())),
			]))),
		};
		std::fs::write(&gate, Hash::zero().as_bytes()).expect("test operation succeeds");
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
