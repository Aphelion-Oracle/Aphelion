//! Several node processes, one deployment.
//!
//! These are the properties that only exist once the nodes are separate
//! operating-system processes with separate databases. Anything provable with
//! several signers inside one process belongs in `aphelion-node`'s
//! `multi_node` suite instead, which is far faster; what is here is here
//! because a process boundary is load-bearing to the question.
//!
//! Every test skips, loudly, when there is no Postgres to build against. See
//! the crate documentation for the environment it needs.

use std::time::Duration;

use aphelion_core::{FeedId, Price};
use aphelion_harness::{harness_or_skip, Harness, Options};

fn feed() -> FeedId {
    FeedId::new("BTC_USD").unwrap()
}

/// Generous on purpose. These are real processes polling a real database on a
/// machine that is also compiling Rust; a tight bound would fail for load
/// rather than for a defect, and a harness that cries wolf gets switched off.
const PATIENCE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn several_node_processes_agree_on_one_price() {
    // The whole point of the harness in one test: three separate binaries,
    // three databases, three keys, three sets of subprocess calls to the
    // chain, and one number at the end of it.
    let mut h = harness_or_skip!(Harness::start(3));

    let published = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("no round published.\n{}", h.logs()));

    assert_eq!(
        published.num_nodes,
        3,
        "every node should have contributed.\n{}",
        h.logs()
    );

    // The venues quote 64231.55 with a basis point either side, so the median
    // of three honest nodes is that price back again. Exactness is not the
    // claim -- the mid of a two-sided book is -- so this allows a few bps.
    let expected = Price::parse_decimal("64231.55").unwrap();
    let drift = aphelion_core::deviation_bps(published.price.raw(), expected.raw());
    assert!(
        drift <= 5,
        "published {} against an expected {expected} ({drift} bps)",
        published.price
    );

    h.shutdown().await;
}

#[tokio::test]
async fn each_process_keeps_its_own_nonce_sequence() {
    // Nonces are per `(node, feed)` and must strictly increase, and the
    // aggregator rejects one that does not. Three processes allocating them
    // from three separate databases, with no coordination between them, is
    // exactly the situation where a shared or restarted counter would show up
    // as a rejected submission.
    let mut h = harness_or_skip!(Harness::start(3));

    // Wait for enough submissions that every node has been round the loop
    // more than once, otherwise "increasing" is vacuous.
    let accepted = h
        .until(PATIENCE, || async {
            let all = h.deployment.accepted();
            (all.len() >= 6).then_some(all)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "only {} submissions accepted.\n{}",
                h.deployment.accepted().len(),
                h.logs()
            )
        });

    let mut by_node: std::collections::HashMap<String, Vec<u64>> = Default::default();
    for (pubkey, submission) in accepted {
        by_node
            .entry(pubkey)
            .or_default()
            .push(submission.message.nonce);
    }

    assert_eq!(by_node.len(), 3, "all three nodes should have submitted");
    for (pubkey, nonces) in by_node {
        let mut sorted = nonces.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            nonces.len(),
            "node {pubkey} reused a nonce: {nonces:?}"
        );
        assert_eq!(
            nonces, sorted,
            "node {pubkey} submitted nonces out of order: {nonces:?}"
        );
    }

    h.shutdown().await;
}

#[tokio::test]
async fn a_node_the_registry_does_not_know_is_refused_while_the_others_publish() {
    // The unregistered node is not broken and not lying. It runs the same
    // binary, reads the same exchanges and signs correctly; the registry has
    // simply never heard of its key. It must be refused, and its refusal must
    // not stop the network -- which is the part only separate processes show,
    // because an in-process test cannot have one node's rejection block
    // another's submission.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 3,
        quorum: 2,
        unregistered: vec![2],
        ..Default::default()
    }));

    let published = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("the registered nodes never published.\n{}", h.logs()));
    assert_eq!(published.num_nodes, 2, "only the registered nodes count");

    let stranger = h.nodes[2].public_key_hex.clone();

    // And it failed for the right reason, rather than by never getting far
    // enough to try. Waited for rather than read once: quorum is two, so the
    // registered pair can close a round before the third node has attempted
    // its first submission, and asserting immediately would be asserting on
    // which process got scheduled first.
    let refused = h
        .until(PATIENCE, || async {
            h.nodes[2]
                .logs()
                .contains("NotAuthorizedNode")
                .then_some(())
        })
        .await;
    assert!(
        refused.is_some(),
        "expected the unregistered node to be refused by the registry.\n{}",
        h.nodes[2].logs()
    );

    // The safety property holds throughout, not just at the end: no submission
    // from the stranger was ever accepted.
    assert!(
        h.deployment
            .accepted()
            .iter()
            .all(|(pubkey, _)| *pubkey != *stranger),
        "the chain accepted a submission from an unregistered key"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn a_node_that_dies_does_not_take_the_network_with_it() {
    // Four nodes, quorum two. Kill one outright -- no shutdown, no
    // deregistration, the way a machine actually disappears -- and the rest
    // must carry on publishing.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 4,
        quorum: 2,
        ..Default::default()
    }));

    let before = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the kill.\n{}", h.logs()));

    let victim = h.nodes[0].public_key_hex.clone();
    h.nodes[0].kill().await;

    // A later round, published after the kill, with no contribution from the
    // node that died.
    let after = h
        .until(PATIENCE, || async {
            h.deployment
                .published(&feed())
                .await
                .filter(|p| p.round_id > before.round_id)
        })
        .await
        .unwrap_or_else(|| panic!("the survivors stopped publishing.\n{}", h.logs()));

    assert!(after.round_id > before.round_id);

    // Nothing from the dead node can have landed in the rounds that followed.
    let submissions_after_kill = h.deployment.accepted();
    let last_from_victim = submissions_after_kill
        .iter()
        .rposition(|(pubkey, _)| *pubkey == victim);
    if let Some(idx) = last_from_victim {
        assert!(
            idx < submissions_after_kill.len() - 1,
            "the killed node submitted after it was killed"
        );
    }

    h.shutdown().await;
}

#[tokio::test]
async fn a_node_that_loses_the_chain_keeps_collecting_and_catches_up() {
    // The failure the README names and nothing else covers: the endpoint goes
    // away underneath a node that is otherwise perfectly healthy. It must not
    // crash, must not stop collecting, and must resume publishing when the
    // endpoint returns -- rather than needing a restart, which is what an
    // operator would otherwise be paged to do.
    let mut h = harness_or_skip!(Harness::start(2));

    let before = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the outage.\n{}", h.logs()));

    h.deployment.set_offline(true);
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Still alive, still serving its own API, having spent several rounds
    // unable to reach the chain.
    for node in &h.nodes {
        let health = aphelion_harness::http_get(&format!("{}/health", node.api())).await;
        assert!(
            health.is_some(),
            "node `{}` stopped serving during the outage.\n{}",
            node.name,
            node.logs()
        );
    }

    h.deployment.set_offline(false);

    let after = h
        .until(PATIENCE, || async {
            h.deployment
                .published(&feed())
                .await
                .filter(|p| p.round_id > before.round_id)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "the network never resumed after the endpoint returned.\n{}",
                h.logs()
            )
        });

    assert!(after.round_id > before.round_id);
    h.shutdown().await;
}

#[tokio::test]
async fn a_clock_too_far_from_the_ledger_stops_a_node_signing() {
    // Clock drift between a node's machine and the ledger, across a real
    // process. The node compares its own `Utc::now()` against the ledger time
    // it reads from the chain, so moving the deployment's clock is drift as
    // the node experiences it. Past `max_clock_skew` it must refuse to sign
    // rather than produce submissions the contract would reject -- and it must
    // recover on its own when the clocks agree again.
    let mut h = harness_or_skip!(Harness::start(2));

    h.await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the drift.\n{}", h.logs()));

    let before = h.deployment.accepted().len();
    h.deployment.set_skew(3_600);

    // Long enough for several rounds to have come and gone.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let during = h.deployment.accepted().len();
    assert_eq!(
        before,
        during,
        "a node signed against a ledger clock an hour away.\n{}",
        h.logs()
    );

    assert!(
        h.nodes
            .iter()
            .any(|n| n.logs().contains("clock is too far from ledger time")),
        "expected the skew to be reported.\n{}",
        h.logs()
    );

    h.deployment.set_skew(0);
    let resumed = h
        .until(PATIENCE, || async {
            (h.deployment.accepted().len() > during).then_some(())
        })
        .await;
    assert!(
        resumed.is_some(),
        "the nodes never resumed once the clocks agreed.\n{}",
        h.logs()
    );

    h.shutdown().await;
}

#[tokio::test]
async fn the_published_price_follows_the_market() {
    // That a round closes says nothing about whether the number in it is
    // current. A node that cached its first observation and never looked at a
    // venue again would satisfy every test above this one -- the rounds keep
    // coming, the nodes keep agreeing, and the price is simply wrong. So move
    // the market underneath the running processes and require the chain to
    // catch up: collector, database, aggregation, signing and the subprocess
    // call to the chain all have to still be turning for that to happen.
    let mut h = harness_or_skip!(Harness::start(2));

    let opening = Price::parse_decimal("64231.55").unwrap();
    let before = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the move.\n{}", h.logs()));
    assert!(
        aphelion_core::deviation_bps(before.price.raw(), opening.raw()) <= 5,
        "expected the opening price first, got {}",
        before.price
    );

    h.exchange.quote_all("70105.25");

    // Waited for by price rather than by round number on purpose: the first
    // round after the move may have been aggregated from observations
    // collected just before it, and failing on that would be failing on which
    // side of a poll the test landed.
    let moved = Price::parse_decimal("70105.25").unwrap();
    let after = h
        .until(PATIENCE, || async {
            h.deployment
                .published(&feed())
                .await
                .filter(|p| aphelion_core::deviation_bps(p.price.raw(), moved.raw()) <= 5)
        })
        .await
        .unwrap_or_else(|| panic!("the network never caught up to the market.\n{}", h.logs()));

    assert!(
        after.round_id > before.round_id,
        "the new price arrived without a new round"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn one_venue_quoting_nonsense_does_not_move_the_network() {
    // A single exchange printing a bad tick is the ordinary failure an oracle
    // exists to absorb, and it is the node's *local* consensus that absorbs
    // it -- the cross-source pass, before anything is signed. Worth proving
    // across the process boundary because here it is the real collector
    // reading a real socket that feeds it, not observations handed to
    // `aggregate` by a test.
    let mut h = harness_or_skip!(Harness::start(2));

    let before = h
        .await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the bad tick.\n{}", h.logs()));

    // Forty percent away, far outside the 1000 bps a source is allowed.
    h.exchange.quote("binance", "90000.00");

    // Waited for so that the rounds judged below are rounds that had the bad
    // tick in front of them; asserting straight away would assert on nothing.
    let noticed = h
        .until(PATIENCE, || async {
            h.nodes
                .iter()
                .all(|n| n.logs().contains("source excluded from round"))
                .then_some(())
        })
        .await;
    assert!(
        noticed.is_some(),
        "no node reported excluding the bad venue.\n{}",
        h.logs()
    );

    let after = h
        .until(PATIENCE, || async {
            h.deployment
                .published(&feed())
                .await
                .filter(|p| p.round_id > before.round_id)
        })
        .await
        .unwrap_or_else(|| panic!("the network stopped publishing.\n{}", h.logs()));

    // Note what this second assertion is and is not. A median of three is
    // already robust to one outlier, so it is not proof that the filter moved
    // the number. It is proof that dropping the liar did not starve the round:
    // three venues less one leaves exactly `min_sources`, and a filter that
    // took one more -- or a node that treated the disagreement as fatal --
    // would show up here as a network that stopped publishing.
    let expected = Price::parse_decimal("64231.55").unwrap();
    let drift = aphelion_core::deviation_bps(after.price.raw(), expected.raw());
    assert!(
        drift <= 5,
        "one venue dragged the published price to {} ({drift} bps off)",
        after.price
    );

    h.shutdown().await;
}

#[tokio::test]
async fn a_node_left_with_one_venue_signs_nothing_and_recovers() {
    // Publishing a price backed by a single exchange is how an oracle
    // launders one venue's outage into consensus, so a node below
    // `min_sources_per_feed` must sign nothing at all -- and must come back on
    // its own when the venues do, rather than needing an operator to restart
    // it.
    //
    // The observation window is compressed here because it is what decides how
    // long a dead venue keeps counting: at the default a node would go on
    // using the last quote it saw for a minute, and the test would spend that
    // minute watching it expire.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 2,
        quorum: 2,
        max_observation_age: Duration::from_secs(4),
        ..Default::default()
    }));

    h.await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the outage.\n{}", h.logs()));

    // Two of the three venues go dark, leaving one -- below the two a node
    // needs before it will sign.
    h.exchange.set_down("binance", true);
    h.exchange.set_down("kraken", true);

    // Long enough for both venues' last observations to age out of the window
    // and for any round already in flight to have finished.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let during = h.deployment.accepted().len();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        during,
        h.deployment.accepted().len(),
        "a node signed a price backed by one venue.\n{}",
        h.logs()
    );

    assert!(
        h.nodes
            .iter()
            .all(|n| n.logs().contains("skipped: no usable data")),
        "expected every node to report having nothing usable to sign.\n{}",
        h.logs()
    );

    // Refusing to sign is not the same as falling over: the processes are
    // still up and still serving, which is what an operator's monitoring sees.
    for node in &h.nodes {
        let health = aphelion_harness::http_get(&format!("{}/health", node.api())).await;
        assert!(
            health.is_some(),
            "node `{}` stopped serving while short of sources.\n{}",
            node.name,
            node.logs()
        );
    }

    h.exchange.set_down("binance", false);
    h.exchange.set_down("kraken", false);

    let resumed = h
        .until(PATIENCE, || async {
            (h.deployment.accepted().len() > during).then_some(())
        })
        .await;
    assert!(
        resumed.is_some(),
        "the nodes never resumed once the venues came back.\n{}",
        h.logs()
    );

    h.shutdown().await;
}

#[tokio::test]
async fn a_live_node_process_sweeps_a_dead_one() {
    // The property that needs two processes and could not be shown with one:
    // the node that charges an absence and the node being charged are separate
    // programs, on separate databases, reaching the chain through separate
    // subprocess calls. Nothing about the charge passes between them — the
    // surviving node reads the registry, decides, and pays.
    //
    // Quorum of one, so each node's own submission closes a round and both are
    // recorded as having taken part before either goes quiet.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 2,
        quorum: 1,
        sweep_absent: true,
        ..Default::default()
    }));

    h.await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published before the node died.\n{}", h.logs()));

    let victim = h.nodes[1].public_key_hex.clone();
    let seen = h
        .until(PATIENCE, || async {
            h.deployment
                .node(&victim)
                .await
                .filter(|n| n.last_submission > 0)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "the node that is about to die never published.\n{}",
                h.logs()
            )
        });

    // It stops. Everything about it on chain stays exactly as it was, which is
    // the problem: full weight, and a vote that would still count.
    h.nodes[1].kill().await;
    assert_eq!(seen.weight_bps, 10_000);

    // Absence thresholds are measured in hours, so the silence is staged rather
    // than waited out. See `Deployment::backdate_submission`.
    let threshold = h.deployment.absence_threshold().await;
    h.deployment
        .backdate_submission(&victim, threshold * 2)
        .await;

    let charged = h
        .until(PATIENCE, || async {
            h.deployment
                .node(&victim)
                .await
                .filter(|n| n.reputation < seen.reputation)
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "the surviving node never charged the dead one for its silence.\n{}",
                h.logs()
            )
        });

    assert_eq!(
        charged.reputation,
        seen.reputation - 25,
        "one silence should cost one missed round, not one per sweep.\n{}",
        h.logs()
    );

    // And the node that did the sweeping did not charge itself along the way.
    let sweeper = h.nodes[0].public_key_hex.clone();
    let sweeper_record = h
        .deployment
        .node(&sweeper)
        .await
        .expect("the surviving node has a record");
    assert!(
        sweeper_record.reputation >= seen.reputation,
        "the sweeping node lost reputation of its own.\n{}",
        h.logs()
    );

    h.shutdown().await;
}

#[tokio::test]
async fn a_node_process_with_upkeep_off_sweeps_nothing() {
    // The default, and the reason it is the default: an operator who has not
    // opted in must not find fees on their account for calls that publish no
    // price. The dead node here is unambiguously sweepable and stays untouched.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 2,
        quorum: 1,
        ..Default::default()
    }));

    h.await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published.\n{}", h.logs()));

    let victim = h.nodes[1].public_key_hex.clone();
    let before = h
        .until(PATIENCE, || async {
            h.deployment
                .node(&victim)
                .await
                .filter(|n| n.last_submission > 0)
        })
        .await
        .unwrap_or_else(|| panic!("the node never published.\n{}", h.logs()));

    h.nodes[1].kill().await;
    let threshold = h.deployment.absence_threshold().await;
    h.deployment
        .backdate_submission(&victim, threshold * 2)
        .await;

    // Comfortably longer than the upkeep interval the harness configures, so
    // this is a loop that did not run rather than one that has not run yet.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let after = h
        .deployment
        .node(&victim)
        .await
        .expect("the dead node still has a record");
    assert_eq!(
        after.reputation,
        before.reputation,
        "a node swept with upkeep disabled.\n{}",
        h.logs()
    );
    assert_eq!(after.weight_bps, before.weight_bps);

    h.shutdown().await;
}

#[tokio::test]
async fn the_sweep_subcommand_reports_before_it_charges() {
    // `aphelion-node sweep` is the hand-operated version of the upkeep loop,
    // and its whole value is that an operator can look before paying. It is run
    // here as a separate process against the same deployment, so what is
    // exercised is the real argument shapes `chain::cli` builds for
    // `list_nodes`, `get_config` and `sweep_absent`.
    let mut h = harness_or_skip!(Harness::with(Options {
        nodes: 2,
        quorum: 1,
        ..Default::default()
    }));

    h.await_published(&feed(), PATIENCE)
        .await
        .unwrap_or_else(|| panic!("nothing published.\n{}", h.logs()));

    let victim = h.nodes[1].public_key_hex.clone();
    let before = h
        .until(PATIENCE, || async {
            h.deployment
                .node(&victim)
                .await
                .filter(|n| n.last_submission > 0)
        })
        .await
        .unwrap_or_else(|| panic!("the node never published.\n{}", h.logs()));

    // Nothing is absent yet, and the command has to say so rather than invent
    // work: a bare `sweep` on a healthy network must not read as a problem.
    let quiet = h.node_command(0, &["sweep"]).await;
    let quiet_out = String::from_utf8_lossy(&quiet.stdout).to_string();
    assert!(quiet.status.success(), "sweep failed: {quiet_out}");
    assert!(
        quiet_out.contains("Nothing to sweep"),
        "expected a healthy network to report nothing to sweep, got:\n{quiet_out}"
    );

    h.nodes[1].kill().await;
    let threshold = h.deployment.absence_threshold().await;
    h.deployment
        .backdate_submission(&victim, threshold * 2)
        .await;

    // Without --commit it names the node and charges nobody. That split is the
    // point of the command existing at all.
    let dry = h.node_command(0, &["sweep"]).await;
    let dry_out = String::from_utf8_lossy(&dry.stdout).to_string();
    assert!(dry.status.success(), "sweep failed: {dry_out}");
    assert!(
        dry_out.contains(&victim) && dry_out.contains("Re-run with --commit"),
        "expected the silent node named and nothing submitted, got:\n{dry_out}"
    );
    assert_eq!(
        h.deployment.node(&victim).await.unwrap().reputation,
        before.reputation,
        "a sweep without --commit charged somebody"
    );

    let committed = h.node_command(0, &["sweep", "--commit"]).await;
    let committed_out = String::from_utf8_lossy(&committed.stdout).to_string();
    assert!(
        committed.status.success(),
        "sweep --commit failed: {committed_out}\n{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert!(
        committed_out.contains("charged 1"),
        "expected one node charged, got:\n{committed_out}"
    );
    assert_eq!(
        h.deployment.node(&victim).await.unwrap().reputation,
        before.reputation - 25,
        "--commit did not charge the missed round"
    );

    h.shutdown().await;
}
