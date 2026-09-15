//! `replay`: reproduce a published round from the observations that produced it.
//!
//! The reads are here; the judgement is [`aphelion_node::engine::replay`],
//! where it is a pure function of a struct literal — the same split `status`
//! and `duties` use, and for a sharper reason than either. The cases worth
//! testing are a tampered row, a window that retention has eaten, and a genuine
//! divergence. None of those can be produced on demand against a live database,
//! and all three are struct literals.
//!
//! Two shapes of output, for two audiences. The default page is for an operator
//! who has just been told their node is being challenged and wants to know,
//! before anything else, whether they are about to defend a good round or
//! discover a bad one. `--json` is the evidence bundle: the observations, the
//! arithmetic, and the canonical signed payload, in the form a counterparty can
//! check without running this code or trusting it.

use std::io::Write;

use aphelion_node::config::Config;
use aphelion_node::db;
use aphelion_node::engine::replay::{replay, visible_at, Params, Replay, Verdict, Window};
use aphelion_node::engine::AggregationParams;
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;

/// Read the round, rebuild its window, judge it, print it, exit with the grade.
pub async fn run(config: &Config, feed: &str, nonce: u64, json: bool) -> Result<()> {
    let feed = aphelion_core::FeedId::new(feed).map_err(|e| NodeError::Config(e.to_string()))?;
    let feed_cfg = config
        .feed(&feed)
        .ok_or_else(|| NodeError::Config(format!("feed `{feed}` is not configured")))?;

    let aggregator = aphelion_node::strkey::contract_id_bytes(&config.network.aggregator_contract)?;

    // The public key, not the secret. Verifying that the stored signature
    // covers the stored round needs only the public half, and a command whose
    // job is to produce evidence for somebody else should not be the one place
    // that quietly requires the key which signs prices.
    let public_key = NodeSigner::load(&config.node.key_path, aggregator)?.public_key();

    let pool = db::connect(&config.database).await?;
    let repo = db::Repo::new(pool);

    let round = repo.round_by_nonce(&feed, nonce).await?.ok_or_else(|| {
        NodeError::Config(format!(
            "this node has no round for feed `{feed}` at nonce {nonce}. Rounds it composed \
                 are listed by `/v1/rounds`; a nonce it never allocated is not a round it can \
                 be asked about."
        ))
    })?;

    // As of when the round was recorded, which is the closest anchor the
    // schema offers. It is a hair later than the moment the query ran -- the
    // row is inserted after the aggregation -- so the window can in principle
    // include an observation that arrived in between. Sub-second, against a
    // round interval measured in minutes, and erring towards including a row
    // rather than towards inventing a discrepancy.
    let as_of = round.created_at;
    let max_age = config.engine.max_observation_age;
    let candidates = repo.observations_in_window(&feed, max_age, as_of).await?;
    let observations = visible_at(&candidates, as_of);

    let window = Window {
        cutoff: as_of
            - chrono::Duration::from_std(max_age)
                .map_err(|e| NodeError::Other(anyhow::anyhow!("max_observation_age: {e}")))?,
        as_of,
        max_observation_age_secs: max_age.as_secs(),
        observations: observations.clone(),
    };

    let result = replay(
        &round,
        &observations,
        window,
        Params {
            aggregation: AggregationParams {
                min_sources: config.engine.min_sources_per_feed,
                max_source_deviation_bps: config.engine.max_source_deviation_bps,
            },
            confidence_floor: feed_cfg.confidence_bps,
            aggregator,
        },
        &public_key,
    );

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| NodeError::Other(e.into()))?
        );
    } else {
        render(&result);
    }

    // `process::exit` skips the destructors that would flush a block-buffered
    // stdout, which is exactly what happens when the output is piped rather
    // than shown.
    std::io::stdout().flush().ok();
    std::process::exit(result.verdict.exit_code());
}

fn render(r: &Replay) {
    let rec = &r.recorded;
    println!("Round {} nonce {}", rec.feed, rec.nonce);
    println!("  composed      {}", rec.created_at.to_rfc3339());
    println!(
        "  status        {}{}",
        rec.status,
        rec.tx_hash
            .as_deref()
            .map(|h| format!("  tx {h}"))
            .unwrap_or_default()
    );
    println!();

    println!("Published");
    println!("  price         {}", rec.price);
    println!("  confidence    {} bps", rec.confidence_bps);
    println!(
        "  sources       {}  spread {} bps  stddev {}",
        rec.source_count, rec.spread_bps, rec.stddev
    );
    println!("  signed for    {}", rec.observed_at.to_rfc3339());
    println!();

    println!("Provenance");
    println!("  key           {}", r.provenance.public_key);
    println!("  aggregator    {}", r.provenance.aggregator);
    println!(
        "  signature     {}",
        if r.provenance.verified {
            "verifies over the round as recorded"
        } else {
            "DOES NOT VERIFY"
        }
    );
    println!("  payload       {}", r.provenance.message_hex);
    println!();

    let w = &r.window;
    println!("Window");
    println!(
        "  {} .. {}  ({}s)",
        w.cutoff.to_rfc3339(),
        w.as_of.to_rfc3339(),
        w.max_observation_age_secs
    );
    if w.observations.is_empty() {
        println!("  no observations retained");
    } else {
        for o in &w.observations {
            println!(
                "  {:<12} {:>16}  observed {}  received {}",
                o.source,
                o.price.to_string(),
                o.observed_at.to_rfc3339(),
                o.received_at.to_rfc3339()
            );
        }
    }
    println!();

    match &r.recomputed {
        None => println!("Recomputed\n  nothing to recompute; see below"),
        Some(c) => {
            println!("Recomputed");
            println!(
                "  price         {}{}",
                c.price,
                if c.price_matches {
                    "  (identical)".to_string()
                } else {
                    format!("  ({} bps from published)", c.deviation_from_recorded_bps)
                }
            );
            println!("  confidence    {} bps", c.confidence_bps);
            println!(
                "  sources       {}  spread {} bps  stddev {}",
                c.source_count, c.spread_bps, c.stddev
            );
            for u in &c.used {
                println!(
                    "    used       {:<12} {:>16}  {} bps from median",
                    u.source,
                    u.price.to_string(),
                    u.deviation_bps
                );
            }
            for d in &c.discarded {
                println!(
                    "    dropped    {:<12} {:>16}  {} bps — {}",
                    d.source,
                    d.price.to_string(),
                    d.deviation_bps,
                    d.reason
                );
            }
        }
    }
    println!();

    println!("Verdict: {}", verdict_line(r.verdict));
    for f in &r.findings {
        println!("  [{}] {}", f.verdict, wrap(&f.detail, 4));
    }
}

fn verdict_line(v: Verdict) -> &'static str {
    match v {
        Verdict::Reproduced => "reproduced — the published price follows from the retained inputs",
        Verdict::Diverged => "diverged — the retained inputs produce a different price",
        Verdict::Incomplete => "incomplete — too little of the window survives to answer",
        Verdict::Tampered => "tampered — the stored signature does not cover the stored round",
    }
}

/// Wrap a finding to something readable in a terminal, indented under its tag.
fn wrap(text: &str, indent: usize) -> String {
    const WIDTH: usize = 76;
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = 0usize;
    for word in text.split_whitespace() {
        if line > 0 && line + 1 + word.len() > WIDTH {
            out.push('\n');
            out.push_str(&pad);
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.len();
    }
    out
}
