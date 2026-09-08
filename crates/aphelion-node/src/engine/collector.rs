//! The collection loop.
//!
//! Polls every (feed, source) pair on a fixed cadence and writes what it sees
//! to Postgres. It never decides anything — no filtering, no aggregation — so
//! that the raw record of what each venue said survives independently of the
//! logic that interprets it. When a node is challenged over a published price,
//! this table is the evidence.
//!
//! Sources are polled concurrently, and one failing venue never blocks or
//! fails the sweep: a failure is recorded against that source's health and the
//! loop moves on.

use std::sync::Arc;
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::config::Config;
use crate::db::Repo;
use crate::error::Result;
use crate::sources::PriceSource;

pub struct Collector {
    config: Arc<Config>,
    sources: Vec<Arc<dyn PriceSource>>,
    repo: Repo,
}

impl Collector {
    pub fn new(config: Arc<Config>, sources: Vec<Arc<dyn PriceSource>>, repo: Repo) -> Self {
        Self {
            config,
            sources,
            repo,
        }
    }

    /// Run until cancelled.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.config.engine.poll_interval);
        // If a sweep overruns the interval, skip the missed ticks rather than
        // queueing them: catching up on stale polls only adds load to an
        // exchange that is already slow.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let started = Instant::now();
                    let (ok, failed) = self.sweep().await;
                    tracing::debug!(
                        ok, failed, elapsed_ms = started.elapsed().as_millis() as u64,
                        "collection sweep complete"
                    );
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("collector shutting down");
                        return;
                    }
                }
            }
        }
    }

    /// One pass over every configured (feed, source) pair.
    /// Returns `(successes, failures)`.
    pub async fn sweep(&self) -> (usize, usize) {
        let mut tasks = FuturesUnordered::new();

        for feed_cfg in &self.config.feeds {
            for source in &self.sources {
                let Some(symbol) = feed_cfg.sources.get(source.name()) else {
                    // This venue does not list this pair; not an error.
                    continue;
                };
                let source = Arc::clone(source);
                let repo = self.repo.clone();
                let feed = feed_cfg.id.clone();
                let symbol = symbol.clone();

                tasks.push(async move {
                    let started = Instant::now();
                    let result = source.fetch(&feed, &symbol).await;
                    let latency = started.elapsed().as_secs_f64();

                    metrics::histogram!(
                        "aphelion_source_latency_seconds",
                        "source" => source.name()
                    )
                    .record(latency);

                    match result {
                        Ok(quote) => {
                            metrics::counter!(
                                "aphelion_source_fetches_total",
                                "source" => source.name(), "outcome" => "ok"
                            )
                            .increment(1);
                            metrics::gauge!(
                                "aphelion_source_price",
                                "source" => source.name(), "feed" => feed.to_string()
                            )
                            .set(quote.price.to_f64());

                            if let Err(e) = repo
                                .record_observation(&feed, source.name(), quote.price, quote.observed_at)
                                .await
                            {
                                tracing::error!(
                                    source = source.name(), %feed, error = %e,
                                    "could not persist observation"
                                );
                                return false;
                            }
                            true
                        }
                        Err(e) => {
                            metrics::counter!(
                                "aphelion_source_fetches_total",
                                "source" => source.name(), "outcome" => "error"
                            )
                            .increment(1);
                            // Debug, not warn: exchanges rate-limit constantly
                            // and a warn-level line per poll would bury real
                            // problems. Sustained failure surfaces through
                            // source_health and the /health endpoint instead.
                            tracing::debug!(
                                source = source.name(), %feed, error = %e,
                                "source fetch failed"
                            );
                            let _ = repo
                                .record_source_failure(&feed, source.name(), &e.to_string())
                                .await;
                            false
                        }
                    }
                });
            }
        }

        let mut ok = 0;
        let mut failed = 0;
        while let Some(success) = tasks.next().await {
            if success {
                ok += 1;
            } else {
                failed += 1;
            }
        }
        (ok, failed)
    }
}

/// Periodic retention job. Runs hourly rather than on every sweep because the
/// delete is the most expensive statement the node issues and the data it
/// removes is, by definition, no longer urgent.
pub async fn run_retention(
    repo: Repo,
    retention: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match repo.prune(retention).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, "pruned old observations"),
                    Err(e) => tracing::warn!(error = %e, "retention pass failed"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}
