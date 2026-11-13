// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Trusted delivery between emulated Penpal chains, without proof or relay dependency checks.

use cumulus_primitives_spec_messaging::{
	leaf_hash, LossyRead, MessagingInherentData, MmrFrontier, StreamId, INHERENT_IDENTIFIER,
};
use emulated_integration_tests_common::xcm_emulator::AdditionalInherentCode;
use frame_support::{
	dispatch::DispatchResult,
	inherent::{InherentData, ProvideInherent},
	traits::ServiceQueues,
	weights::Weight,
};
use penpal_runtime::{MessageQueue, ParachainInfo, RuntimeCall, RuntimeOrigin, SpecMessaging};
use sp_runtime::traits::Dispatchable;
use std::{cell::RefCell, collections::BTreeMap};

thread_local! {
	static ARCHIVE: RefCell<BTreeMap<(u32, StreamId), Vec<Vec<u8>>>> = RefCell::new(BTreeMap::new());
}

fn frontier(leaves: &[Vec<u8>]) -> MmrFrontier {
	let mut frontier = MmrFrontier::new();
	leaves.iter().for_each(|leaf| frontier.append(leaf_hash(leaf)));
	frontier
}

fn inherent_data() -> MessagingInherentData {
	let us = ParachainInfo::parachain_id();
	let consumed = SpecMessaging::consumed_streams();
	let mut data = MessagingInherentData::default();
	ARCHIVE.with(|archive| {
		let archive = archive.borrow();
		for (channel, stored) in consumed.channels {
			let (sender, stream) = channel.sender_data_stream(us);
			let Some(leaves) = archive.get(&(sender.into(), stream)) else { continue };
			let start = stored.leaf_count() as usize;
			if leaves.len() > start {
				assert_eq!(
					frontier(&leaves[..start]),
					stored,
					"{channel:?} diverged from its sender"
				);
				data.channels.insert(channel, leaves[start..].to_vec());
			}
		}
		for (channel, cursor) in consumed.registers {
			let (recipient, stream) = channel.peer_ack_stream(us);
			let Some(leaves) = archive.get(&(recipient.into(), stream)) else { continue };
			if leaves.len() as u64 > cursor.0 {
				let head = leaves.len() - 1;
				let read = LossyRead {
					frontier: frontier(&leaves[..head]),
					payloads: vec![leaves[head].clone()],
				};
				data.registers.insert(channel, read);
			}
		}
	});
	data
}

pub struct SpecMsgTransport;

impl AdditionalInherentCode for SpecMsgTransport {
	fn on_new_block() -> DispatchResult {
		let data = inherent_data();
		if data.is_empty() {
			return Ok(());
		}
		let mut inherent = InherentData::new();
		inherent.put_data(INHERENT_IDENTIFIER, &data).expect("encodes; qed");
		let Some(call) = <SpecMessaging as ProvideInherent>::create_inherent(&inherent) else {
			return Ok(());
		};
		RuntimeCall::SpecMessaging(call)
			.dispatch(RuntimeOrigin::none())
			.map_err(|e| e.error)?;
		// Match the emulator's HRMP queue servicing.
		let _ = MessageQueue::service_queues(Weight::MAX);
		Ok(())
	}

	fn on_finalize_block() {
		let us: u32 = ParachainInfo::parachain_id().into();
		let sends = SpecMessaging::outbound_messages();
		ARCHIVE.with(|archive| {
			let mut archive = archive.borrow_mut();
			for (stream, leaves) in sends {
				archive.entry((us, stream)).or_default().extend(leaves);
			}
		});
	}
}
