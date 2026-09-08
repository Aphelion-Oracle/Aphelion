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

#[derive(Serialize)]
struct FeedHealth {
    feed: String,
    live_sources: usize,
    required_sources: usize,
    seconds_since_last_submission: Option<i64>,
    healthy: bool,
    reason: Option<String>,
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

    for feed_cfg in &state.config.feeds {
        let observations = state
            .repo
            .latest_per_source(&feed_cfg.id, state.config.engine.max_observation_age)
            .await?;
        let live = observations.len();
        let required = state.config.engine.min_sources_per_feed;

        let last = state.repo.last_submitted_round(&feed_cfg.id).await?;
        let age = last
            .as_ref()
            .map(|r| (chrono::Utc::now() - r.created_at).num_seconds());

        // Two heartbeats of silence is the threshold: one missed heartbeat can
        // be a slow ledger, two is a pattern.
        let stale_submission = age
            .map(|a| a > 2 * state.config.engine.heartbeat.as_secs() as i64)
            .unwrap_or(false);

        let reason = if live < required {
            Some(format!("only {live} live source(s), need {required}"))
        } else if stale_submission {
            Some(format!("no submission in {}s", age.unwrap_or_default()))
        } else {
            None
        };

        let healthy = reason.is_none();
        all_healthy &= healthy;

        feeds.push(FeedHealth {
            feed: feed_cfg.id.to_string(),
            live_sources: live,
            required_sources: required,
            seconds_since_last_submission: age,
            healthy,
            reason,
        });
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
    let feed = FeedId::new(feed)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

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
    Ok(Json(json!({ "sources": state.repo.source_health().await? })))
}
