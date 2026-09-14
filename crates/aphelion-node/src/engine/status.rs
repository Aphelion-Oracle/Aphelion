//! One answer to "is my node all right?".
//!
//! Everything here was already knowable. An operator could run `pubkey`, then
//! `check-sources`, then `duties`, then curl `/v1/node` and `/ready`, and
//! assemble the answer themselves — but only while the node is running, only
//! with a database to hand, and only if they knew which of the five to be
//! worried about. The point of this module is the assembly and the verdict,
//! not any new fact.
//!
//! The gathering is in [`crate::cmd::status`]; what lives here is the
//! judgement, so that the interesting part — which combination of facts is
//! merely untidy and which means this node is not doing its job — is a pure
//! function of a struct literal and can be tested without a chain, a database
//! or a network. That is the same split [`super::duty`] uses, for the same
//! reason.
//!
//! A section that cannot be read is reported as unread rather than as empty.
//! "No duties outstanding" and "could not ask" are different facts and only one
//! of them is reassuring; the same goes for every other section, which is why
//! nothing here collapses a failed read into a zero.

use std::fmt;

use aphelion_core::FeedId;
use serde::Serialize;

use crate::chain::OnChainNode;

/// How bad it is, in the three grades an operator actually acts on.
///
/// Ordered worst-first so a list of findings sorts by sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// This node is not doing the job it is staked to do, now.
    Critical,
    /// Working, with something that will become critical if ignored.
    Degraded,
    /// Nothing to do.
    Healthy,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Degraded => "degraded",
            Self::Healthy => "healthy",
        }
    }

    /// Process exit code, so `status` is usable as a health check without
    /// anything having to parse its output.
    ///
    /// Zero only for healthy: a monitor that treats non-zero as failure gets
    /// the right answer without knowing the grades, and one that wants to
    /// distinguish them can look at the code.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Critical => 2,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing worth telling the operator, and how much it matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub verdict: Verdict,
    pub detail: String,
}

impl Finding {
    fn new(verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            verdict,
            detail: detail.into(),
        }
    }
}

/// Whether the RPC endpoint answered, and what it said if it did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChainStatus {
    pub reachable: bool,
    pub ledger_sequence: Option<u32>,
    pub ledger_time: Option<u64>,
    pub error: Option<String>,
}

/// This node's standing in the registry.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Registration {
    /// The registry could not be read. Distinct from `Absent`: an unreachable
    /// registry is not evidence that a node is unregistered, and reporting it
    /// as one would send an operator to fix the wrong thing.
    Unknown {
        because: String,
    },
    /// The registry answered, and does not know this key.
    Absent,
    Present(OnChainNode),
}

/// What one configured feed looks like from here.
#[derive(Debug, Clone, Serialize)]
pub struct FeedStatus {
    pub feed: FeedId,
    /// Venues mapped to a symbol for this feed in the configuration.
    pub configured_sources: usize,
    /// Of those, the ones that answered with a usable quote just now.
    pub live_sources: usize,
    pub required_sources: usize,
    /// Seconds since the aggregator's stored price for this feed, by ledger
    /// time. `None` when the aggregator holds nothing, or could not be read.
    pub on_chain_age: Option<i64>,
    pub on_chain_round: Option<u64>,
}

impl FeedStatus {
    /// Whether a round composed right now would have enough to sign.
    pub fn publishable(&self) -> bool {
        self.live_sources >= self.required_sources
    }
}

/// The result of asking one venue for one feed.
#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus {
    pub source: String,
    pub feed: FeedId,
    pub price: Option<String>,
    pub error: Option<String>,
}

impl SourceStatus {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// What the slashing contract is waiting on, counted by consequence.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DutiesStatus {
    /// No `slashing_contract` in the configuration. Not a problem — this
    /// deployment simply has no disputes to take part in.
    NotConfigured,
    /// Configured, and the read failed.
    Unavailable { because: String },
    Counted {
        costly: usize,
        forfeited: usize,
        owed: usize,
        housekeeping: usize,
    },
}

/// Everything gathered, before anything is concluded from it.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub node_name: String,
    pub version: String,
    pub public_key: String,
    pub chain: ChainStatus,
    pub registration: Registration,
    pub feeds: Vec<FeedStatus>,
    pub sources: Vec<SourceStatus>,
    pub duties: DutiesStatus,
    /// From `engine.heartbeat`, used to decide when a stored price is old.
    pub heartbeat_secs: i64,
}

/// The whole judgement, worst finding first.
///
/// Returns `Healthy` with an empty list when there is nothing to say, which is
/// the only case where an operator does not have to read anything.
pub fn assess(report: &Report) -> (Verdict, Vec<Finding>) {
    let mut findings = Vec::new();

    // -- the chain ----------------------------------------------------------
    //
    // First, and it short-circuits nothing: a node that cannot reach its RPC
    // endpoint still has sources worth probing, and an operator looking at a
    // network problem benefits from knowing the rest of the node is fine.
    if !report.chain.reachable {
        findings.push(Finding::new(
            Verdict::Critical,
            format!(
                "cannot reach the RPC endpoint: {}",
                report
                    .chain
                    .error
                    .as_deref()
                    .unwrap_or("no reason reported")
            ),
        ));
    }

    // -- standing in the registry -------------------------------------------
    match &report.registration {
        Registration::Unknown { because } => findings.push(Finding::new(
            Verdict::Degraded,
            format!("could not read this node's registry record: {because}"),
        )),
        Registration::Absent => findings.push(Finding::new(
            Verdict::Critical,
            "this key is not in the registry; the aggregator will reject anything it signs \
             (see `scripts/register-node.sh`)",
        )),
        Registration::Present(node) => {
            // The registry spells these as a contract enum, which reaches us
            // as `Active` through the CLI and as `active` through the mock.
            // Comparing case-sensitively here would have this silently stop
            // noticing jail on one of the two paths.
            match node.status.to_ascii_lowercase().as_str() {
                "jailed" => findings.push(Finding::new(
                    Verdict::Critical,
                    "jailed: the aggregator refuses every submission from this node until its \
                     jail term is served and `release` is called",
                )),
                "exiting" => findings.push(Finding::new(
                    Verdict::Degraded,
                    "exiting: stake is unbonding, this node no longer votes, and its \
                     submissions no longer count",
                )),
                "active" if node.weight_bps == 0 => findings.push(Finding::new(
                    Verdict::Degraded,
                    "active but carrying no weight: submissions land and change nothing",
                )),
                _ => {}
            }
        }
    }

    // -- feeds --------------------------------------------------------------
    let short: Vec<&FeedStatus> = report.feeds.iter().filter(|f| !f.publishable()).collect();
    if !report.feeds.is_empty() && short.len() == report.feeds.len() {
        // Every feed short at once is a different problem from one feed short:
        // it is almost never four venues failing independently, it is this
        // node's egress.
        findings.push(Finding::new(
            Verdict::Critical,
            format!(
                "no configured feed has enough live sources to publish ({} of {} short); \
                 this is usually one network problem rather than {} venue problems",
                short.len(),
                report.feeds.len(),
                short.len()
            ),
        ));
    } else {
        for f in &short {
            findings.push(Finding::new(
                Verdict::Degraded,
                format!(
                    "{}: {} live source(s), {} required; this feed will not be signed",
                    f.feed, f.live_sources, f.required_sources
                ),
            ));
        }
    }

    // A stored price older than two heartbeats is the network's problem rather
    // than this node's, but it is what a consumer of the feed would see, and
    // an operator should not learn it from the consumer.
    for f in &report.feeds {
        if let Some(age) = f.on_chain_age {
            if age > 2 * report.heartbeat_secs {
                findings.push(Finding::new(
                    Verdict::Degraded,
                    format!(
                        "{}: the aggregator's price is {age}s old, past two heartbeats of \
                         {}s — consumers are reading a stale feed",
                        f.feed, report.heartbeat_secs
                    ),
                ));
            }
        }
    }

    // -- sources ------------------------------------------------------------
    //
    // Reported per venue rather than per probe. One venue failing on every
    // feed is one thing to fix; the per-feed counts above already say what it
    // cost, and repeating it once per feed would bury everything else.
    let failed_probes = report.sources.iter().filter(|s| !s.ok()).count();
    if failed_probes > 0 {
        let mut venues: Vec<&str> = report
            .sources
            .iter()
            .filter(|s| !s.ok())
            .map(|s| s.source.as_str())
            .collect();
        venues.sort_unstable();
        venues.dedup();
        findings.push(Finding::new(
            Verdict::Degraded,
            format!(
                "{failed_probes} of {} source probe(s) failed, across {}",
                report.sources.len(),
                venues.join(", ")
            ),
        ));
    }

    // -- duties -------------------------------------------------------------
    match &report.duties {
        DutiesStatus::NotConfigured => {}
        DutiesStatus::Unavailable { because } => findings.push(Finding::new(
            Verdict::Degraded,
            format!("could not read outstanding duties: {because}"),
        )),
        DutiesStatus::Counted {
            costly,
            forfeited,
            owed,
            housekeeping,
        } => {
            // Costly is critical here even though nothing about the node is
            // broken, because the grade answers "should somebody act now?" and
            // a closing appeal window is the one thing on this page that
            // cannot be done late.
            if *costly > 0 {
                findings.push(Finding::new(
                    Verdict::Critical,
                    format!(
                        "{costly} duty(s) with a deadline that costs stake if missed — run \
                         `aphelion-node duties`"
                    ),
                ));
            }
            if *forfeited > 0 {
                findings.push(Finding::new(
                    Verdict::Degraded,
                    format!(
                        "{forfeited} duty(s) forfeit a say if left — run `aphelion-node duties`"
                    ),
                ));
            }
            if *owed > 0 {
                findings.push(Finding::new(
                    Verdict::Degraded,
                    format!("{owed} settlement(s) owed to this operator are unclaimed"),
                ));
            }
            // Housekeeping is deliberately not a finding. It is work the
            // network needs and anybody may do; charging it to this operator's
            // status page would make a permanently non-green node.
            let _ = housekeeping;
        }
    }

    findings.sort_by_key(|f| f.verdict);
    let verdict = findings
        .first()
        .map(|f| f.verdict)
        .unwrap_or(Verdict::Healthy);
    (verdict, findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(id: &str) -> FeedId {
        FeedId::new(id).unwrap()
    }

    fn healthy_node() -> OnChainNode {
        OnChainNode {
            public_key_hex: "ab".repeat(32),
            stake: 10_000,
            reputation: 8_000,
            status: "active".into(),
            weight_bps: 10_000,
            last_submission: 1_000,
        }
    }

    fn report() -> Report {
        Report {
            node_name: "test-node".into(),
            version: "0.1.0".into(),
            public_key: "ab".repeat(32),
            chain: ChainStatus {
                reachable: true,
                ledger_sequence: Some(42),
                ledger_time: Some(1_000),
                error: None,
            },
            registration: Registration::Present(healthy_node()),
            feeds: vec![FeedStatus {
                feed: feed("BTC_USD"),
                configured_sources: 3,
                live_sources: 3,
                required_sources: 2,
                on_chain_age: Some(30),
                on_chain_round: Some(7),
            }],
            sources: vec![SourceStatus {
                source: "binance".into(),
                feed: feed("BTC_USD"),
                price: Some("64231.55".into()),
                error: None,
            }],
            duties: DutiesStatus::Counted {
                costly: 0,
                forfeited: 0,
                owed: 0,
                housekeeping: 0,
            },
            heartbeat_secs: 300,
        }
    }

    #[test]
    fn a_working_node_says_nothing() {
        let (verdict, findings) = assess(&report());
        assert_eq!(verdict, Verdict::Healthy);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(verdict.exit_code(), 0);
    }

    #[test]
    fn an_unreachable_chain_is_critical() {
        let mut r = report();
        r.chain = ChainStatus {
            reachable: false,
            error: Some("connection refused".into()),
            ..Default::default()
        };
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Critical);
        assert!(findings[0].detail.contains("connection refused"));
    }

    #[test]
    fn an_unregistered_key_is_critical() {
        let mut r = report();
        r.registration = Registration::Absent;
        assert_eq!(assess(&r).0, Verdict::Critical);
    }

    #[test]
    fn an_unreadable_registry_is_not_an_unregistered_node() {
        // The distinction is the point: one sends an operator to register, the
        // other sends them to their RPC endpoint.
        let mut r = report();
        r.registration = Registration::Unknown {
            because: "rpc timeout".into(),
        };
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        assert!(
            findings[0].detail.contains("could not read"),
            "{findings:?}"
        );
    }

    #[test]
    fn jail_is_recognised_whatever_case_the_chain_spells_it_in() {
        for spelling in ["jailed", "Jailed", "JAILED"] {
            let mut r = report();
            let mut node = healthy_node();
            node.status = spelling.into();
            node.weight_bps = 0;
            r.registration = Registration::Present(node);
            let (verdict, findings) = assess(&r);
            assert_eq!(verdict, Verdict::Critical, "{spelling}");
            assert!(findings[0].detail.contains("jailed"), "{spelling}");
        }
    }

    #[test]
    fn exiting_is_degraded_rather_than_critical() {
        let mut r = report();
        let mut node = healthy_node();
        node.status = "Exiting".into();
        r.registration = Registration::Present(node);
        assert_eq!(assess(&r).0, Verdict::Degraded);
    }

    #[test]
    fn an_active_node_with_no_weight_is_worth_saying() {
        let mut r = report();
        let mut node = healthy_node();
        node.weight_bps = 0;
        r.registration = Registration::Present(node);
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        assert!(findings[0].detail.contains("no weight"), "{findings:?}");
    }

    #[test]
    fn one_feed_short_is_degraded_and_names_the_feed() {
        let mut r = report();
        r.feeds.push(FeedStatus {
            feed: feed("ETH_USD"),
            configured_sources: 3,
            live_sources: 1,
            required_sources: 2,
            on_chain_age: Some(10),
            on_chain_round: Some(7),
        });
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        assert!(
            findings.iter().any(|f| f.detail.contains("ETH_USD")),
            "{findings:?}"
        );
    }

    #[test]
    fn every_feed_short_is_read_as_one_problem_not_many() {
        let mut r = report();
        r.feeds[0].live_sources = 0;
        r.feeds.push(FeedStatus {
            feed: feed("ETH_USD"),
            configured_sources: 3,
            live_sources: 0,
            required_sources: 2,
            on_chain_age: None,
            on_chain_round: None,
        });
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Critical);
        let short: Vec<_> = findings
            .iter()
            .filter(|f| f.detail.contains("no configured feed"))
            .collect();
        assert_eq!(
            short.len(),
            1,
            "one finding, not one per feed: {findings:?}"
        );
    }

    #[test]
    fn a_stale_stored_price_is_reported_against_two_heartbeats() {
        let mut r = report();
        r.feeds[0].on_chain_age = Some(601); // heartbeat 300 => limit 600
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        assert!(
            findings.iter().any(|f| f.detail.contains("601s old")),
            "{findings:?}"
        );

        r.feeds[0].on_chain_age = Some(600);
        assert_eq!(
            assess(&r).0,
            Verdict::Healthy,
            "exactly two is not past two"
        );
    }

    #[test]
    fn failing_probes_are_summarised_by_venue() {
        let mut r = report();
        for f in ["BTC_USD", "ETH_USD"] {
            r.sources.push(SourceStatus {
                source: "kraken".into(),
                feed: feed(f),
                price: None,
                error: Some("HTTP 503".into()),
            });
        }
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        let probe: Vec<_> = findings
            .iter()
            .filter(|f| f.detail.contains("source probe"))
            .collect();
        assert_eq!(probe.len(), 1, "{findings:?}");
        assert!(probe[0].detail.contains("2 of 3"), "{:?}", probe[0]);
        assert!(probe[0].detail.contains("kraken"), "{:?}", probe[0]);
    }

    #[test]
    fn a_costly_duty_is_critical_even_though_the_node_is_fine() {
        let mut r = report();
        r.duties = DutiesStatus::Counted {
            costly: 1,
            forfeited: 0,
            owed: 0,
            housekeeping: 0,
        };
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Critical);
        assert!(findings[0].detail.contains("costs stake"), "{findings:?}");
    }

    #[test]
    fn housekeeping_alone_never_colours_the_verdict() {
        // Otherwise every node in the network is permanently amber over work
        // that is nobody's in particular.
        let mut r = report();
        r.duties = DutiesStatus::Counted {
            costly: 0,
            forfeited: 0,
            owed: 0,
            housekeeping: 5,
        };
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Healthy);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_deployment_without_slashing_is_not_degraded_by_saying_so() {
        let mut r = report();
        r.duties = DutiesStatus::NotConfigured;
        assert_eq!(assess(&r).0, Verdict::Healthy);
    }

    #[test]
    fn unreadable_duties_are_not_silence() {
        let mut r = report();
        r.duties = DutiesStatus::Unavailable {
            because: "no operator_account configured".into(),
        };
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Degraded);
        assert!(
            findings[0].detail.contains("operator_account"),
            "{findings:?}"
        );
    }

    #[test]
    fn findings_are_ordered_worst_first() {
        let mut r = report();
        r.registration = Registration::Absent; // critical
        r.feeds[0].on_chain_age = Some(10_000); // degraded
        let (verdict, findings) = assess(&r);
        assert_eq!(verdict, Verdict::Critical);
        assert!(findings.len() >= 2);
        assert!(findings.windows(2).all(|w| w[0].verdict <= w[1].verdict));
        assert_eq!(findings[0].verdict, Verdict::Critical);
    }

    #[test]
    fn exit_codes_separate_the_three_grades() {
        assert_eq!(Verdict::Healthy.exit_code(), 0);
        assert_eq!(Verdict::Degraded.exit_code(), 1);
        assert_eq!(Verdict::Critical.exit_code(), 2);
    }
}
