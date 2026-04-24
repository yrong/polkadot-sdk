// XCMP MMD Relayer - Main entry point

use anyhow::Result;
use clap::Parser;
use tracing::info;

mod client;
mod config;
mod proof;
mod relayer;
mod signer;
mod types;

use config::Config;
use relayer::Relayer;

#[derive(Parser, Debug)]
#[command(name = "xcmp-mmd-relayer")]
#[command(about = "Off-chain relayer for XCMP MMD POC")]
#[command(long_about = "
Watches a source parachain for outbound XCMP MMD messages, constructs
the nested proof bundle (relay MMR + para-heads + outbox MMR), and
submits MessageWithProof to the destination parachain.

Proof construction flow:
  1. Monitor source parachain for PreRuntime(*b\"xmmd\", ...) digests
  2. Fetch payload bytes from source HrmpOutboundMessages
  3. Generate outbox MMR proof via source runtime API
  4. Generate relay MMR proof via relay chain mmr_generateProof RPC
  5. Reconstruct para-heads Merkle proof from relay state
  6. Submit MessageWithProof to destination submit_xcmp_mmd extrinsic
")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "relayer.toml")]
    config: String,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .init();

    info!("Starting XCMP MMD Relayer");

    let config = Config::load(&cli.config)?;
    info!("Config loaded: source={}, dest={}, relay={}",
        config.source_ws, config.dest_ws, config.relay_ws);

    let relayer = Relayer::new(config).await?;
    relayer.run().await?;

    Ok(())
}
