//! HTTP handlers.

use std::collections::BTreeMap;

use aphelion_core::FeedId;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::trace::TraceLayer;

use super::AppState;
use crate::db::Observation;
use crate::engine::{aggregate, AggregationParams};
use crate::error::NodeError;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/node", get(node))
        .route("/v1/feeds", get(feeds))
        .route("/v1/prices/{feed}", get(price))
        .route("/v1/rounds", get(rounds))
        .route("/v1/sources", get(sources))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Errors are rendered as JSON with the same shape everywhere, so a dashboard
/// does not have to special-case which endpoint failed.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<crate::error::NodeError> for ApiError {
    fn from(e: crate::error::NodeError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct FeedHealth {
    feed: String,
    live_sources: usize,
    /// Sources that survive outlier filtering — what a round would actually
    /// have to work with.
    usable_sources: usize,
    required_sources: usize,
    seconds_since_last_submission: Option<i64>,
    healthy: bool,
    reason: Option<String>,
}

/// The verdict for one feed, as a pure function of what the node can see.
///
/// Split out from the handler so it can be tested without a database, and
/// because the interesting judgement is here rather than in the plumbing.
///
/// The check runs the real aggregation rather than counting rows. Enough live
/// sources is not the same as enough *usable* ones: four venues that disagree
/// past `max_source_deviation_bps` are four healthy HTTP endpoints and no
/// publishable price. Counting rows would call that feed healthy right up
/// until the submission staleness check noticed, two heartbeats later, and
/// then blame the silence on the chain instead of on the sources.
fn assess(
    feed: &FeedId,
    observations: &[Observation],
    params: AggregationParams,
    seconds_since_last_submission: Option<i64>,
    heartbeat_secs: i64,
) -> FeedHealth {
    let live = observations.len();
    let required = params.min_sources;
    let agg = aggregate(feed, observations, params);

    // The count matters most on the failure path, which is the one an operator
    // is looking at when they call this. "One source survived filtering" and
    // "none did" are different problems -- the first is a single venue away
    // from publishing, the second is a feed with no agreement in it at all --
    // and `InsufficientSources` has already done the counting, so report what
    // it found rather than flattening both to zero.
    let usable = match &agg {
        Ok(a) => a.used.len(),
        Err(NodeError::InsufficientSources { available, .. }) => *available,
        Err(_) => 0,
    };

    // Two heartbeats of silence is the threshold: one missed heartbeat can be
    // a slow ledger, two is a pattern.
    let stale_submission = seconds_since_last_submission
        .map(|a| a > 2 * heartbeat_secs)
        .unwrap_or(false);

    // Source trouble is reported ahead of submission staleness, because when
    // both are true the sources are the cause and the silence is the symptom.
    let reason = match &agg {
        Err(e) => Some(e.to_string()),
        Ok(_) if stale_submission => Some(format!(
            "no submission in {}s",
            seconds_since_last_submission.unwrap_or_default()
        )),
        Ok(_) => None,
    };

    FeedHealth {
        feed: feed.to_string(),
        live_sources: live,
        usable_sources: usable,
        required_sources: required,
        seconds_since_last_submission,
        healthy: reason.is_none(),
        reason,
    }
}

/// Liveness *and* usefulness.
///
/// A node whose process is running but which has not been able to compose a
/// round for ten minutes is not healthy in any sense a load balancer or an
/// on-call engineer cares about, so the check looks at data freshness rather
/// than just answering 200 because the thread is alive. A degraded feed
/// returns 503, which is what makes it page somebody.
async fn health(State(state): State<AppState>) -> ApiResult<Response> {
    let mut feeds = Vec::new();
    let mut all_healthy = true;

    let params = AggregationParams {
        min_sources: state.config.engine.min_sources_per_feed,
        max_source_deviation_bps: state.config.engine.max_source_deviation_bps,
    };

    for feed_cfg in &state.config.feeds {
        let observations = state
            .repo
            .latest_per_source(&feed_cfg.id, state.config.engine.max_observation_age)
            .await?;

        let last = state.repo.last_submitted_round(&feed_cfg.id).await?;
        let age = last
            .as_ref()
            .map(|r| (chrono::Utc::now() - r.created_at).num_seconds());

        let health = assess(
            &feed_cfg.id,
            &observations,
            params,
            age,
            state.config.engine.heartbeat.as_secs() as i64,
        );
        all_healthy &= health.healthy;
        feeds.push(health);
    }

    let body = json!({
        "status": if all_healthy { "healthy" } else { "degraded" },
        "node": state.config.node.name,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.uptime_seconds(),
        "public_key": state.signer.public_key_hex(),
        "feeds": feeds,
    });

    let code = if all_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok((code, Json(body)).into_response())
}

/// Readiness: can the node reach the things it depends on right now?
async fn ready(State(state): State<AppState>) -> ApiResult<Response> {
    let db_ok = sqlx::query("SELECT 1")
        .execute(state.repo.pool())
        .await
        .is_ok();
    let rpc = state.rpc.latest_ledger().await;
    let rpc_ok = rpc.is_ok();

    let body = json!({
        "database": db_ok,
        "rpc": rpc_ok,
        "rpc_url": state.rpc.url(),
        "ledger_sequence": rpc.as_ref().ok().map(|l| l.sequence),
        "rpc_error": rpc.as_ref().err().map(|e| e.to_string()),
    });

    let code = if db_ok && rpc_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok((code, Json(body)).into_response())
}

async fn metrics(State(state): State<AppState>) -> Response {
    match &state.metrics {
        Some(handle) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4")],
            handle.render(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "metrics are disabled").into_response(),
    }
}

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

async fn node(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let pk = state.signer.public_key_hex();
    // Read through to the chain, but never fail the endpoint because RPC is
    // down — an operator debugging a connectivity problem needs this to answer.
    let on_chain = state.chain.node_info(&pk).await.ok().flatten();

    Ok(Json(json!({
        "name": state.config.node.name,
        "public_key": pk,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.uptime_seconds(),
        "network": {
            "rpc_url": state.config.network.rpc_url,
            "registry": state.config.network.registry_contract,
            "aggregator": state.config.network.aggregator_contract,
        },
        "registered": on_chain.is_some(),
        "on_chain": on_chain,
    })))
}

async fn feeds(State(state): State<AppState>) -> Json<serde_json::Value> {
    let feeds: Vec<_> = state
        .config
        .feeds
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "confidence_bps": f.confidence_bps,
                "sources": f.sources.keys().collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({ "feeds": feeds }))
}

#[derive(Serialize)]
struct SourceView {
    source: String,
    price: String,
    observed_at: String,
    age_seconds: i64,
    deviation_bps: u32,
    included: bool,
    excluded_because: Option<&'static str>,
}

/// What this node currently believes about a feed, and how that compares to
/// what the aggregator holds.
///
/// The per-source breakdown is the point: when an operator asks "why is my
/// node's price different?", this endpoint answers it in one request instead
/// of a database session.
async fn price(
    State(state): State<AppState>,
    Path(feed): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let feed = FeedId::new(feed).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

    if state.config.feed(&feed).is_none() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("feed `{feed}` is not configured on this node"),
        ));
    }

    let observations = state
        .repo
        .latest_per_source(&feed, state.config.engine.max_observation_age)
        .await?;

    let params = AggregationParams {
        min_sources: state.config.engine.min_sources_per_feed,
        max_source_deviation_bps: state.config.engine.max_source_deviation_bps,
    };
    let agg = aggregate(&feed, &observations, params);

    let now = chrono::Utc::now();
    let source_views = |obs: &[Observation], agg: &Option<crate::engine::Aggregated>| {
        let excluded: BTreeMap<&str, &'static str> = agg
            .as_ref()
            .map(|a| {
                a.discarded
                    .iter()
                    .map(|d| (d.name.as_str(), d.reason))
                    .collect()
            })
            .unwrap_or_default();
        let reference = agg.as_ref().map(|a| a.price.raw());

        obs.iter()
            .map(|o| SourceView {
                source: o.source.clone(),
                price: o.price.to_string(),
                observed_at: o.observed_at.to_rfc3339(),
                age_seconds: (now - o.observed_at).num_seconds(),
                deviation_bps: reference
                    .map(|r| aphelion_core::deviation_bps(o.price.raw(), r))
                    .unwrap_or(0),
                included: !excluded.contains_key(o.source.as_str()),
                excluded_because: excluded.get(o.source.as_str()).copied(),
            })
            .collect::<Vec<_>>()
    };

    let agg_ok = agg.as_ref().ok().cloned();
    let sources = source_views(&observations, &agg_ok);
    let on_chain = state.chain.latest_price(&feed).await.ok().flatten();

    Ok(Json(json!({
        "feed": feed,
        "local": agg_ok.as_ref().map(|a| json!({
            "price": a.price.to_string(),
            "spread_bps": a.spread_bps,
            "stddev": a.stddev.to_string(),
            "source_count": a.used.len(),
        })),
        "local_error": agg.as_ref().err().map(|e| e.to_string()),
        "on_chain": on_chain,
        "divergence_bps": match (&agg_ok, &on_chain) {
            (Some(a), Some(c)) => Some(aphelion_core::deviation_bps(a.price.raw(), c.price.raw())),
            _ => None,
        },
        "sources": sources,
    })))
}

#[derive(Deserialize)]
struct RoundsQuery {
    feed: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn rounds(
    State(state): State<AppState>,
    Query(q): Query<RoundsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let feed = q
        .feed
        .map(FeedId::new)
        .transpose()
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

    let rounds = state.repo.recent_rounds(feed.as_ref(), q.limit).await?;
    Ok(Json(json!({ "rounds": rounds })))
}

async fn sources(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        json!({ "sources": state.repo.source_health().await? }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aphelion_core::Price;
    use chrono::Utc;

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    fn obs(source: &str, price: &str) -> Observation {
        Observation {
            feed: feed(),
            source: source.into(),
            price: Price::parse_decimal(price).unwrap(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
        }
    }

    fn params() -> AggregationParams {
        AggregationParams {
            min_sources: 2,
            max_source_deviation_bps: 1_000, // 10%
        }
    }

    const HEARTBEAT: i64 = 300;

    #[test]
    fn a_feed_that_can_compose_a_round_is_healthy() {
        let h = assess(
            &feed(),
            &[obs("binance", "100.00"), obs("kraken", "100.01")],
            params(),
            Some(10),
            HEARTBEAT,
        );
        assert!(h.healthy, "{h:?}");
        assert_eq!(h.reason, None);
        assert_eq!(h.usable_sources, 2);
    }

    #[test]
    fn live_sources_that_disagree_are_not_a_healthy_feed() {
        // Three venues answering their HTTP endpoints, no publishable price
        // between them. Counting rows would call this healthy.
        let h = assess(
            &feed(),
            &[
                obs("binance", "100.00"),
                obs("kraken", "500.00"),
                obs("coinbase", "900.00"),
            ],
            params(),
            Some(10),
            HEARTBEAT,
        );
        assert_eq!(h.live_sources, 3, "every venue answered");
        // One survives, and only one: the provisional median is zero bps from
        // itself, so it always clears the filter. The other two are 8000 bps
        // out. Reporting 0 here would say "no agreement anywhere" about a feed
        // that is one honest venue short of publishing.
        assert_eq!(h.usable_sources, 1, "the median survives its own filter");
        assert!(!h.healthy, "a feed that cannot publish is not healthy");
    }

    #[test]
    fn the_usable_count_survives_the_failure_it_describes() {
        // The count is read out of the aggregation error rather than defaulted
        // to zero, because this is the path an operator reads while debugging.
        // "One venue away from publishing" and "nothing here agrees with
        // anything" are different problems that would otherwise arrive at this
        // endpoint looking identical.

        // Too few to aggregate at all: the one source that did report is still
        // reported, not erased.
        let short = assess(
            &feed(),
            &[obs("binance", "100.00")],
            params(),
            Some(10),
            HEARTBEAT,
        );
        assert!(!short.healthy);
        assert_eq!(short.live_sources, 1);
        assert_eq!(short.usable_sources, 1, "{short:?}");

        // A silent feed is the genuinely empty case.
        let nothing = assess(&feed(), &[], params(), Some(10), HEARTBEAT);
        assert!(!nothing.healthy);
        assert_eq!(nothing.usable_sources, 0, "{nothing:?}");
    }

    #[test]
    fn a_dead_venue_is_named_before_the_silence_it_causes() {
        // Both are true: one usable source, and no submission for an hour.
        // The sources are the cause and the silence is the symptom, so the
        // reason has to point at the cause or it sends the operator to the
        // wrong place.
        let h = assess(
            &feed(),
            &[obs("binance", "100.00")],
            params(),
            Some(3_600),
            HEARTBEAT,
        );
        assert!(!h.healthy);
        let reason = h.reason.expect("degraded feeds carry a reason");
        assert!(
            reason.contains("source"),
            "expected the source shortfall, got `{reason}`"
        );
    }

    #[test]
    fn a_healthy_feed_that_has_gone_quiet_is_still_degraded() {
        // Nothing wrong with the data; the node is not getting rounds on
        // chain. One missed heartbeat is a slow ledger, two is a pattern.
        let obs = [obs("binance", "100.00"), obs("kraken", "100.01")];

        let one = assess(&feed(), &obs, params(), Some(HEARTBEAT + 1), HEARTBEAT);
        assert!(one.healthy, "one missed heartbeat is not yet a pattern");

        let two = assess(&feed(), &obs, params(), Some(2 * HEARTBEAT + 1), HEARTBEAT);
        assert!(!two.healthy);
        assert!(two.reason.unwrap().contains("no submission"));
    }

    #[test]
    fn a_node_that_has_never_submitted_is_judged_on_its_data_alone() {
        // A freshly started node has no last submission. That is not two
        // heartbeats of silence, it is no history — reporting it as stale
        // would make every restart page somebody.
        let h = assess(
            &feed(),
            &[obs("binance", "100.00"), obs("kraken", "100.01")],
            params(),
            None,
            HEARTBEAT,
        );
        assert!(h.healthy, "{h:?}");
    }
}
