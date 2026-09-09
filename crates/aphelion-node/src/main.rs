//! `aphelion-node` — the Aphelion oracle node.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aphelion_core::{FeedId, Price};
use aphelion_node::api::AppState;
use aphelion_node::chain::{ChainClient, CliChain, MockChain, RpcClient};
use aphelion_node::config::Config;
use aphelion_node::db::Repo;
use aphelion_node::engine::{run_retention, Collector, RoundRunner};
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use aphelion_node::{api, db, sources, telemetry};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "aphelion-node",
    version,
    about = "Aphelion oracle node",
    long_about = "Fetches prices from exchanges, aggregates them, signs the result \
                  and submits it to the Aphelion aggregator contract on Stellar."
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(
        short,
        long,
        env = "APHELION_CONFIG",
        default_value = "aphelion.toml",
        global = true
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the node: collect, aggregate, sign, submit, serve.
    Run {
        /// Compose and sign rounds but never submit them. Useful for a new
        /// operator who wants to watch what their node *would* publish before
        /// bonding stake behind it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Generate a new Ed25519 node key.
    Keygen {
        #[arg(short, long, default_value = "./node-key.json")]
        out: PathBuf,
    },

    /// Print the public key of the configured node key.
    Pubkey,

    /// Fetch every configured source once and print the result.
    ///
    /// The first thing to run when a feed is misbehaving: it needs no
    /// database, no chain access, and no registration.
    CheckSources,

    /// Reproduce the exact bytes and signature for a submission, to compare
    /// against what the contract rejected.
    Sign {
        feed: String,
        /// Decimal price, e.g. 64231.55
        price: String,
        timestamp: u64,
        #[arg(default_value_t = 50)]
        confidence_bps: u32,
        #[arg(default_value_t = 1)]
        nonce: u64,
    },

    /// Apply database migrations and exit.
    Migrate,

    /// Print the effective configuration after env overrides.
    ShowConfig,
}

#[tokio::main]
async fn main() {
    telemetry::init_tracing();

    if let Err(e) = run().await {
        tracing::error!(error = %e, "fatal");
        // Configuration and key problems are the operator's to fix, and the
        // most common reason a first run fails, so they get a pointer rather
        // than just a stack of `Caused by`.
        if matches!(e, NodeError::Config(_) | NodeError::Signing(_)) {
            eprintln!("\nSee docs/node-operator.md for setup instructions.");
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Keygen { out } => {
            let public = NodeSigner::generate(&out)?;
            println!("Generated Ed25519 node key at {}", out.display());
            println!("public key: {}", hex::encode(public.to_bytes()));
            println!(
                "\nRegister it on chain with:\n  \
                 scripts/register-node.sh {}",
                hex::encode(public.to_bytes())
            );
            Ok(())
        }

        Command::Pubkey => {
            let config = Config::load(&cli.config)?;
            let signer = load_signer(&config)?;
            println!("{}", signer.public_key_hex());
            Ok(())
        }

        Command::ShowConfig => {
            let config = Config::load(&cli.config)?;
            println!(
                "{}",
                toml::to_string_pretty(&config).map_err(|e| NodeError::Config(e.to_string()))?
            );
            Ok(())
        }

        Command::Sign {
            feed,
            price,
            timestamp,
            confidence_bps,
            nonce,
        } => {
            let config = Config::load(&cli.config)?;
            let signer = load_signer(&config)?;
            let feed = FeedId::new(feed).map_err(|e| NodeError::Config(e.to_string()))?;
            let price =
                Price::parse_decimal(&price).map_err(|e| NodeError::Config(e.to_string()))?;

            let submission = signer.sign_price(&feed, price, timestamp, confidence_bps, nonce);
            println!("public_key : {}", signer.public_key_hex());
            println!("message    : {}", submission.message.to_hex());
            println!("signature  : {}", submission.signature_hex());
            println!("verifies   : {}", submission.verify(&signer.public_key()));
            Ok(())
        }

        Command::Migrate => {
            let config = Config::load(&cli.config)?;
            db::connect(&config.database).await?;
            println!("migrations applied");
            Ok(())
        }

        Command::CheckSources => {
            let config = Config::load(&cli.config)?;
            let sources = sources::build(&config.sources)?;
            let mut failures = 0;

            for feed_cfg in &config.feeds {
                println!("\n{}", feed_cfg.id);
                for source in &sources {
                    let Some(symbol) = feed_cfg.sources.get(source.name()) else {
                        continue;
                    };
                    match source.fetch(&feed_cfg.id, symbol).await {
                        Ok(q) => println!(
                            "  {:<10} {:>18}  observed {}",
                            source.name(),
                            q.price.to_string(),
                            q.observed_at.to_rfc3339()
                        ),
                        Err(e) => {
                            failures += 1;
                            println!("  {:<10} FAILED: {e}", source.name());
                        }
                    }
                }
            }
            if failures > 0 {
                println!("\n{failures} source(s) failed");
            }
            Ok(())
        }

        Command::Run { dry_run } => serve(cli.config, dry_run).await,
    }
}

fn load_signer(config: &Config) -> Result<NodeSigner> {
    let aggregator_id = contract_id_bytes(&config.network.aggregator_contract)?;
    NodeSigner::load(&config.node.key_path, aggregator_id)
}

/// Derive the raw 32-byte contract id that goes into the signing payload.
///
/// A `C...` address is a 56-character strkey: a version byte, 32 payload
/// bytes and a 2-byte CRC, base32-encoded. Decoding it here rather than
/// pulling in a strkey dependency keeps the node's dependency surface small,
/// and the checksum is verified so a typo in a config file fails loudly
/// instead of producing signatures that nothing will ever accept.
fn contract_id_bytes(address: &str) -> Result<[u8; 32]> {
    const VERSION_BYTE_CONTRACT: u8 = 2 << 3; // 'C'

    let decoded = base32_decode(address)
        .ok_or_else(|| NodeError::Config(format!("`{address}` is not valid base32")))?;

    if decoded.len() != 35 {
        return Err(NodeError::Config(format!(
            "`{address}` decodes to {} bytes, expected 35",
            decoded.len()
        )));
    }
    if decoded[0] != VERSION_BYTE_CONTRACT {
        return Err(NodeError::Config(format!(
            "`{address}` is not a contract address (wrong version byte)"
        )));
    }

    let expected = u16::from_le_bytes([decoded[33], decoded[34]]);
    let actual = crc16_xmodem(&decoded[..33]);
    if expected != actual {
        return Err(NodeError::Config(format!(
            "`{address}` has a bad checksum; check for a transcription error"
        )));
    }

    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded[1..33]);
    Ok(out)
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);

    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

async fn serve(config_path: PathBuf, dry_run: bool) -> Result<()> {
    let config = Arc::new(Config::load(&config_path)?);
    let metrics = if config.api.metrics_enabled {
        Some(telemetry::init_metrics()?)
    } else {
        None
    };

    let signer = Arc::new(load_signer(&config)?);
    tracing::info!(
        node = %config.node.name,
        public_key = %signer.public_key_hex(),
        feeds = config.feeds.len(),
        dry_run,
        "starting aphelion node"
    );

    let pool = db::connect(&config.database).await?;
    let repo = Repo::new(pool);
    let price_sources = sources::build(&config.sources)?;
    let rpc = Arc::new(RpcClient::new(&config.network.rpc_url)?);

    // In dry-run mode the node still needs a chain client for ledger time and
    // for reads, but must not be able to submit — so it gets the mock, seeded
    // from real ledger time, rather than a live client it is trusted not to use.
    let chain: Arc<dyn ChainClient> = if dry_run {
        let live = CliChain::new(config.network.clone())
            .ok()
            .map(|c| async move { c.ledger_time().await.ok() });
        let now = match live {
            Some(fut) => fut.await.unwrap_or_else(now_unix),
            None => now_unix(),
        };
        tracing::warn!("dry run: submissions are disabled");
        Arc::new(MockChain::new(now))
    } else {
        Arc::new(CliChain::new(config.network.clone())?)
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let runner = RoundRunner::new(
        Arc::clone(&config),
        repo.clone(),
        Arc::clone(&signer),
        Arc::clone(&chain),
        dry_run,
    );

    // Startup checks. Neither is fatal: a node that cannot reach RPC at boot
    // should keep collecting and start submitting when RPC returns, rather
    // than crash-loop and lose its observation history.
    if let Err(e) = runner.resync_nonces().await {
        tracing::warn!(error = %e, "nonce resynchronisation failed");
    }
    if let Err(e) = runner.refresh_node_snapshot().await {
        tracing::warn!(error = %e, "could not read this node's registry record");
    }

    let collector = Collector::new(Arc::clone(&config), price_sources, repo.clone());

    let state = AppState {
        config: Arc::clone(&config),
        repo: repo.clone(),
        chain: Arc::clone(&chain),
        rpc,
        signer: Arc::clone(&signer),
        metrics,
        started_at: Instant::now(),
    };

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(collector.run(shutdown_rx.clone()));
    tasks.spawn(runner.run(shutdown_rx.clone()));
    {
        let repo = repo.clone();
        let retention = config.database.retention;
        let rx = shutdown_rx.clone();
        tasks.spawn(async move {
            if let Err(e) = run_retention(repo, retention, rx).await {
                tracing::error!(error = %e, "retention task exited");
            }
        });
    }
    {
        let bind = config.api.bind.clone();
        let rx = shutdown_rx.clone();
        tasks.spawn(async move {
            if let Err(e) = api::serve(state, &bind, rx).await {
                tracing::error!(error = %e, "http api exited");
            }
        });
    }

    wait_for_shutdown().await;
    tracing::info!("shutdown signal received; draining");
    let _ = shutdown_tx.send(true);

    // Bounded drain: a stuck task must not hold the process open forever, but
    // an in-flight submission deserves a chance to finish and be recorded.
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(30), drain)
        .await
        .is_err()
    {
        tracing::warn!("some tasks did not stop within 30s; exiting anyway");
    }
    tracing::info!("stopped");
    Ok(())
}

fn now_unix() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_contract_address() {
        // Native XLM SAC on testnet.
        let addr = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
        let id = contract_id_bytes(addr).expect("valid address");
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn rejects_an_account_address() {
        let account = "GCEZWKCA5VLDNRLN3RPRJMRZOX3Z6G5CHCGSNFHEYVXM3XOJMDS674JZ";
        let err = contract_id_bytes(account).unwrap_err().to_string();
        assert!(err.contains("not a contract address"), "{err}");
    }

    #[test]
    fn rejects_a_transcription_error() {
        // Flip one character of a valid address; the CRC must catch it.
        let addr = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSD";
        let err = contract_id_bytes(addr).unwrap_err().to_string();
        assert!(err.contains("checksum"), "{err}");
    }

    #[test]
    fn crc16_matches_the_known_xmodem_vector() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }
}
