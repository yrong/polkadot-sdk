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

//! The requester side of `/spec-msg/fetch/1`: try the source's peers in turn and accept the
//! first response that verifies under the named root. Some refusals are expected, so no peer is
//! penalized.

use crate::{protocol::PROTOCOL_NAME, LOG_TARGET};
use codec::{DecodeLimit, Encode, MaxEncodedLen};
use cumulus_primitives_core::ParaId;
use cumulus_primitives_spec_messaging::{
	verify_channel_messages, verify_messages, FetchError, FetchResponse, MessagePosition,
	MessagesRequest, MessagesResponse, MmrFrontier, Register, StreamId, StreamProof, StreamsRoot,
	VerifyError, MAX_SPECULATIVE_MESSAGE_LEN,
};
use parking_lot::Mutex;
use sc_network::{IfDisconnected, NetworkRequest, PeerId, ProtocolName};
use std::{collections::BTreeMap, sync::Arc};

/// Decoding depth limit for responses. They hold flat vectors; this only blocks crafted nesting.
const MAX_DECODE_DEPTH: u32 = 16;

/// How a request reaches a source's collators.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
	/// The peers that serve `source`, in the order to try them.
	fn peers(&self, source: ParaId) -> Vec<PeerId>;

	/// Send one encoded request to `peer` and return the encoded response.
	async fn request(&self, peer: PeerId, request: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// Rotates the first peer asked each round so a peer whose verified replies lack the needed
/// payload delays only the rounds it leads.
pub(crate) struct Rotating<T: ?Sized> {
	inner: Arc<T>,
	rounds: Mutex<BTreeMap<ParaId, usize>>,
}

impl<T: ?Sized> Rotating<T> {
	pub(crate) fn new(inner: Arc<T>) -> Self {
		Self { inner, rounds: Mutex::default() }
	}

	pub(crate) fn next_round(&self, source: ParaId) {
		*self.rounds.lock().entry(source).or_default() += 1;
	}
}

#[async_trait::async_trait]
impl<T: Transport + ?Sized> Transport for Rotating<T> {
	fn peers(&self, source: ParaId) -> Vec<PeerId> {
		let mut peers = self.inner.peers(source);
		if !peers.is_empty() {
			let round = self.rounds.lock().get(&source).copied().unwrap_or_default();
			let by = round % peers.len();
			peers.rotate_left(by);
		}
		peers
	}

	async fn request(&self, peer: PeerId, request: Vec<u8>) -> Result<Vec<u8>, String> {
		self.inner.request(peer, request).await
	}
}

/// [`Transport`] over the node's network, to a static set of peers per source.
pub struct NetworkTransport {
	network: Arc<dyn NetworkRequest + Send + Sync>,
	peers: BTreeMap<ParaId, Vec<PeerId>>,
}

impl NetworkTransport {
	/// Requests for `source` go to `peers[source]`, in order. The network must know how to reach
	/// them, for example as reserved peers.
	pub fn new(
		network: Arc<dyn NetworkRequest + Send + Sync>,
		peers: BTreeMap<ParaId, Vec<PeerId>>,
	) -> Self {
		Self { network, peers }
	}
}

#[async_trait::async_trait]
impl Transport for NetworkTransport {
	fn peers(&self, source: ParaId) -> Vec<PeerId> {
		self.peers.get(&source).cloned().unwrap_or_default()
	}

	async fn request(&self, peer: PeerId, request: Vec<u8>) -> Result<Vec<u8>, String> {
		self.network
			.request(
				peer,
				ProtocolName::from(PROTOCOL_NAME),
				request,
				None,
				IfDisconnected::TryConnect,
			)
			.await
			.map(|(response, _)| response)
			.map_err(|e| e.to_string())
	}
}

/// Why no response was accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
	/// No peer is known for the source.
	NoPeers,
	/// Every peer failed, sent a response that did not verify, or refused. `refusals` holds the
	/// typed refusals, so the caller can tell a pruned range from an unknown root.
	Unserved {
		/// The peers that answered with a [`FetchError`].
		refusals: Vec<(PeerId, FetchError)>,
	},
	/// A response verified under the named root, but from another frontier than the stored one
	/// at the same leaf count: the stream under that root does not run through the stored
	/// frontier. No peer can serve one that does.
	Diverged,
}

/// Send `request` to `source`'s peers until one response verifies. With `stored` (an inbound
/// channel's local frontier) the response must also start from it; one that verifies on its own
/// from another frontier at that leaf count ends the search with [`RequestError::Diverged`].
pub async fn fetch<T: Transport + ?Sized>(
	transport: &T,
	source: ParaId,
	request: &MessagesRequest,
	stored: Option<&MmrFrontier>,
) -> Result<MessagesResponse, RequestError> {
	let peers = transport.peers(source);
	if peers.is_empty() {
		return Err(RequestError::NoPeers);
	}
	let encoded = request.encode();
	let mut refusals = Vec::new();
	for peer in peers {
		let bytes = match transport.request(peer, encoded.clone()).await {
			Ok(bytes) => bytes,
			Err(e) => {
				log::debug!(target: LOG_TARGET, "Peer {peer} of {source:?} did not serve: {e}");
				continue;
			},
		};
		let mut input = &bytes[..];
		let response = match FetchResponse::decode_with_depth_limit(MAX_DECODE_DEPTH, &mut input) {
			Ok(response) if input.is_empty() => response,
			_ => {
				log::debug!(target: LOG_TARGET, "Peer {peer} of {source:?} sent junk");
				continue;
			},
		};
		let response = match response {
			Ok(response) => response,
			Err(e) => {
				log::debug!(target: LOG_TARGET, "Peer {peer} of {source:?} refused: {e:?}");
				refusals.push((peer, e));
				continue;
			},
		};
		let verified = match stored {
			Some(stored) => verify_channel_messages(request, &response, stored),
			None => verify_messages(request, &response),
		};
		match verified {
			Ok(()) => return Ok(response),
			Err(VerifyError::FrontierMismatch)
				if stored.is_some_and(|stored| {
					response.frontier.leaf_count() == stored.leaf_count()
				}) && verify_messages(request, &response).is_ok() =>
			{
				log::debug!(
					target: LOG_TARGET,
					"Peer {peer} of {source:?} proved {:?} under {:?} does not run through the \
					 stored frontier",
					request.stream,
					request.under,
				);
				return Err(RequestError::Diverged);
			},
			Err(e) => {
				log::debug!(
					target: LOG_TARGET,
					"Response of peer {peer} of {source:?} does not verify: {e:?}",
				);
			},
		}
	}
	Err(RequestError::Unserved { refusals })
}

/// Fetch a register under `under`, following a position-only head reply from its verified frontier.
pub(crate) async fn fetch_register_head<T: Transport + ?Sized>(
	transport: &T,
	source: ParaId,
	stream: StreamId,
	under: StreamsRoot,
) -> Result<(MmrFrontier, Vec<u8>, StreamProof), RequestError> {
	let request = MessagesRequest {
		stream,
		under,
		start: None,
		max_bytes: Register::max_encoded_len() as u32,
	};
	let mut response = fetch(transport, source, &request, None).await?;
	if response.payloads.is_empty() {
		let request = MessagesRequest {
			start: Some(MessagePosition(response.frontier.leaf_count())),
			max_bytes: MAX_SPECULATIVE_MESSAGE_LEN,
			..request
		};
		response = fetch(transport, source, &request, Some(&response.frontier)).await?;
	}
	let [payload] = <[Vec<u8>; 1]>::try_from(response.payloads.into_inner())
		.map_err(|_| RequestError::Unserved { refusals: Vec::new() })?;
	Ok((response.frontier, payload, response.tree_proof))
}
