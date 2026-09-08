//! Coinbase Exchange.
//!
//! `/products/{id}/ticker` is the only built-in source that stamps its own
//! response, so its `time` field is used as the observation time. That matters
//! during an outage: if Coinbase starts serving a cached snapshot, the age
//! check in the round loop sees the real staleness rather than `now()`.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "coinbase";
const BASE_URL: &str = "https://api.exchange.coinbase.com/products";

pub struct Coinbase {
    client: reqwest::Client,
    base_url: String,
}

impl Coinbase {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: BASE_URL.to_string(),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        if let Some(message) = body.get("message").and_then(|m| m.as_str()) {
            return Err(source_err(NAME, feed, format!("coinbase error: {message}")));
        }

        let side = |key: &str| -> Result<Price> {
            let raw = body
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| source_err(NAME, feed, format!("ticker has no `{key}`")))?;
            Price::parse_decimal(raw).map_err(|e| source_err(NAME, feed, e))
        };

        let observed_at = body
            .get("time")
            .and_then(|t| t.as_str())
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            // Falling back to now() is safe here only because a missing `time`
            // means the field was absent, not that it was old.
            .unwrap_or_else(Utc::now);

        Ok(Quote {
            price: mid_price(NAME, feed, side("bid")?, side("ask")?)?,
            observed_at,
        })
    }
}

#[async_trait]
impl PriceSource for Coinbase {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}/{symbol}/ticker", self.base_url);
        let body = get_json(&self.client, NAME, feed, &url, &[]).await?;
        Self::parse(feed, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    #[test]
    fn uses_the_venue_supplied_timestamp() {
        let body = serde_json::json!({
            "trade_id": 1,
            "price": "64231.55",
            "bid": "64231.50",
            "ask": "64231.60",
            "time": "2025-01-01T00:00:00.000000Z"
        });
        let quote = Coinbase::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
        assert_eq!(quote.observed_at.to_rfc3339(), "2025-01-01T00:00:00+00:00");
    }

    #[test]
    fn surfaces_the_error_message_body() {
        let body = serde_json::json!({ "message": "NotFound" });
        let err = Coinbase::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("NotFound"), "{err}");
    }
}
