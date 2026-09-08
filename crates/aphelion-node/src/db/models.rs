//! Row types.
//!
//! `price_raw` columns are `NUMERIC(40,0)`, which no Rust integer type maps to
//! directly, so every query casts them to `text` and these structs parse the
//! result. Verbose, but exact — and precision is not something an oracle gets
//! to be casual about.

use aphelion_core::{FeedId, Price};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;

use crate::error::{NodeError, Result};

/// Parse a `NUMERIC(40,0)` rendered as text into a scaled price.
pub(crate) fn parse_raw_price(s: &str) -> Result<Price> {
    s.trim()
        .parse::<i128>()
        .map(Price::from_raw)
        .map_err(|e| NodeError::Other(anyhow::anyhow!("bad NUMERIC in database: `{s}` ({e})")))
}

#[derive(Debug, Clone, FromRow)]
pub struct RawPriceRow {
    pub id: i64,
    pub feed_id: String,
    pub source: String,
    pub price_text: String,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

/// A raw observation, decoded into domain types.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub feed: FeedId,
    pub source: String,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl RawPriceRow {
    pub fn decode(self) -> Result<Observation> {
        Ok(Observation {
            feed: FeedId::new(self.feed_id).map_err(|e| NodeError::Other(e.into()))?,
            source: self.source,
            price: parse_raw_price(&self.price_text)?,
            observed_at: self.observed_at,
            received_at: self.received_at,
        })
    }
}

/// Lifecycle of a round this node composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoundStatus {
    /// Signed, not yet confirmed on chain.
    Pending,
    /// Landed in a ledger.
    Submitted,
    /// The transaction failed or the contract rejected it.
    Failed,
    /// Deliberately not submitted (price had not moved enough).
    Skipped,
}

impl RoundStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RoundStatus::Pending => "pending",
            RoundStatus::Submitted => "submitted",
            RoundStatus::Failed => "failed",
            RoundStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct LocalRoundRow {
    pub id: i64,
    pub feed_id: String,
    pub nonce: i64,
    pub price_text: String,
    pub confidence_bps: i32,
    pub source_count: i32,
    pub spread_bps: i32,
    pub observed_at: DateTime<Utc>,
    pub status: String,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One round, decoded. This is what `/rounds` returns and what an operator
/// pastes into a dispute if their submission is ever challenged.
#[derive(Debug, Clone, Serialize)]
pub struct LocalRound {
    pub id: i64,
    pub feed: FeedId,
    pub nonce: u64,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub confidence_bps: u32,
    pub source_count: u32,
    pub spread_bps: u32,
    pub observed_at: DateTime<Utc>,
    pub status: String,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl LocalRoundRow {
    pub fn decode(self) -> Result<LocalRound> {
        Ok(LocalRound {
            id: self.id,
            feed: FeedId::new(self.feed_id).map_err(|e| NodeError::Other(e.into()))?,
            nonce: self.nonce as u64,
            price: parse_raw_price(&self.price_text)?,
            confidence_bps: self.confidence_bps as u32,
            source_count: self.source_count as u32,
            spread_bps: self.spread_bps as u32,
            observed_at: self.observed_at,
            status: self.status,
            tx_hash: self.tx_hash,
            error: self.error,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct SourceHealthRow {
    pub source: String,
    pub feed_id: String,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
    pub total_successes: i64,
    pub total_failures: i64,
}

/// Prices are serialised as decimal strings, never as JSON numbers — a
/// consumer parsing them into an IEEE double would silently lose the low
/// digits of a large price.
fn ser_price<S: serde::Serializer>(p: &Price, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&p.to_string())
}
