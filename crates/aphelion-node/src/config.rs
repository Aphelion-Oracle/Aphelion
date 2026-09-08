//! Node configuration.
//!
//! Layered, in increasing order of precedence: defaults in this file, the TOML
//! file passed with `--config`, then environment variables. Secrets are *only*
//! read from the environment — the TOML file names the variable to read rather
//! than carrying the value, so a config file can be committed to a private
//! repo without becoming a credential.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aphelion_core::FeedId;
use serde::{Deserialize, Serialize};

use crate::error::{NodeError, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub node: NodeConfig,
    pub network: NetworkConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default)]
    pub feeds: Vec<FeedConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeConfig {
    /// Free-form operator label, attached to logs and metrics.
    pub name: String,
    /// Path to the Ed25519 signing key (see `aphelion-node keygen`).
    pub key_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfig {
    /// Soroban RPC endpoint, used for reads and simulation.
    pub rpc_url: String,
    /// e.g. "Test SDF Network ; September 2015".
    pub network_passphrase: String,
    /// Network name understood by the `stellar` CLI (`testnet`, `mainnet`, ...).
    #[serde(default = "default_network_name")]
    pub network: String,
    /// Contract id (C...) of the deployed registry.
    pub registry_contract: String,
    /// Contract id (C...) of the deployed aggregator.
    pub aggregator_contract: String,
    /// Name of the environment variable holding the Stellar secret seed (S...)
    /// used to pay for submission transactions. Never the seed itself.
    #[serde(default = "default_secret_env")]
    pub submitter_secret_env: String,
    /// Public account (G...) that pays for submissions.
    pub submitter_account: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    /// Name of the environment variable holding the Postgres URL.
    #[serde(default = "default_database_url_env")]
    pub url_env: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// Observations older than this are pruned by the retention job.
    #[serde(with = "humantime_serde", default = "default_retention")]
    pub retention: Duration,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    pub bind: String,
    /// Serve `/metrics` in Prometheus text format.
    pub metrics_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineConfig {
    /// How often a round is opened. Must match the aggregator's configured
    /// minimum round interval or submissions will be rejected as too frequent.
    #[serde(with = "humantime_serde", default = "default_round_interval")]
    pub round_interval: Duration,
    /// How often each source is polled. Faster than the round interval so a
    /// round always has fresh observations to work with.
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    /// Observations older than this are ignored when a round is composed.
    #[serde(with = "humantime_serde", default = "default_max_observation_age")]
    pub max_observation_age: Duration,
    /// Minimum number of independent sources that must agree before this node
    /// is willing to sign anything at all.
    #[serde(default = "default_min_sources")]
    pub min_sources_per_feed: usize,
    /// A source further than this from the cross-source median is discarded.
    #[serde(default = "default_max_source_deviation_bps")]
    pub max_source_deviation_bps: u32,
    /// Skip submission when the price has moved less than this since the last
    /// one — unless the heartbeat has elapsed. Saves fees on quiet feeds.
    #[serde(default = "default_submit_deviation_bps")]
    pub submit_deviation_bps: u32,
    /// Submit at least this often regardless of price movement.
    #[serde(with = "humantime_serde", default = "default_heartbeat")]
    pub heartbeat: Duration,
    /// Refuse to submit if the node's own clock is further than this from the
    /// ledger clock; a skewed clock produces submissions the contract rejects
    /// as stale or future-dated.
    #[serde(with = "humantime_serde", default = "default_max_clock_skew")]
    pub max_clock_skew: Duration,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourcesConfig {
    #[serde(default = "default_true")]
    pub binance: bool,
    #[serde(default = "default_true")]
    pub kraken: bool,
    #[serde(default = "default_true")]
    pub coinbase: bool,
    #[serde(default)]
    pub coingecko: bool,
    /// Name of the environment variable holding a CoinGecko Pro API key, if any.
    #[serde(default)]
    pub coingecko_key_env: Option<String>,
    #[serde(with = "humantime_serde", default = "default_source_timeout")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedConfig {
    pub id: FeedId,
    /// Confidence half-width published alongside the price, in basis points.
    #[serde(default = "default_confidence_bps")]
    pub confidence_bps: u32,
    /// Per-source symbol mapping, e.g. `binance = "BTCUSDT"`.
    /// A source that has no entry here simply does not cover this feed.
    pub sources: BTreeMap<String, String>,
}

impl Config {
    /// Load from a TOML file and apply environment overrides.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            NodeError::Config(format!("cannot read config `{}`: {e}", path.display()))
        })?;
        let mut cfg: Config = toml::from_str(&raw)
            .map_err(|e| NodeError::Config(format!("invalid config `{}`: {e}", path.display())))?;
        cfg.apply_env_overrides();
        cfg.validate()?;
        Ok(cfg)
    }

    /// A small set of overrides, chosen because they are the ones that differ
    /// between a laptop, CI and a deployed container.
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("APHELION_NODE_NAME") {
            self.node.name = v;
        }
        if let Ok(v) = std::env::var("APHELION_KEY_PATH") {
            self.node.key_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("APHELION_RPC_URL") {
            self.network.rpc_url = v;
        }
        if let Ok(v) = std::env::var("APHELION_REGISTRY_CONTRACT") {
            self.network.registry_contract = v;
        }
        if let Ok(v) = std::env::var("APHELION_AGGREGATOR_CONTRACT") {
            self.network.aggregator_contract = v;
        }
        if let Ok(v) = std::env::var("APHELION_API_BIND") {
            self.api.bind = v;
        }
    }

    fn validate(&self) -> Result<()> {
        if self.feeds.is_empty() {
            return Err(NodeError::Config(
                "no feeds configured; the node would have nothing to publish".into(),
            ));
        }
        if self.engine.min_sources_per_feed == 0 {
            return Err(NodeError::Config(
                "engine.min_sources_per_feed must be at least 1".into(),
            ));
        }
        for feed in &self.feeds {
            if feed.sources.len() < self.engine.min_sources_per_feed {
                return Err(NodeError::Config(format!(
                    "feed `{}` maps {} source(s) but engine.min_sources_per_feed is {}; \
                     this feed could never produce a submission",
                    feed.id,
                    feed.sources.len(),
                    self.engine.min_sources_per_feed
                )));
            }
            for name in feed.sources.keys() {
                if !self.sources.is_enabled(name) {
                    return Err(NodeError::Config(format!(
                        "feed `{}` maps source `{name}`, which is unknown or disabled",
                        feed.id
                    )));
                }
            }
        }
        if self.engine.round_interval < self.engine.poll_interval {
            return Err(NodeError::Config(
                "engine.round_interval must not be shorter than engine.poll_interval".into(),
            ));
        }
        if !self.network.registry_contract.starts_with('C')
            || !self.network.aggregator_contract.starts_with('C')
        {
            return Err(NodeError::Config(
                "registry_contract and aggregator_contract must be contract ids starting with `C`"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Resolve a secret from the environment variable the config names.
    pub fn secret_from_env(var: &str) -> Result<String> {
        std::env::var(var).map_err(|_| {
            NodeError::Config(format!(
                "environment variable `{var}` is not set; it must contain the secret \
                 referenced by the config file"
            ))
        })
    }

    pub fn feed(&self, id: &FeedId) -> Option<&FeedConfig> {
        self.feeds.iter().find(|f| &f.id == id)
    }
}

impl SourcesConfig {
    pub fn is_enabled(&self, name: &str) -> bool {
        match name {
            "binance" => self.binance,
            "kraken" => self.kraken,
            "coinbase" => self.coinbase,
            "coingecko" => self.coingecko,
            _ => false,
        }
    }
}

impl Default for SourcesConfig {
    /// The three exchanges that need no credentials are on by default;
    /// CoinGecko is off because its free tier rate-limits hard enough to be a
    /// liability in a 60-second loop.
    fn default() -> Self {
        Self {
            binance: true,
            kraken: true,
            coinbase: true,
            coingecko: false,
            coingecko_key_env: None,
            timeout: default_source_timeout(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            metrics_enabled: true,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            round_interval: default_round_interval(),
            poll_interval: default_poll_interval(),
            max_observation_age: default_max_observation_age(),
            min_sources_per_feed: default_min_sources(),
            max_source_deviation_bps: default_max_source_deviation_bps(),
            submit_deviation_bps: default_submit_deviation_bps(),
            heartbeat: default_heartbeat(),
            max_clock_skew: default_max_clock_skew(),
        }
    }
}

fn default_true() -> bool { true }
fn default_network_name() -> String { "testnet".into() }
fn default_secret_env() -> String { "APHELION_STELLAR_SECRET".into() }
fn default_database_url_env() -> String { "DATABASE_URL".into() }
fn default_max_connections() -> u32 { 10 }
fn default_retention() -> Duration { Duration::from_secs(60 * 60 * 24 * 30) }
fn default_round_interval() -> Duration { Duration::from_secs(60) }
fn default_poll_interval() -> Duration { Duration::from_secs(10) }
fn default_max_observation_age() -> Duration { Duration::from_secs(120) }
fn default_min_sources() -> usize { 2 }
fn default_max_source_deviation_bps() -> u32 { 1_000 }
fn default_submit_deviation_bps() -> u32 { 25 }
fn default_heartbeat() -> Duration { Duration::from_secs(300) }
fn default_max_clock_skew() -> Duration { Duration::from_secs(30) }
fn default_source_timeout() -> Duration { Duration::from_secs(5) }
fn default_confidence_bps() -> u32 { 50 }

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
[node]
name = "test-node"
key_path = "./node-key.json"

[network]
rpc_url = "https://soroban-testnet.stellar.org"
network_passphrase = "Test SDF Network ; September 2015"
registry_contract = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
aggregator_contract = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
submitter_account = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

[database]

[[feeds]]
id = "BTC_USD"
sources = { binance = "BTCUSDT", kraken = "XBTUSD" }
"#
    }

    fn parse(toml_str: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(toml_str)
            .map_err(|e| NodeError::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn minimal_config_fills_in_defaults() {
        let cfg = parse(minimal_toml()).expect("should parse");
        assert_eq!(cfg.engine.round_interval, Duration::from_secs(60));
        assert_eq!(cfg.api.bind, "0.0.0.0:8080");
        assert_eq!(cfg.feeds[0].confidence_bps, 50);
    }

    #[test]
    fn rejects_a_feed_that_can_never_reach_quorum() {
        let bad = minimal_toml().replace(
            r#"sources = { binance = "BTCUSDT", kraken = "XBTUSD" }"#,
            r#"sources = { binance = "BTCUSDT" }"#,
        );
        let err = parse(&bad).unwrap_err().to_string();
        assert!(err.contains("could never produce a submission"), "{err}");
    }

    #[test]
    fn rejects_a_feed_pointing_at_a_disabled_source() {
        let bad = minimal_toml().replace(
            "[database]",
            "[database]\n\n[sources]\nbinance = false",
        );
        let err = parse(&bad).unwrap_err().to_string();
        assert!(err.contains("unknown or disabled"), "{err}");
    }

    #[test]
    fn rejects_non_contract_addresses() {
        let bad = minimal_toml().replace(
            "aggregator_contract = \"CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"",
            "aggregator_contract = \"GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"",
        );
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_empty_feed_list() {
        let bad = minimal_toml()
            .split("[[feeds]]")
            .next()
            .unwrap()
            .to_string();
        let err = parse(&bad).unwrap_err().to_string();
        assert!(err.contains("no feeds configured"), "{err}");
    }
}
