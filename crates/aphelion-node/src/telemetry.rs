//! Logging and metrics setup.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::error::{NodeError, Result};

/// Initialise tracing. `APHELION_LOG` (or `RUST_LOG`) controls the filter;
/// `APHELION_LOG_FORMAT=json` switches to structured output for log shippers.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_env("APHELION_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,aphelion_node=debug,sqlx=warn"));

    let json = std::env::var("APHELION_LOG_FORMAT").as_deref() == Ok("json");
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(fmt::layer().json().with_current_span(true))
            .init();
    } else {
        registry.with(fmt::layer().with_target(true)).init();
    }
}

/// Install the Prometheus recorder and return a handle the HTTP layer renders.
pub fn init_metrics() -> Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("aphelion_round_duration_seconds".into()),
            &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0],
        )
        .and_then(|b| {
            b.set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(
                    "aphelion_source_latency_seconds".into(),
                ),
                &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0],
            )
        })
        .map_err(|e| NodeError::Config(format!("prometheus setup failed: {e}")))?
        .install_recorder()
        .map_err(|e| NodeError::Config(format!("prometheus recorder failed: {e}")))?;

    describe_metrics();
    Ok(handle)
}

/// Names and meanings in one place, so the Grafana dashboard and the code
/// cannot drift apart silently.
fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_counter!(
        "aphelion_source_fetches_total",
        "Price fetches attempted, labelled by source and outcome"
    );
    describe_histogram!(
        "aphelion_source_latency_seconds",
        Unit::Seconds,
        "Wall time of a single source fetch"
    );
    describe_gauge!(
        "aphelion_source_price",
        "Most recent price seen from each source, per feed"
    );
    describe_counter!(
        "aphelion_rounds_total",
        "Aggregation rounds completed, labelled by feed and outcome"
    );
    describe_counter!(
        "aphelion_round_errors_total",
        "Rounds that failed, labelled by error kind, and by feed where one is \
         to blame. Failures that abort the whole tick before any feed is \
         reached -- an unreadable ledger time, a clock too far from it -- carry \
         no feed label, because no feed is at fault. Aggregate with `sum by \
         (kind)` rather than by feed, or those land in an empty bucket"
    );
    describe_histogram!(
        "aphelion_round_duration_seconds",
        Unit::Seconds,
        "Wall time from opening a round to submitting or skipping it"
    );
    describe_gauge!(
        "aphelion_local_price",
        "This node's aggregated price per feed, as submitted"
    );
    describe_gauge!(
        "aphelion_source_spread_bps",
        "Spread between the highest and lowest source for a feed, in basis points"
    );
    describe_counter!(
        "aphelion_submissions_total",
        "On-chain submissions attempted, labelled by feed and outcome"
    );
    describe_gauge!(
        "aphelion_clock_skew_seconds",
        "Node clock minus ledger clock; a large value means submissions will be rejected"
    );
    describe_gauge!(
        "aphelion_reputation",
        "This node's on-chain reputation, 0..10000"
    );
    describe_gauge!("aphelion_stake", "This node's bonded stake, in stroops");
    describe_gauge!(
        "aphelion_seconds_since_submission",
        "Seconds since this node last landed a submission for a feed"
    );
}
