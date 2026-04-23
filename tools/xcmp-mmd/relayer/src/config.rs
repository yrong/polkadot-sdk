// Configuration for the XCMP MMD relayer

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Source parachain WebSocket endpoint
    pub source_ws: String,

    /// Destination parachain WebSocket endpoint
    pub dest_ws: String,

    /// Relay chain WebSocket endpoint
    pub relay_ws: String,

    /// Source parachain ID
    pub source_para_id: u32,

    /// Destination parachain ID
    pub dest_para_id: u32,

    /// Signer account seed phrase for submitting extrinsics
    pub signer_seed: String,

    /// How many blocks to look back on startup for missed messages (default: 0)
    #[serde(default)]
    pub lookback_blocks: u32,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path))?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            source_ws: "ws://127.0.0.1:9944".to_string(),
            dest_ws: "ws://127.0.0.1:9955".to_string(),
            relay_ws: "ws://127.0.0.1:9900".to_string(),
            source_para_id: 1000,
            dest_para_id: 2000,
            signer_seed: "//Alice".to_string(),
            lookback_blocks: 0,
        }
    }
}
