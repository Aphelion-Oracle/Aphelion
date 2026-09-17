//! The round loop: decide, sign, submit.
//!
//! One pass per feed per round interval:
//!
//! 1. Check the node's clock against ledger time. A skewed clock produces
//!    submissions the contract rejects outright, and the node would burn fees
//!    discovering that once a minute.
//! 2. Read the freshest observation from each source.
//! 3. Aggregate (see [`super::aggregate`]).
//! 4. Decide whether the result is worth a transaction fee.
//! 5. Allocate a nonce, sign, submit, record the outcome.
//!
//! Step 4 is the one that is easy to get wrong in the expensive direction.
//! Submitting on every round on a quiet feed spends real money to republish a
//! number nobody's position depends on; submitting only on movement leaves
//! consumers unable to distinguish "unchanged" from "this node is dead". The
//! heartbeat resolves it: publish on meaningful movement, and at least once
//! per heartbeat regardless.
//!
//! There is a second question hiding inside step 4, and the answer to it is
//! [`Authority`]: whether a submission from this node would be *counted* at
//! all. `submit_price` reads the submitter's weight and reverts with
//! `NotAuthorizedNode` when it is zero, which is the state of every jailed,
//! exiting and unregistered node — after the transaction has been paid for.
//! A node that stays in one of those states goes on buying that refusal once
//! per feed per round interval, for as long as it lasts.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aphelion_core::{deviation_bps, FeedId};
use chrono::Utc;

use super::aggregate::{aggregate, confidence_bps, AggregationParams};
use crate::chain::{ChainClient, OnChainNode};
use crate::config::Config;
use crate::db::{Repo, RoundStatus};
use crate::error::{NodeError, Result};
use crate::signer::NodeSigner;

/// Why a round did or did not result in a submission. Surfaced in logs and in
/// `/rounds` so an operator can answer "why has my node been quiet?" without
/// reading the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    Submitted {
        nonce: u64,
        tx_hash: Option<String>,
    },
    /// Price had not moved enough and the heartbeat had not elapsed.
    SkippedUnchanged {
        deviation_bps: u32,
    },
    /// Not enough usable sources; nothing was signed.
    SkippedNoData(String),
    /// `--dry-run`: signed and recorded, deliberately not submitted.
    DryRun {
        nonce: u64,
    },
    /// Signed and recorded, and not paid for: the registry gives this node no
    /// voting weight, so the aggregator would have reverted the transaction.
    SkippedRefused {
        nonce: u64,
        reason: String,
    },
    Failed(String),
}

/// Whether the aggregator would count what this node signs.
///
/// Read from the registry rather than inferred from a rejection, because the
/// rejection is the expensive way to find out: `submit_price` reads
/// `weight_of` and reverts on zero, and the fee for the reverted transaction
/// has already been spent by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// Not read yet, or the last read failed.
    ///
    /// **Submissions go ahead.** This is the fail-open case and it is the most
    /// important decision in this module: a submission that is refused costs
    /// one transaction fee, while a round this node stays quiet for is a
    /// missed round that anybody may sweep and charge to its reputation. An
    /// unreadable registry must not be able to turn the cheap failure into the
    /// expensive one, so an unread authority is treated as permission.
    Unknown,
    /// The registry gives this node weight; what it signs will be counted.
    Voting { weight_bps: u32 },
    /// Weight zero. Every submission would revert, after the fee.
    Refused(Refusal),
}

/// Why the registry gives a node no weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The registry has no record of this key.
    Unregistered,
    /// Jailed until the given ledger time. Zero when the record did not carry
    /// one, which reads as unknown rather than as a term already served.
    Jailed { until: u64 },
    /// Unbonding, and no longer voting from the moment `request_unbond`
    /// landed. `until` is when the stake may be withdrawn.
    Exiting { until: u64 },
    /// Active and registered, and carrying no weight regardless. Not a state
    /// the registry produces today — `weight_for` gives every active node at
    /// least half weight — but it is derived from a number on the ledger
    /// rather than from the status string, so it is reported rather than
    /// assumed impossible.
    NoWeight,
}

impl Authority {
    /// Whether to spend a fee on a submission.
    ///
    /// Note which way `Unknown` falls. See the variant.
    pub fn may_submit(&self) -> bool {
        !matches!(self, Self::Refused(_))
    }

    /// The deadline this state ends at, for a node waiting on one.
    pub fn deadline(&self) -> Option<u64> {
        match self {
            Self::Refused(Refusal::Jailed { until })
            | Self::Refused(Refusal::Exiting { until })
                if *until != 0 =>
            {
                Some(*until)
            }
            _ => None,
        }
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("registry unread; submitting anyway"),
            Self::Voting { weight_bps } => write!(f, "voting at {weight_bps} bps"),
            Self::Refused(Refusal::Unregistered) => f.write_str(
                "this key is not in the registry, so the aggregator would refuse every \
                 submission; run `scripts/register-node.sh`",
            ),
            Self::Refused(Refusal::Jailed { .. }) => f.write_str(
                "jailed, so the aggregator would refuse every submission until the term is \
                 served and `release` is called; see `aphelion-node status`",
            ),
            Self::Refused(Refusal::Exiting { .. }) => f.write_str(
                "exiting, so this node no longer votes and the aggregator would refuse every \
                 submission",
            ),
            Self::Refused(Refusal::NoWeight) => f.write_str(
                "registered and carrying no voting weight, so submissions would \
                             be refused",
            ),
        }
    }
}

/// What the registry record means for the next submission.
///
/// A pure function of the record, so the decision that stops a node paying —
/// or, worse, could stop a healthy node speaking — is testable without a
/// chain. `None` is an answered read of a key the registry does not hold, not
/// a read that failed; a failed read never reaches here.
pub fn authority_of(node: Option<&OnChainNode>) -> Authority {
    let Some(node) = node else {
        return Authority::Refused(Refusal::Unregistered);
    };
    if node.weight_bps > 0 {
        return Authority::Voting {
            weight_bps: node.weight_bps,
        };
    }
    // Weight is the thing the aggregator actually reads, so it decides; the
    // status only names the reason. Spelled case-insensitively for the reason
    // `engine::status` spells it that way — the CLI says `Jailed` and the mock
    // says `jailed`, and a match that noticed only one of them would report
    // the right refusal on one path and a shrug on the other.
    Authority::Refused(match node.status.to_ascii_lowercase().as_str() {
        "jailed" => Refusal::Jailed {
            until: node.jailed_until,
        },
        "exiting" => Refusal::Exiting {
            until: node.unbonding_until,
        },
        _ => Refusal::NoWeight,
    })
}

pub struct RoundRunner {
    config: Arc<Config>,
    repo: Repo,
    signer: Arc<NodeSigner>,
    chain: Arc<dyn ChainClient>,
    dry_run: bool,
    /// The registry's last word on whether this node may vote, refreshed once
    /// per tick by [`RoundRunner::refresh_node_snapshot`].
    ///
    /// Cached rather than read per feed, because it is one answer about the
    /// node and not one answer per feed. Never sticky: it is overwritten by
    /// every refresh, so a node released mid-jail resumes on the next tick
    /// without anything having to notice that it was jailed.
    authority: Mutex<Authority>,
}

impl RoundRunner {
    pub fn new(
        config: Arc<Config>,
        repo: Repo,
        signer: Arc<NodeSigner>,
        chain: Arc<dyn ChainClient>,
        dry_run: bool,
    ) -> Self {
        Self {
            config,
            repo,
            signer,
            chain,
            dry_run,
            authority: Mutex::new(Authority::Unknown),
        }
    }

    /// What the last registry read said. Cheap, and never blocks on the chain.
    pub fn authority(&self) -> Authority {
        self.authority.lock().unwrap().clone()
    }

    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.config.engine.round_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => self.run_all_feeds().await,
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("round runner shutting down");
                        return;
                    }
                }
            }
        }
    }

    async fn run_all_feeds(&self) {
        let ledger_time = match self.chain.ledger_time().await {
            Ok(t) => t,
            Err(e) => {
                // Without ledger time there is no safe timestamp to sign, so
                // the whole round is abandoned rather than guessed at.
                tracing::warn!(error = %e, "cannot read ledger time; skipping round");
                metrics::counter!("aphelion_round_errors_total", "kind" => "chain").increment(1);
                return;
            }
        };

        // Before any feed, and after the ledger time it is dated against. One
        // read per tick rather than one per feed: it is a fact about the node,
        // not about a feed, and a failure here leaves the previous answer in
        // place rather than silencing the round.
        if let Err(e) = self.refresh_node_snapshot().await {
            tracing::warn!(
                error = %e,
                "could not read this node's registry record; keeping the last answer"
            );
        }

        let local = Utc::now().timestamp().max(0) as u64;
        let skew = local as i64 - ledger_time as i64;
        metrics::gauge!("aphelion_clock_skew_seconds").set(skew as f64);

        if skew.unsigned_abs() > self.config.engine.max_clock_skew.as_secs() {
            tracing::error!(
                skew_seconds = skew,
                limit_seconds = self.config.engine.max_clock_skew.as_secs(),
                "node clock is too far from ledger time; refusing to sign. Check NTP."
            );
            metrics::counter!("aphelion_round_errors_total", "kind" => "clock_skew").increment(1);
            return;
        }

        for feed_cfg in &self.config.feeds {
            let started = Instant::now();
            let outcome = self.run_feed(&feed_cfg.id, ledger_time).await;
            let elapsed = started.elapsed().as_secs_f64();
            metrics::histogram!("aphelion_round_duration_seconds").record(elapsed);

            let label = match &outcome {
                Ok(RoundOutcome::Submitted { nonce, tx_hash }) => {
                    tracing::info!(feed = %feed_cfg.id, nonce, tx = ?tx_hash, "submitted");
                    "submitted"
                }
                Ok(RoundOutcome::SkippedUnchanged { deviation_bps }) => {
                    tracing::debug!(feed = %feed_cfg.id, deviation_bps, "skipped: price unchanged");
                    "skipped_unchanged"
                }
                Ok(RoundOutcome::SkippedNoData(why)) => {
                    tracing::warn!(feed = %feed_cfg.id, reason = %why, "skipped: no usable data");
                    "skipped_no_data"
                }
                Ok(RoundOutcome::DryRun { nonce }) => {
                    tracing::info!(feed = %feed_cfg.id, nonce, "dry run: not submitting");
                    "dry_run"
                }
                // Debug, not warn. The transition into this state is already a
                // warning, and one line per feed per interval for the length of
                // a jail term would be the loudest thing in the log and the
                // least informative.
                Ok(RoundOutcome::SkippedRefused { nonce, reason }) => {
                    tracing::debug!(
                        feed = %feed_cfg.id, nonce, reason = %reason,
                        "not submitting: this node has no voting weight"
                    );
                    "skipped_refused"
                }
                Ok(RoundOutcome::Failed(why)) | Err(NodeError::Chain(why)) => {
                    tracing::error!(feed = %feed_cfg.id, reason = %why, "round failed");
                    metrics::counter!(
                        "aphelion_round_errors_total",
                        "feed" => feed_cfg.id.to_string(), "kind" => "chain"
                    )
                    .increment(1);
                    "failed"
                }
                Err(e) => {
                    tracing::error!(feed = %feed_cfg.id, error = %e, "round failed");
                    metrics::counter!(
                        "aphelion_round_errors_total",
                        "feed" => feed_cfg.id.to_string(), "kind" => e.kind()
                    )
                    .increment(1);
                    "failed"
                }
            };
            metrics::counter!(
                "aphelion_rounds_total",
                "feed" => feed_cfg.id.to_string(), "outcome" => label
            )
            .increment(1);
        }
    }

    /// One feed, one round. Public so integration tests can drive a single
    /// deterministic round without waiting on a timer.
    pub async fn run_feed(&self, feed: &FeedId, ledger_time: u64) -> Result<RoundOutcome> {
        let feed_cfg = self
            .config
            .feed(feed)
            .ok_or_else(|| NodeError::Config(format!("feed `{feed}` is not configured")))?;

        let observations = self
            .repo
            .latest_per_source(feed, self.config.engine.max_observation_age)
            .await?;

        let params = AggregationParams {
            min_sources: self.config.engine.min_sources_per_feed,
            max_source_deviation_bps: self.config.engine.max_source_deviation_bps,
        };

        let agg = match aggregate(feed, &observations, params) {
            Ok(a) => a,
            Err(e @ NodeError::InsufficientSources { .. }) => {
                return Ok(RoundOutcome::SkippedNoData(e.to_string()));
            }
            Err(e) => return Err(e),
        };

        if !agg.discarded.is_empty() {
            for d in &agg.discarded {
                tracing::warn!(
                    %feed, source = %d.name, price = %d.price,
                    deviation_bps = d.deviation_bps, reason = d.reason,
                    "source excluded from round"
                );
            }
        }

        metrics::gauge!("aphelion_local_price", "feed" => feed.to_string()).set(agg.price.to_f64());
        metrics::gauge!("aphelion_source_spread_bps", "feed" => feed.to_string())
            .set(agg.spread_bps as f64);

        // Worth a fee?
        let last = self.repo.last_submitted_round(feed).await?;
        if let Some(last) = &last {
            let moved = deviation_bps(agg.price.raw(), last.price.raw());
            let age = (Utc::now() - last.created_at).num_seconds().max(0) as u64;
            let heartbeat_due = age >= self.config.engine.heartbeat.as_secs();

            metrics::gauge!("aphelion_seconds_since_submission", "feed" => feed.to_string())
                .set(age as f64);

            if moved < self.config.engine.submit_deviation_bps && !heartbeat_due {
                return Ok(RoundOutcome::SkippedUnchanged {
                    deviation_bps: moved,
                });
            }
        }

        let confidence = confidence_bps(&agg, feed_cfg.confidence_bps);

        // The signed timestamp is the age of the *data*, not of the round. A
        // round assembled from observations that are all 90 seconds old must
        // say so, or a consumer's freshness check is meaningless. It dates the
        // surviving sources only — see `Aggregated::observed_at`.
        let observed_at = agg.observed_at(ledger_time);

        let nonce = self.repo.next_nonce(feed).await?;
        let submission = self
            .signer
            .sign_price(feed, agg.price, observed_at, confidence, nonce);

        // Verify our own signature before paying to publish it. Cheap, and it
        // turns a silent on-chain rejection into a loud local error.
        if !submission.verify(&self.signer.public_key()) {
            return Err(NodeError::Signing(
                "locally produced signature does not verify; key material may be corrupt".into(),
            ));
        }

        let round_id = self
            .repo
            .insert_round(
                feed,
                nonce,
                agg.price,
                confidence,
                agg.used.len() as u32,
                agg.spread_bps,
                agg.stddev,
                chrono::DateTime::from_timestamp(observed_at as i64, 0).unwrap_or_else(Utc::now),
                &submission.signature_hex(),
                RoundStatus::Pending,
            )
            .await?;

        if self.dry_run {
            self.repo
                .settle_round(round_id, RoundStatus::Skipped, None, Some("dry run"))
                .await?;
            tracing::info!(%feed, nonce, price = %agg.price, "dry run: not submitting");
            return Ok(RoundOutcome::DryRun { nonce });
        }

        // Last gate before the fee, and deliberately after everything above.
        // The round is aggregated, signed and written to the local history
        // exactly as it would have been, because all of that is free and it is
        // the record an operator wants when they come back: what this node
        // would have published throughout, rather than a gap. Only the
        // transaction is withheld — the one part that costs money and would
        // have reverted. Same placement, and the same reasoning, as the dry
        // run immediately above.
        //
        // Which also settles the precedence between the two: a dry run reports
        // `DryRun` even for a node that has no weight. Both mean "not
        // submitted", and the one the operator asked for is the more useful
        // answer to why. The refusal is not lost — the refresh logs it, and
        // `status` and `/v1/node` report it — and step 4 of the operator guide
        // is a dry run against a key that is deliberately not registered yet,
        // which would otherwise report a problem that is the instruction.
        let authority = self.authority();
        if !authority.may_submit() {
            let reason = authority.to_string();
            self.repo
                .settle_round(round_id, RoundStatus::Skipped, None, Some(&reason))
                .await?;
            metrics::counter!(
                "aphelion_submissions_total",
                "feed" => feed.to_string(), "outcome" => "refused"
            )
            .increment(1);
            return Ok(RoundOutcome::SkippedRefused { nonce, reason });
        }

        match self
            .chain
            .submit_price(&self.signer.public_key_hex(), &submission)
            .await
        {
            Ok(receipt) => {
                self.repo
                    .settle_round(
                        round_id,
                        RoundStatus::Submitted,
                        receipt.tx_hash.as_deref(),
                        None,
                    )
                    .await?;
                metrics::counter!(
                    "aphelion_submissions_total",
                    "feed" => feed.to_string(), "outcome" => "ok"
                )
                .increment(1);
                Ok(RoundOutcome::Submitted {
                    nonce,
                    tx_hash: receipt.tx_hash,
                })
            }
            Err(e) => {
                // The nonce is deliberately *not* reused. It was allocated,
                // and the aggregator may yet have accepted the transaction
                // even though the response was lost; reissuing it would look
                // like a replay. Burning a nonce costs nothing.
                self.repo
                    .settle_round(round_id, RoundStatus::Failed, None, Some(&e.to_string()))
                    .await?;
                metrics::counter!(
                    "aphelion_submissions_total",
                    "feed" => feed.to_string(), "outcome" => "error"
                )
                .increment(1);
                Ok(RoundOutcome::Failed(e.to_string()))
            }
        }
    }

    /// Pull the node's registry record, cache it, and return what it means for
    /// the next submission.
    ///
    /// Called once at startup and once per tick thereafter. It used to be the
    /// startup call alone, which left `aphelion_reputation` and
    /// `aphelion_stake` reporting whatever was true when the process began: a
    /// node jailed at three in the morning went on exporting the reputation it
    /// had at boot, and the one gauge that would have shown the operator what
    /// had happened was the one that had stopped moving.
    ///
    /// An error here is returned and not swallowed, and the cached authority is
    /// left alone rather than reset. The caller logs it and carries on — see
    /// [`Authority::Unknown`] for why a failed read must never be the reason a
    /// node goes quiet.
    pub async fn refresh_node_snapshot(&self) -> Result<Authority> {
        let pk = self.signer.public_key_hex();
        let ledger_time = self.chain.ledger_time().await?;
        let record = self.chain.node_info(&pk).await?;
        let authority = authority_of(record.as_ref());

        if let Some(node) = &record {
            metrics::gauge!("aphelion_reputation").set(node.reputation as f64);
            metrics::gauge!("aphelion_stake").set(node.stake as f64);
            self.repo
                .save_node_snapshot(
                    &pk,
                    node.stake,
                    node.reputation,
                    &node.status,
                    chrono::DateTime::from_timestamp(ledger_time as i64, 0)
                        .unwrap_or_else(Utc::now),
                )
                .await?;
        }

        // Zero is the honest value here rather than an absent series: a node
        // with no weight is a fact worth graphing, and it is the difference
        // between this node and one whose exporter has stopped.
        metrics::gauge!("aphelion_weight_bps").set(match &authority {
            Authority::Voting { weight_bps } => *weight_bps as f64,
            _ => 0.0,
        });
        // The deadline, on the other hand, is absent when there is none, so a
        // rule on "seconds remaining is low" does not fire forever on a node
        // that is merely healthy.
        if let Some(until) = authority.deadline() {
            metrics::gauge!("aphelion_standing_deadline_seconds")
                .set(until as f64 - ledger_time as f64);
        }

        // Logged on the change rather than on the tick. A jailed node is
        // refused every round for as long as the term runs, and a line per
        // feed per interval would bury the one that said when it started.
        let mut cached = self.authority.lock().unwrap();
        if *cached != authority {
            match &authority {
                Authority::Refused(_) => tracing::warn!(
                    public_key = %pk, authority = %authority,
                    "this node's submissions would not be counted; not paying to send them"
                ),
                _ => tracing::info!(public_key = %pk, authority = %authority, "registry standing"),
            }
            *cached = authority.clone();
        }

        Ok(authority)
    }

    /// Move the local nonce counter past anything the chain has already seen.
    /// Called once at startup; the case it protects against is a node restored
    /// from a database backup that predates its most recent submissions.
    pub async fn resync_nonces(&self) -> Result<()> {
        let pk = self.signer.public_key_hex();
        for feed_cfg in &self.config.feeds {
            match self.chain.last_nonce(&pk, &feed_cfg.id).await {
                Ok(0) => {}
                Ok(on_chain) => {
                    self.repo.bump_nonce_floor(&feed_cfg.id, on_chain).await?;
                    tracing::info!(feed = %feed_cfg.id, on_chain, "nonce counter resynchronised");
                }
                Err(e) => tracing::warn!(
                    feed = %feed_cfg.id, error = %e,
                    "could not read last on-chain nonce; continuing with the local counter"
                ),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn node(status: &str, weight_bps: u32) -> OnChainNode {
        OnChainNode {
            public_key_hex: "ab".repeat(32),
            stake: 1_000 * 10_000_000,
            reputation: if weight_bps > 0 { 8_000 } else { 2_500 },
            status: status.into(),
            weight_bps,
            last_submission: NOW,
            jailed_until: 0,
            unbonding_until: 0,
        }
    }

    #[test]
    fn a_node_with_weight_may_submit() {
        let a = authority_of(Some(&node("active", 10_000)));
        assert_eq!(a, Authority::Voting { weight_bps: 10_000 });
        assert!(a.may_submit());
        assert_eq!(a.deadline(), None);
    }

    /// The expensive case this exists for. `submit_price` reads `weight_of`
    /// and reverts on zero, so every one of these would have been a fee paid
    /// for a transaction that failed.
    #[test]
    fn every_zero_weight_state_is_refused_and_names_why() {
        for (status, expected) in [
            ("jailed", Refusal::Jailed { until: 0 }),
            ("exiting", Refusal::Exiting { until: 0 }),
            ("active", Refusal::NoWeight),
        ] {
            let a = authority_of(Some(&node(status, 0)));
            assert_eq!(a, Authority::Refused(expected), "{status}");
            assert!(!a.may_submit(), "{status}");
        }

        let a = authority_of(None);
        assert_eq!(a, Authority::Refused(Refusal::Unregistered));
        assert!(!a.may_submit());
    }

    /// The registry's `weight_for` does not consult the status string, and
    /// neither does the aggregator: it asks `weight_of` and believes the
    /// answer. So weight decides, and the status only supplies the reason —
    /// a record that somehow said `jailed` while carrying weight would still
    /// be allowed to submit, because the submission would still be counted.
    #[test]
    fn weight_decides_and_the_status_only_explains() {
        assert_eq!(
            authority_of(Some(&node("jailed", 5_000))),
            Authority::Voting { weight_bps: 5_000 }
        );
    }

    /// Spelled `Jailed` through the CLI and `jailed` through the mock. A match
    /// that noticed only one would give the right reason on one path and shrug
    /// on the other.
    #[test]
    fn the_reason_is_read_whatever_case_the_chain_spells_it_in() {
        for spelling in ["jailed", "Jailed", "JAILED"] {
            let mut n = node(spelling, 0);
            n.jailed_until = NOW + 3_600;
            assert_eq!(
                authority_of(Some(&n)),
                Authority::Refused(Refusal::Jailed { until: NOW + 3_600 }),
                "{spelling}"
            );
        }
    }

    /// The deadline feeds a gauge, and a gauge that reported 1970 for every
    /// node with no stored deadline would make the panel unreadable.
    #[test]
    fn a_missing_deadline_produces_no_deadline_rather_than_zero() {
        assert_eq!(
            authority_of(Some(&node("jailed", 0))).deadline(),
            None,
            "zero is the registry's `not set`, not a moment in 1970"
        );
        let mut n = node("exiting", 0);
        n.unbonding_until = NOW + 7 * 24 * 3_600;
        assert_eq!(
            authority_of(Some(&n)).deadline(),
            Some(NOW + 7 * 24 * 3_600)
        );
    }

    /// The most important assertion in this module, and the one that is a
    /// safety property rather than a saving. A refused submission costs one
    /// fee; a round this node is quiet for is a missed round that anybody may
    /// sweep and charge to its reputation. An unreadable registry must never
    /// be able to turn the first into the second.
    #[test]
    fn an_unread_registry_never_stops_a_submission() {
        assert!(Authority::Unknown.may_submit());
        assert_eq!(Authority::Unknown.deadline(), None);
    }

    /// The seam. `authority_of` is a prediction about what the aggregator would
    /// do, and a prediction that drifted from the thing it predicts would
    /// quietly either cost fees again or — far worse — silence a node that was
    /// perfectly entitled to speak. So the prediction is checked against a
    /// chain that enforces the rule: `MockChain` mirrors `submit_price`'s
    /// `weight_of` check, panic for panic, down to the error string.
    #[tokio::test]
    async fn the_prediction_agrees_with_what_the_chain_actually_does() {
        use crate::chain::{ChainClient, MockChain};
        use crate::signer::NodeSigner;
        use aphelion_core::{FeedId, Price};

        let dir = std::env::temp_dir().join(format!("aphelion-authority-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        NodeSigner::generate(&dir.join("key.json")).unwrap();
        let signer = NodeSigner::load(&dir.join("key.json"), [1u8; 32]).unwrap();
        let pk = signer.public_key_hex();
        let feed = FeedId::new("BTC_USD").unwrap();

        for (status, weight, should_submit) in [
            ("active", 10_000, true),
            ("active", 5_000, true),
            ("jailed", 0, false),
            ("exiting", 0, false),
            ("active", 0, false),
        ] {
            let mut record = node(status, weight);
            record.public_key_hex = pk.clone();
            let chain = MockChain::new(NOW).with_node(record.clone());

            let predicted = authority_of(Some(&record)).may_submit();
            assert_eq!(predicted, should_submit, "{status} at {weight} bps");

            let sub = signer.sign_price(&feed, Price::parse_decimal("100").unwrap(), NOW, 50, 1);
            let actual = chain.submit_price(&pk, &sub).await;
            assert_eq!(
                actual.is_ok(),
                predicted,
                "{status} at {weight} bps: the chain and the prediction disagree ({actual:?})"
            );
            if let Err(e) = actual {
                assert!(
                    e.to_string().contains("NotAuthorizedNode"),
                    "refused for the reason this module claims: {e}"
                );
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every refusal says what it is and what ends it, because this string is
    /// what lands in the log, in `/v1/rounds` and in `/v1/node`.
    #[test]
    fn a_refusal_explains_itself_wherever_it_is_printed() {
        assert!(authority_of(None).to_string().contains("register-node.sh"));
        assert!(authority_of(Some(&node("jailed", 0)))
            .to_string()
            .contains("release"));
        assert!(authority_of(Some(&node("exiting", 0)))
            .to_string()
            .contains("no longer votes"));
    }
}
