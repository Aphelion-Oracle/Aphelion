//! `aphelion-node` — the Aphelion oracle node.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aphelion_core::{FeedId, Price};
use aphelion_node::api::AppState;
use aphelion_node::chain::{
    ChainClient, CliChain, CliCommittee, CommitteeClient, ReadOnlyChain, RpcClient,
};
use aphelion_node::config::Config;
use aphelion_node::db::Repo;
use aphelion_node::engine::{run_retention, Collector, RoundRunner, Sweeper, Watch};
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use aphelion_node::strkey::contract_id_bytes;
use aphelion_node::{api, db, sources, telemetry};
use clap::{Parser, Subcommand};

mod cmd;
use cmd::beacon::BeaconCmd;
use cmd::committee::{DisputeCmd, ElectionCmd};

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

    /// Show which registered nodes have gone silent, and optionally charge
    /// them the missed round the aggregator allows anyone to charge.
    ///
    /// Reads only, until `--commit`. Needs no database.
    Sweep {
        /// Submit the sweep instead of only printing it. Costs a transaction
        /// fee, and takes reputation off every node the aggregator agrees is
        /// absent.
        #[arg(long)]
        commit: bool,
    },

    /// Show what the slashing contract is asking of this operator, and by when.
    ///
    /// A dispute against this node, a committee vote outstanding, a ballot not
    /// cast. Reads only — nothing here votes on anybody's behalf. Needs no
    /// database.
    Duties {
        /// Machine-readable output, for an alert rather than an operator.
        #[arg(long)]
        json: bool,
    },

    /// The committee's elections: stand, vote, count.
    Election {
        #[command(subcommand)]
        cmd: ElectionCmd,
    },

    /// The randomness beacon: commit, reveal, and what this node owes it.
    ///
    /// Needs a database: the secret behind a commitment lives there, and a
    /// node that cannot produce it is penalised.
    Beacon {
        #[command(subcommand)]
        cmd: BeaconCmd,
    },

    /// Disputes: file, answer, vote, appeal, settle.
    Dispute {
        #[command(subcommand)]
        cmd: DisputeCmd,
    },

    /// One page: identity, chain, registry standing, feeds, sources and
    /// outstanding duties, with a verdict and an exit code.
    ///
    /// Needs no database and does not need the node to be running, which is
    /// when it is worth the most. Exits 0 healthy, 1 degraded, 2 critical.
    Status {
        /// Machine-readable output, for an alert rather than an operator.
        #[arg(long)]
        json: bool,
        /// Skip the live source probe. Faster, and the only way to get an
        /// answer when this machine cannot reach the exchanges at all.
        #[arg(long)]
        no_probe: bool,
    },

    /// Reproduce a published round from the observations that produced it.
    ///
    /// The command to run when a dispute is filed against this node. It checks
    /// two separate things: that the stored signature really covers the stored
    /// round, and that re-running the aggregation over the retained
    /// observations produces the price that was published.
    ///
    /// Needs a database — the observations are the evidence. Exits 0
    /// reproduced, 1 too little retained to answer, 2 diverged or tampered.
    Replay {
        /// Feed id, e.g. BTC_USD.
        feed: String,
        /// The nonce the round was published under. This is how a challenge
        /// names it; `/v1/rounds` lists what this node has.
        nonce: u64,
        /// The evidence bundle: observations, arithmetic and the canonical
        /// signed payload, for a counterparty rather than an operator.
        #[arg(long)]
        json: bool,
    },

    /// Check an evidence bundle that somebody else produced.
    ///
    /// The counterpart to `replay --json`, for the side judging a dispute
    /// rather than answering one. Treats the bundle as hostile input: its own
    /// verdict is ignored, and the price compared against the observations is
    /// the one inside the signed bytes, never the one the file says is there.
    ///
    /// Give it the allegation as well. `dispute show` prints the accused, the
    /// feed and the nonce; passed here they are checked against the signed
    /// bytes, which is what stops a genuine bundle for a different round being
    /// handed in as an answer. Without them the relevance of the file is left
    /// to the reader, and the output says so.
    ///
    /// Needs nothing — no configuration, no key, no database, no chain. Exits 0
    /// sound, 1 unsupported by its own observations, 2 unrelated, misdescribed
    /// or unsigned — and 64 if the allegation above will not parse, which is
    /// the caller's mistake and deliberately outside the range a script reads
    /// as a judgement on the bundle.
    VerifyEvidence {
        /// The bundle, or `-` for stdin.
        path: PathBuf,
        /// The accused, as the dispute names them: 32 bytes of hex.
        #[arg(long)]
        node: Option<String>,
        /// The feed the allegation is about, e.g. BTC_USD.
        #[arg(long)]
        feed: Option<String>,
        /// The nonce the allegation is about.
        #[arg(long)]
        nonce: Option<u64>,
        /// The deployment the dispute was filed on: the aggregator's `C...`
        /// address, or its 32 bytes in hex. A bundle signed for another
        /// network is sound and irrelevant.
        #[arg(long)]
        aggregator: Option<String>,
        /// SHA-256 of the document the accused put on the record, as
        /// `dispute show` prints it. Checked against this file's bytes rather
        /// than against the signed payload: it answers whether this is the
        /// answer they committed to while the vote was open.
        #[arg(long)]
        digest: Option<String>,
        #[arg(long)]
        json: bool,
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

        Command::Replay { feed, nonce, json } => {
            let config = Config::load(&cli.config)?;
            cmd::replay::run(&config, &feed, nonce, json).await
        }

        // Deliberately does not load a configuration: a committee member
        // judging somebody else's node has none, and this must run for them.
        Command::VerifyEvidence {
            path,
            node,
            feed,
            nonce,
            aggregator,
            digest,
            json,
        } => cmd::verify_evidence::run(
            &path,
            &cmd::verify_evidence::Against {
                node,
                feed,
                nonce,
                aggregator,
                digest,
            },
            json,
        ),

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

        Command::Sweep { commit } => {
            let config = Config::load(&cli.config)?;
            let signer = load_signer(&config)?;
            let chain: Arc<dyn ChainClient> = Arc::new(CliChain::new(config.network.clone())?);

            // The configured interval and batch are respected, but not the
            // on/off switch: running this command *is* the opt-in, and an
            // operator asking to see the plan should not have to enable the
            // loop to be shown it.
            let mut upkeep = config.upkeep.clone();
            upkeep.sweep_absent = true;
            let sweeper = Sweeper::new(chain, signer.public_key_hex(), upkeep);

            let plan = sweeper.plan().await?;
            println!(
                "{} registered node(s); absence threshold {}s",
                plan.examined, plan.absence_threshold
            );
            for (excuse, count) in plan.excuse_counts() {
                println!("  {count:>3} {}", describe_excuse(excuse));
            }

            if plan.is_empty() {
                println!("\nNothing to sweep.");
                return Ok(());
            }

            println!("\n{:<66} {:>10}  {:>6}", "public key", "silent", "weight");
            for c in &plan.candidates {
                println!(
                    "{:<66} {:>9}s  {:>5}bp",
                    c.public_key_hex, c.silent_for, c.weight_bps
                );
            }
            if plan.deferred > 0 {
                println!(
                    "\n{} more beyond the batch limit of {}; run again to reach them.",
                    plan.deferred, config.upkeep.max_batch
                );
            }

            if !commit {
                println!(
                    "\nNothing submitted. Re-run with --commit to charge these \
                     nodes a missed round."
                );
                return Ok(());
            }

            match sweeper.sweep_once().await? {
                Some(report) => {
                    println!(
                        "\noffered {}, charged {}{}",
                        report.plan.candidates.len(),
                        report.charged,
                        report
                            .tx_hash
                            .map(|h| format!(", tx {h}"))
                            .unwrap_or_default()
                    );
                    if report.charged < report.plan.candidates.len() as u32 {
                        println!(
                            "The aggregator declined the rest: it had seen them more recently\n\
                             than the registry showed, or somebody else swept first."
                        );
                    }
                }
                // Between the plan above and the call, somebody else swept.
                None => println!("\nNothing left to sweep."),
            }
            Ok(())
        }

        Command::Status { json, no_probe } => {
            let config = Config::load(&cli.config)?;
            cmd::status::status(&config, json, !no_probe).await
        }

        Command::Duties { json } => {
            let config = Config::load(&cli.config)?;
            cmd::committee::duties(&config, json).await
        }

        Command::Election { cmd } => {
            let config = Config::load(&cli.config)?;
            cmd::committee::election(&config, cmd).await
        }

        Command::Beacon { cmd } => {
            let config = Config::load(&cli.config)?;
            cmd::beacon::run(&config, cmd).await
        }

        Command::Dispute { cmd } => {
            let config = Config::load(&cli.config)?;
            cmd::committee::dispute(&config, cmd).await
        }

        Command::Run { dry_run } => serve(cli.config, dry_run).await,
    }
}

fn describe_excuse(excuse: aphelion_node::engine::Excuse) -> &'static str {
    use aphelion_node::engine::Excuse;
    match excuse {
        Excuse::Own => "this node (never its own business)",
        Excuse::NoWeight => "carry no weight (unknown, jailed or exiting)",
        Excuse::NeverSeen => "have never taken part in a closed round",
        Excuse::Recent => "seen recently enough",
        Excuse::AlreadyOffered => "already offered inside this absence window",
    }
}

fn load_signer(config: &Config) -> Result<NodeSigner> {
    let aggregator_id = contract_id_bytes(&config.network.aggregator_contract)?;
    NodeSigner::load(&config.node.key_path, aggregator_id)
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
    // for reads, but must not be able to submit — so the live client is
    // wrapped in one that refuses writes, rather than replaced by a mock.
    // A mock satisfies the second requirement by breaking the first: its
    // ledger clock stands still while the node's advances, so every dry run
    // eventually reports a clock skew that is not real, and its empty registry
    // reports a registered node as unregistered. Dry run exists to show an
    // operator what their node would do against the deployment they are
    // pointed at, which means the reads have to be that deployment's.
    let chain: Arc<dyn ChainClient> = if dry_run {
        tracing::warn!("dry run: submissions are disabled");
        Arc::new(ReadOnlyChain::new(Arc::new(CliChain::new(
            config.network.clone(),
        )?)))
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

    // Network upkeep. Off unless the operator asked for it, and inert in a dry
    // run — the read-only client refuses the write, so an enabled sweep would
    // log a refusal every interval rather than quietly costing anything.
    //
    // One instance, shared with the API: `/v1/upkeep` has to report the plan
    // this loop would submit, which means it has to be the same loop.
    let sweeper = Arc::new(Sweeper::new(
        Arc::clone(&chain),
        signer.public_key_hex(),
        config.upkeep.clone(),
    ));
    if dry_run && sweeper.enabled() {
        tracing::warn!("dry run: absence sweeps are configured but will not be submitted");
    }

    // Watching the slashing contract. Reads only, and off only when the
    // deployment has not been configured with one: an operator should not have
    // to opt in to being told that a dispute has been filed against them.
    //
    // Enabled in a dry run too, for the same reason the reads are live there:
    // the disputes and elections it reports are the deployment's real ones. It
    // needs no read-only wrapper to be safe there, because the loop calls only
    // `CommitteeClient`'s reads and every one of those is a simulated call the
    // CLI is told not to send.
    let watch = match config.network.slashing_contract {
        Some(_) => match CliCommittee::new(&config.network) {
            Ok(c) => Some(Arc::new(
                Watch::new(
                    Arc::clone(&chain),
                    Arc::new(c) as Arc<dyn CommitteeClient>,
                    signer.public_key_hex(),
                )
                .with_scan_depth(config.committee.scan_depth),
            )),
            // Not fatal. A misconfigured operator account stops this node
            // taking part in the committee; it does not stop it publishing
            // prices, and refusing to start would be a worse trade.
            Err(e) => {
                tracing::warn!(error = %e, "not watching the slashing contract");
                None
            }
        },
        None => {
            tracing::info!(
                "no `slashing_contract` configured; disputes and elections \
                 concerning this node will not be reported"
            );
            None
        }
    };

    let state = AppState {
        config: Arc::clone(&config),
        repo: repo.clone(),
        chain: Arc::clone(&chain),
        rpc,
        signer: Arc::clone(&signer),
        sweeper: Arc::clone(&sweeper),
        watch: watch.clone(),
        metrics,
        started_at: Instant::now(),
    };

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(collector.run(shutdown_rx.clone()));
    tasks.spawn(runner.run(shutdown_rx.clone()));
    tasks.spawn(sweeper.run(shutdown_rx.clone()));
    if let Some(watch) = watch {
        tasks.spawn(watch.run(config.committee.watch_interval, shutdown_rx.clone()));
    }
    // The beacon loop runs whenever a randomness contract is configured, not
    // only when `participate` is on. Participation gates entering new rounds;
    // it does not release this node from a commitment already on the ledger,
    // and the loop is what sends that reveal. An operator who switches
    // participation off mid-round must still not be slashed for it.
    if config.network.randomness_contract.is_some() {
        let config = Arc::clone(&config);
        let mut rx = shutdown_rx.clone();
        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(config.beacon.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Never fatal. A beacon round is worth a log line and
                        // the next tick; it is not worth taking the price feed
                        // down for, and the price feed is what this node is
                        // staked to run.
                        if let Err(e) = cmd::beacon::tick(&config).await {
                            tracing::warn!(error = %e, "beacon tick failed");
                        }
                    }
                    _ = rx.changed() => break,
                }
            }
        });
    }
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
