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

use std::sync::Arc;
use std::time::Instant;

use aphelion_core::{deviation_bps, FeedId};
use chrono::Utc;

use super::aggregate::{aggregate, confidence_bps, AggregationParams};
use crate::chain::ChainClient;
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
    Failed(String),
}

pub struct RoundRunner {
    config: Arc<Config>,
    repo: Repo,
    signer: Arc<NodeSigner>,
    chain: Arc<dyn ChainClient>,
    dry_run: bool,
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
        }
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

    /// Pull the node's registry record and cache it, so the API can report
    /// reputation and stake without an RPC round trip.
    pub async fn refresh_node_snapshot(&self) -> Result<()> {
        let pk = self.signer.public_key_hex();
        let ledger_time = self.chain.ledger_time().await?;
        match self.chain.node_info(&pk).await? {
            Some(node) => {
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
            None => {
                tracing::warn!(
                    public_key = %pk,
                    "this node is not registered on chain; submissions will be rejected. \
                     Run `scripts/register-node.sh`."
                );
            }
        }
        Ok(())
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
