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

use core::ops::RangeInclusive;
use cumulus_primitives_spec_messaging::{
	InChannelState, MessagePosition, OutChannelState, RegisterStatus, SessionParams,
};

/// Channel-state rules the pallet applies to [`OutChannelState`].
pub(crate) trait OutChannelPolicy {
	/// Whether we took part in (sent `Hello` for) session `s`.
	fn took_part_in(&self, s: u32) -> bool;

	/// Whether we already sent `CloseChannel` for session `s`.
	fn closed(&self, s: u32) -> bool;

	/// The active session parameters if we may send `Data` right now.
	fn active_session(&self, supported_versions: RangeInclusive<u8>) -> Option<SessionParams>;

	/// Cursor for reading the recipient's register in `consumed_streams()`, while we want the
	/// channel open: the position after the applied register.
	fn register_cursor(&self) -> Option<MessagePosition>;
}

impl OutChannelPolicy for OutChannelState {
	fn took_part_in(&self, s: u32) -> bool {
		self.open.is_some_and(|o| o.session == s)
	}

	fn closed(&self, s: u32) -> bool {
		self.closed_session == Some(s)
	}

	fn active_session(&self, supported_versions: RangeInclusive<u8>) -> Option<SessionParams> {
		let o = self.open?;
		if o.opened && !self.closed(o.session) && supported_versions.contains(&o.params.version) {
			Some(o.params)
		} else {
			None
		}
	}

	fn register_cursor(&self) -> Option<MessagePosition> {
		if !self.want_open {
			return None;
		}
		match self.register {
			None => Some(MessagePosition(0)),
			Some(applied) => applied.position.0.checked_add(1).map(MessagePosition),
		}
	}
}

/// Channel-state rules the pallet applies to [`InChannelState`].
pub(crate) trait InChannelPolicy {
	/// `consumed_streams()` lists the channel when this holds and its delivery queue has room.
	fn is_watched(&self) -> bool;
}

impl InChannelPolicy for InChannelState {
	fn is_watched(&self) -> bool {
		matches!(self.status, RegisterStatus::Accepting { .. } | RegisterStatus::Opened) ||
			self.draining
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use cumulus_primitives_spec_messaging::{AppliedRegister, MmrFrontier, OpenSession, Register};

	#[test]
	fn out_channel_state_helpers_reflect_lifecycle_terms() {
		let params = SessionParams { version: 2, max_message_size: 1024 };
		let mut out = OutChannelState {
			want_open: true,
			session: 1,
			register: None,
			open: Some(OpenSession { session: 1, params, opened: false }),
			closed_session: None,
		};
		assert!(out.took_part_in(1));
		assert!(!out.took_part_in(2));
		assert_eq!(out.active_session(1..=3), None, "not opened yet");
		assert_eq!(out.register_cursor(), Some(MessagePosition(0)));

		out.open.as_mut().unwrap().opened = true;
		assert_eq!(out.active_session(1..=3), Some(params));
		assert_eq!(out.active_session(3..=4), None, "version dropped by upgrade");

		out.closed_session = Some(1);
		assert!(out.closed(1));
		assert_eq!(out.active_session(1..=3), None, "closed by us");

		out.register = Some(AppliedRegister {
			position: MessagePosition(4),
			register: Register { up_to: MessagePosition(0), status: RegisterStatus::Opened },
		});
		assert_eq!(out.register_cursor(), Some(MessagePosition(5)));
		out.want_open = false;
		assert_eq!(out.register_cursor(), None);
	}

	#[test]
	fn in_channel_watching_rule() {
		let mut st = InChannelState {
			frontier: MmrFrontier::new(),
			session: 1,
			status: RegisterStatus::Accepting {
				session: 1,
				min: 1,
				max: 1,
				max_message_size: 1024,
			},
			published: None,
			open: None,
			draining: false,
		};
		assert!(st.is_watched());
		st.status = RegisterStatus::Opened;
		assert!(st.is_watched());
		st.status = RegisterStatus::Closed { session: 1 };
		st.draining = true;
		assert!(st.is_watched());
		st.draining = false;
		assert!(!st.is_watched());
	}
}
