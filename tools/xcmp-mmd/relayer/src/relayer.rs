// Main relayer event loop

use anyhow::{Context, Result};
use codec::Encode;
use sp_core::H256;
use std::collections::HashSet;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use crate::client::{DestClient, RelayClient, SourceClient};
use crate::config::Config;
use crate::proof::build_message_with_proof;
use crate::signer::ExtrinsicSigner;
use crate::types::{MessageWithProof, PendingMessage};

pub struct Relayer {
    source: SourceClient,
    dest: DestClient,
    relay: RelayClient,
    signer: ExtrinsicSigner,
    config: Config,
}

impl Relayer {
    pub async fn new(config: Config) -> Result<Self> {
        let source = SourceClient::new(&config.source_ws, config.source_para_id).await?;
        let dest = DestClient::new(&config.dest_ws, config.dest_para_id).await?;
        let relay = RelayClient::new(&config.relay_ws).await?;
        let signer = ExtrinsicSigner::new(&config.signer_seed)?;
        info!("Signer public key: 0x{}", hex::encode(signer.public().0));
        Ok(Self { source, dest, relay, signer, config })
    }

    /// Main loop: poll source for new finalized blocks and relay messages
    pub async fn run(&self) -> Result<()> {
        info!("Relayer started. Monitoring source para {} → dest para {}",
            self.config.source_para_id, self.config.dest_para_id);

        let mut last_processed: Option<H256> = None;
        let mut submitted: HashSet<(u32, u64)> = HashSet::new();

        // Lookback: find the starting point
        if self.config.lookback_blocks > 0 {
            info!("Looking back {} blocks on startup", self.config.lookback_blocks);
        }

        loop {
            match self.poll_once(&mut last_processed, &mut submitted).await {
                Ok(n) if n > 0 => info!("Processed {} message(s) this round", n),
                Ok(_) => debug!("No new messages"),
                Err(e) => error!("Poll error: {:#}", e),
            }
            sleep(Duration::from_secs(6)).await;
        }
    }

    /// Single poll iteration. Returns number of messages relayed.
    async fn poll_once(
        &self,
        last_processed: &mut Option<H256>,
        submitted: &mut HashSet<(u32, u64)>,
    ) -> Result<usize> {
        let head = self.source.inner.finalized_head().await?;

        // Skip if nothing changed
        if *last_processed == Some(head) {
            return Ok(0);
        }

        debug!("New finalized head: {:?}", head);

        // Discover pending messages at this block
        let messages = self.discover_messages(head).await?;
        if messages.is_empty() {
            *last_processed = Some(head);
            return Ok(0);
        }

        let mut relayed = 0;
        for msg in messages {
            let key = (msg.source_para_id, msg.mmr_leaf_index);
            if submitted.contains(&key) {
                debug!("Already submitted leaf_index={}", msg.mmr_leaf_index);
                continue;
            }

            match self.relay_message(&msg).await {
                Ok(tx_hash) => {
                    info!(
                        "Relayed message leaf_index={} → tx {:?}",
                        msg.mmr_leaf_index, tx_hash
                    );
                    submitted.insert(key);
                    relayed += 1;
                }
                Err(e) => {
                    warn!("Failed to relay leaf_index={}: {:#}", msg.mmr_leaf_index, e);
                }
            }
        }

        *last_processed = Some(head);
        Ok(relayed)
    }

    /// Scan the source block for XCMP MMD messages destined for our dest para
    async fn discover_messages(&self, block_hash: H256) -> Result<Vec<PendingMessage>> {
        let header = self.source.inner.header(block_hash).await?;

        // Check for xmmd digest - if absent, no messages in this block
        let outbox_mmr_root = match self.source.parse_xmmd_digest(&header) {
            Some(root) => root,
            None => return Ok(vec![]),
        };

        debug!("Found xmmd digest root: {:?}", outbox_mmr_root);

        let block_number = self.source.inner.block_number(block_hash).await?;

        // Fetch HRMP outbound messages for our dest para
        let hrmp_messages = self.source.hrmp_outbound_messages(block_hash).await?;

        let mut pending = Vec::new();
        for (recipient, payload) in hrmp_messages {
            if recipient != self.config.dest_para_id {
                continue;
            }

            // Get the MMR leaf count to compute the leaf index for this message.
            // In the outbox MMR, messages are appended in order; the leaf index for
            // a message committed in block N is derived from the MMR leaf count at
            // the *previous* block. For the POC we use (mmr_size - 1) as an approximation.
            let mmr_size = self.source.mmr_leaf_count(block_hash).await
                .unwrap_or(1);
            let mmr_leaf_index = mmr_size.saturating_sub(1);

            info!(
                "Discovered message: dest={} leaf_index={} payload_len={}",
                recipient, mmr_leaf_index, payload.len()
            );

            pending.push(PendingMessage {
                source_para_id: self.config.source_para_id,
                dest_para_id: recipient,
                mmr_leaf_index,
                source_block_hash: block_hash,
                source_block_number: block_number,
                outbox_mmr_root,
                payload,
            });
        }

        Ok(pending)
    }

    /// Build proof and submit to destination chain. Returns tx hash.
    async fn relay_message(&self, message: &PendingMessage) -> Result<H256> {
        let mwp = build_message_with_proof(message, &self.source, &self.relay).await
            .with_context(|| format!("Proof construction failed for leaf_index={}", message.mmr_leaf_index))?;

        self.submit_to_dest(&mwp).await
    }

    /// Build a signed `submit_xcmp_mmd` extrinsic and submit it to the destination chain.
    async fn submit_to_dest(&self, message: &MessageWithProof) -> Result<H256> {
        let hex = self.signer
            .build_signed_extrinsic(message, &self.dest.inner)
            .await
            .with_context(|| "Failed to build signed extrinsic")?;

        self.dest.inner.submit_extrinsic(&hex).await
            .with_context(|| "Failed to submit extrinsic to destination")
    }
}
