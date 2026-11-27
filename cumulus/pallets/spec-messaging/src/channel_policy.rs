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

//! Channel-state rules that belong to the pallet, not to the shared primitives.

use cumulus_primitives_spec_messaging::{
	InChannelState, InboundStatus, MessagePosition, OutChannelState, RegisterStatus,
};

pub(crate) trait OutChannelPolicy {
	fn send_size_limit(&self) -> Option<u32>;
	fn register_cursor(&self, sent: impl FnOnce() -> u64) -> Option<MessagePosition>;
}

impl OutChannelPolicy for OutChannelState {
	fn send_size_limit(&self) -> Option<u32> {
		match self.register?.register.status {
			RegisterStatus::Open { max_message_size } if self.want_open => Some(max_message_size),
			_ => None,
		}
	}
	fn register_cursor(&self, sent: impl FnOnce() -> u64) -> Option<MessagePosition> {
		if !self.want_open {
			let register = self.register.map(|r| r.register);
			if register.is_some_and(|r| r.status == RegisterStatus::Closed) ||
				register.map_or(0, |r| r.up_to.0) >= sent()
			{
				return None;
			}
		}
		match self.register {
			None => Some(MessagePosition(0)),
			Some(applied) => applied.position.0.checked_add(1).map(MessagePosition),
		}
	}
}

pub(crate) trait InChannelPolicy {
	fn is_watched(&self) -> bool;
	fn register_status(&self) -> RegisterStatus;
}

impl InChannelPolicy for InChannelState {
	fn is_watched(&self) -> bool {
		matches!(self.status, InboundStatus::Open | InboundStatus::Closing)
	}
	fn register_status(&self) -> RegisterStatus {
		match self.status {
			InboundStatus::Open => RegisterStatus::Open { max_message_size: self.max_message_size },
			InboundStatus::Closing => RegisterStatus::Closing,
			InboundStatus::Closed | InboundStatus::ForceClosed => RegisterStatus::Closed,
		}
	}
}
