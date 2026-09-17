//! `status`: the one command to run when something is wrong and you do not yet
//! know what.
//!
//! Five reads, each of which an operator could already do separately, in one
//! page with a verdict on the end. The reads are here; the verdict is
//! [`aphelion_node::engine::status`], where it is a pure function and can be
//! tested against a struct literal rather than against a chain.
//!
//! A sixth section is not a read of anything the operator could have looked at,
//! because the two halves of it live in different places: `database.retention`
//! is in their configuration and the periods it has to outlast are on the
//! ledger. See [`aphelion_node::engine::status::EvidenceWindow`] for why a
//! comparison nobody was making belongs on a page about whether the node is
//! all right.
//!
//! The registry's minimum stake is read for the same reason and is not a
//! section: on its own it says nothing, and what it is for is the two
//! sentences it lets the standing line finish. A jailed node is refused by
//! `release` while it is under-bonded, however long it waits, and an active
//! node under the minimum has already lost the way back it has not needed yet.
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

use aphelion_node::chain::committee::{CliCommittee, CommitteeClient};
use aphelion_node::chain::{ChainClient, CliChain, OnChainNode, RpcClient};
use aphelion_node::config::Config;
use aphelion_node::engine::duty::{Consequence, Watch};
use aphelion_node::engine::status::{
    assess, humanise, stroops, ChainStatus, DutiesStatus, EvidenceWindow, FeedStatus, Registration,
    Report, SourceStatus, Verdict,
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

    // -- the evidence window ------------------------------------------------
    //
    // Reads the two periods and compares them against this operator's own
    // retention setting. It needs the chain for the periods and nothing else:
    // no database, which matters, because the failure it catches is one an
    // operator is most likely to be looking for on a node that will not start.
    let evidence = match (&config.network.slashing_contract, &chain_client) {
        (None, _) => EvidenceWindow::NotConfigured,
        (Some(_), None) => EvidenceWindow::Unavailable {
            because: "no chain client".into(),
        },
        (Some(_), Some(_)) => match read_periods(config).await {
            Ok((voting_period, appeal_period)) => EvidenceWindow::Measured {
                retained: config.database.retention.as_secs(),
                voting_period,
                appeal_period,
            },
            Err(e) => EvidenceWindow::Unavailable {
                because: e.to_string(),
            },
        },
    };

    // -- the stake floor ----------------------------------------------------
    //
    // Read whenever the registry was: it is one more field of the same
    // contract's config, and it is the half of `release` that a jailed
    // operator cannot satisfy by waiting. Left `None` on a failed read rather
    // than defaulted to zero, which would report every bond as sufficient.
    let min_stake = match &chain_client {
        Some(c) => c.min_stake().await.ok(),
        None => None,
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
        evidence,
        min_stake,
        heartbeat_secs: config.engine.heartbeat.as_secs() as i64,
    })
}

/// The two periods that decide how long a dispute can keep asking.
async fn read_periods(config: &Config) -> Result<(u64, u64)> {
    let committee = CliCommittee::new(&config.network)?;
    let params = committee.params().await?;
    Ok((params.voting_period, params.appeal_period))
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

/// The deadline a jailed or exiting node is waiting on, as a suffix.
///
/// On the summary line and not only in the findings, because the line is what
/// an operator reads first and "jailed" without a date is the state they
/// already knew they were in.
fn standing_clock(n: &OnChainNode, r: &Report) -> String {
    let Some(now) = r.chain.ledger_time else {
        return String::new();
    };
    let (deadline, waiting, ready) = match n.status.to_ascii_lowercase().as_str() {
        "jailed" => (n.jailed_until, "release in", "releasable now"),
        "exiting" => (n.unbonding_until, "unlocks in", "withdrawable now"),
        _ => return String::new(),
    };
    match deadline {
        0 => String::new(),
        d if now < d => format!(" · {waiting} {}", humanise(d - now)),
        _ => format!(" · {ready}"),
    }
}

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
            "registry   : {} · {} bps · reputation {} · stake {}{}",
            n.status,
            n.weight_bps,
            n.reputation,
            stroops(n.stake),
            standing_clock(n, r)
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

    // Printed whether or not it is a finding, because the number an operator
    // wants on a good day is "how far back can I still defend", and a line
    // that appeared only when the answer was bad would never be read until it
    // was too late to act on.
    match &r.evidence {
        EvidenceWindow::NotConfigured => {}
        EvidenceWindow::Unavailable { .. } => println!("\nevidence   : unread"),
        EvidenceWindow::Measured { retained, .. } => println!(
            "\nevidence   : {} retained · defends rounds up to {} old",
            humanise(*retained),
            r.evidence
                .defensible()
                .map(humanise)
                .unwrap_or_else(|| "?".into()),
        ),
    }

    println!("\n{}", verdict.as_str().to_uppercase());
    for f in findings {
        println!("  [{}] {}", f.verdict.as_str(), f.detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn node(status: &str, jailed_until: u64, unbonding_until: u64) -> OnChainNode {
        OnChainNode {
            public_key_hex: "ab".repeat(32),
            stake: 1_000 * 10_000_000,
            reputation: 2_500,
            status: status.into(),
            weight_bps: 0,
            last_submission: NOW,
            jailed_until,
            unbonding_until,
        }
    }

    fn report(ledger_time: Option<u64>) -> Report {
        Report {
            node_name: "test-node".into(),
            version: "0.1.0".into(),
            public_key: "ab".repeat(32),
            chain: ChainStatus {
                reachable: ledger_time.is_some(),
                ledger_sequence: Some(42),
                ledger_time,
                error: None,
            },
            registration: Registration::Absent,
            feeds: Vec::new(),
            sources: Vec::new(),
            duties: DutiesStatus::NotConfigured,
            evidence: EvidenceWindow::NotConfigured,
            min_stake: Some(500 * 10_000_000),
            heartbeat_secs: 300,
        }
    }

    /// The summary line is what an operator reads first, and "jailed" without a
    /// date is the state they already knew they were in.
    #[test]
    fn the_registry_line_carries_the_deadline_the_state_ends_at() {
        let r = report(Some(NOW));
        assert_eq!(
            standing_clock(&node("Jailed", NOW + 6 * 60 * 60, 0), &r),
            " · release in 6h"
        );
        assert_eq!(
            standing_clock(&node("jailed", NOW - 60, 0), &r),
            " · releasable now"
        );
        assert_eq!(
            standing_clock(&node("Exiting", 0, NOW + 3 * 24 * 60 * 60), &r),
            " · unlocks in 3d"
        );
        assert_eq!(
            standing_clock(&node("exiting", 0, NOW - 1), &r),
            " · withdrawable now"
        );
    }

    /// A state with no deadline, a deadline that did not decode, and a ledger
    /// time that could not be read all print nothing rather than a guess. The
    /// findings say why in each case; the line does not invent one.
    #[test]
    fn nothing_is_printed_where_there_is_no_clock_to_print() {
        let r = report(Some(NOW));
        assert_eq!(standing_clock(&node("Active", 0, 0), &r), "");
        assert_eq!(standing_clock(&node("jailed", 0, 0), &r), "");
        assert_eq!(
            standing_clock(&node("jailed", NOW + 60, 0), &report(None)),
            "",
            "the clock is the ledger's; this machine's is never a substitute"
        );
    }

    /// The line and the findings quote the same amount, so they read as one
    /// number rather than two.
    #[test]
    fn the_line_prints_stake_in_the_units_the_findings_use() {
        assert_eq!(stroops(1_000 * 10_000_000), "1000");
    }
}
