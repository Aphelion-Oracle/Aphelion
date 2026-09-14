//! `status`: the one command to run when something is wrong and you do not yet
//! know what.
//!
//! Five reads, each of which an operator could already do separately, in one
//! page with a verdict on the end. The reads are here; the verdict is
//! [`aphelion_node::engine::status`], where it is a pure function and can be
//! tested against a struct literal rather than against a chain.
//!
//! Two properties matter more than the contents.
//!
//! **Nothing here needs the node to be running**, and nothing here needs a
//! database. A node that will not start is exactly when this is worth having,
//! and an operator debugging one should not be told to start it first. That
//! rules out the round history, which lives in Postgres — `/v1/rounds` already
//! serves it to anyone whose node is up.
//!
//! **No single failure stops the page.** Every section is read independently
//! and a section that cannot be read says so and leaves the others alone. An
//! unreachable RPC endpoint is the most likely reason to be running this, and
//! it must not be the reason the source probes go unreported.

use std::io::Write;
use std::sync::Arc;

use aphelion_node::chain::committee::CliCommittee;
use aphelion_node::chain::{ChainClient, CliChain, RpcClient};
use aphelion_node::config::Config;
use aphelion_node::engine::duty::{Consequence, Watch};
use aphelion_node::engine::status::{
    assess, ChainStatus, DutiesStatus, FeedStatus, Registration, Report, SourceStatus, Verdict,
};
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use aphelion_node::sources;

/// Gather, judge, print, and exit with the grade.
pub async fn status(config: &Config, json: bool, probe: bool) -> Result<()> {
    let report = gather(config, probe).await?;
    let (verdict, findings) = assess(&report);

    if json {
        let body = serde_json::json!({
            "verdict": verdict,
            "findings": findings,
            "report": report,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body).map_err(|e| NodeError::Other(e.into()))?
        );
    } else {
        render(&report, verdict, &findings);
    }

    // The grade is the exit code so this works as a health check with nothing
    // parsing its output. `process::exit` skips the destructors that would
    // normally flush a block-buffered stdout, which is exactly what happens
    // when the output is piped rather than shown, so the flush is explicit.
    std::io::stdout().flush().ok();
    std::process::exit(verdict.exit_code());
}

/// Read every section, letting each fail on its own.
async fn gather(config: &Config, probe: bool) -> Result<Report> {
    // The one hard requirement: without a key there is no node to report on,
    // and the error already tells an operator to run `keygen`.
    let signer = NodeSigner::load(
        &config.node.key_path,
        aphelion_node::strkey::contract_id_bytes(&config.network.aggregator_contract)?,
    )?;
    let public_key = signer.public_key_hex();

    // -- the chain ----------------------------------------------------------
    //
    // Reachability comes from the RPC endpoint directly, which needs no
    // credentials, so it is answered even when the contract reads below are
    // not. The two failures are genuinely different -- an endpoint that is
    // down, and a machine that has not been given the key to talk to it -- and
    // an operator should not have to guess which one they are looking at.
    let rpc = RpcClient::new(&config.network.rpc_url)?;
    let ledger = rpc.latest_ledger().await;

    // Contract reads go through the Stellar CLI, which resolves the signing
    // secret when it is constructed even though nothing here writes. So an
    // operator who has not exported it -- the common case when a node will not
    // start, which is exactly when this command is worth running -- gets the
    // rest of the page with these sections marked unread, rather than one
    // error and nothing else.
    let (chain_client, chain_unavailable): (Option<Arc<dyn ChainClient>>, Option<String>) =
        match CliChain::new(config.network.clone()) {
            Ok(c) => (Some(Arc::new(c)), None),
            Err(e) => (None, Some(e.to_string())),
        };

    let ledger_time = match &chain_client {
        Some(c) => c.ledger_time().await.ok(),
        None => None,
    };

    let chain = ChainStatus {
        reachable: ledger.is_ok(),
        ledger_sequence: ledger.as_ref().ok().map(|l| l.sequence),
        ledger_time,
        error: ledger.as_ref().err().map(|e| e.to_string()),
    };

    // -- standing -----------------------------------------------------------
    let registration = match &chain_client {
        None => Registration::Unknown {
            because: chain_unavailable
                .clone()
                .unwrap_or_else(|| "no chain client".into()),
        },
        Some(c) => match c.node_info(&public_key).await {
            Ok(Some(node)) => Registration::Present(node),
            Ok(None) => Registration::Absent,
            Err(e) => Registration::Unknown {
                because: e.to_string(),
            },
        },
    };

    // -- sources ------------------------------------------------------------
    //
    // Probing is every feed times every venue that covers it, which is a
    // dozen HTTP calls on a normal deployment. Run together rather than in
    // turn: serialised, a status page on a deployment with one hanging venue
    // takes the source timeout times the number of feeds to answer, which is
    // long enough that an operator stops running it.
    let mut source_results = Vec::new();
    if probe {
        let built = sources::build(&config.sources)?;
        let mut probes = Vec::new();
        for feed_cfg in &config.feeds {
            for source in &built {
                let Some(symbol) = feed_cfg.sources.get(source.name()) else {
                    continue;
                };
                let source = Arc::clone(source);
                let feed = feed_cfg.id.clone();
                let symbol = symbol.clone();
                probes.push(async move {
                    let result = source.fetch(&feed, &symbol).await;
                    SourceStatus {
                        source: source.name().to_string(),
                        feed,
                        price: result.as_ref().ok().map(|q| q.price.to_string()),
                        error: result.err().map(|e| e.to_string()),
                    }
                });
            }
        }
        source_results = futures::future::join_all(probes).await;
    }

    // -- feeds --------------------------------------------------------------
    let mut feeds = Vec::new();
    for feed_cfg in &config.feeds {
        let on_chain = match &chain_client {
            Some(c) => c.latest_price(&feed_cfg.id).await.ok().flatten(),
            None => None,
        };
        // Age against ledger time, not against this machine's clock: the
        // contract's notion of "now" is the only one a staleness claim can
        // honestly be made in.
        let on_chain_age = match (ledger_time, on_chain.as_ref()) {
            (Some(now), Some(p)) => Some(now as i64 - p.timestamp as i64),
            _ => None,
        };
        let live = source_results
            .iter()
            .filter(|s| s.feed == feed_cfg.id && s.ok())
            .count();

        feeds.push(FeedStatus {
            feed: feed_cfg.id.clone(),
            configured_sources: feed_cfg.sources.len(),
            // Without a probe there is nothing observed, so the requirement is
            // reported as met rather than as failed: `--no-probe` must not
            // manufacture a critical verdict out of not having looked.
            live_sources: if probe {
                live
            } else {
                config.engine.min_sources_per_feed
            },
            required_sources: config.engine.min_sources_per_feed,
            on_chain_age,
            on_chain_round: on_chain.as_ref().map(|p| p.round_id),
        });
    }

    // -- duties -------------------------------------------------------------
    let duties = match (&config.network.slashing_contract, &chain_client) {
        (None, _) => DutiesStatus::NotConfigured,
        (Some(_), None) => DutiesStatus::Unavailable {
            because: chain_unavailable.unwrap_or_else(|| "no chain client".into()),
        },
        (Some(_), Some(chain)) => match read_duties(config, chain, &public_key).await {
            Ok(counted) => counted,
            Err(e) => DutiesStatus::Unavailable {
                because: e.to_string(),
            },
        },
    };

    Ok(Report {
        node_name: config.node.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        public_key,
        chain,
        registration,
        feeds,
        sources: source_results,
        duties,
        heartbeat_secs: config.engine.heartbeat.as_secs() as i64,
    })
}

async fn read_duties(
    config: &Config,
    chain: &Arc<dyn ChainClient>,
    public_key: &str,
) -> Result<DutiesStatus> {
    let committee = Arc::new(CliCommittee::new(&config.network)?);
    let watch = Watch::new(Arc::clone(chain), committee, public_key.to_string());
    let (_snapshot, duties) = watch.duties().await?;

    let count = |c: Consequence| duties.iter().filter(|d| d.consequence == c).count();
    Ok(DutiesStatus::Counted {
        costly: count(Consequence::Costly),
        forfeited: count(Consequence::Forfeited),
        owed: count(Consequence::Owed),
        housekeeping: count(Consequence::Housekeeping),
    })
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

fn render(r: &Report, verdict: Verdict, findings: &[aphelion_node::engine::status::Finding]) {
    println!("{} {}", r.node_name, r.version);
    println!("key        : {}", r.public_key);

    match (r.chain.reachable, r.chain.ledger_sequence) {
        (true, Some(seq)) => println!("ledger     : {seq}"),
        (true, None) => println!("ledger     : reachable"),
        (false, _) => println!("ledger     : UNREACHABLE"),
    }

    match &r.registration {
        Registration::Present(n) => println!(
            "registry   : {} · {} bps · reputation {} · stake {}",
            n.status, n.weight_bps, n.reputation, n.stake
        ),
        Registration::Absent => println!("registry   : not registered"),
        Registration::Unknown { .. } => println!("registry   : unread"),
    }

    println!("\nfeeds");
    for f in &r.feeds {
        let age = f
            .on_chain_age
            .map(|a| format!("{a}s old"))
            .unwrap_or_else(|| "nothing stored".into());
        println!(
            "  {:<10} {}/{} sources · on chain: {}",
            f.feed.to_string(),
            f.live_sources,
            f.required_sources,
            age
        );
    }

    if r.sources.is_empty() {
        println!("\nsources    : not probed");
    } else {
        let failed = r.sources.iter().filter(|s| !s.ok()).count();
        println!(
            "\nsources    : {} of {} answered",
            r.sources.len() - failed,
            r.sources.len()
        );
        for s in r.sources.iter().filter(|s| !s.ok()) {
            println!(
                "  {:<10} {:<10} {}",
                s.source,
                s.feed.to_string(),
                s.error.as_deref().unwrap_or("failed")
            );
        }
    }

    match &r.duties {
        DutiesStatus::NotConfigured => {}
        DutiesStatus::Unavailable { because } => println!("\nduties     : unread ({because})"),
        DutiesStatus::Counted {
            costly,
            forfeited,
            owed,
            housekeeping,
        } => println!(
            "\nduties     : {costly} costly · {forfeited} forfeited · {owed} owed · \
             {housekeeping} housekeeping"
        ),
    }

    println!("\n{}", verdict.as_str().to_uppercase());
    for f in findings {
        println!("  [{}] {}", f.verdict.as_str(), f.detail);
    }
}
