//! Bitstamp spot.
//!
//! `/api/v2/ticker/{pair}/` is the flattest of the built-in sources: bid, ask
//! and a `timestamp` in epoch *seconds*, all as strings, with no success
//! envelope around them. Failure is signalled by a `status` of `"error"`, which
//! Bitstamp will return with a 404 as well as a 200, so both paths are covered.
//!
//! The pair goes in the path in lower case (`btcusd`), not as a query
//! parameter; the symbol mapping in the config is expected to spell it that
//! way, and nothing here rewrites it.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "bitstamp";
const BASE_URL: &str = "https://www.bitstamp.net/api/v2/ticker";

pub struct Bitstamp {
    client: reqwest::Client,
    base_url: String,
}

impl Bitstamp {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: super::base_url(NAME, BASE_URL),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        if body.get("status").and_then(|s| s.as_str()) == Some("error") {
            let reason = body
                .get("reason")
                .map(|r| r.to_string())
                .unwrap_or_else(|| "unspecified".into());
            return Err(source_err(NAME, feed, format!("bitstamp error: {reason}")));
        }

        let side = |key: &str| -> Result<Price> {
            let raw = body
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| source_err(NAME, feed, format!("ticker has no `{key}`")))?;
            Price::parse_decimal(raw).map_err(|e| source_err(NAME, feed, e))
        };

        // Seconds here, not milliseconds, and quoted as a string.
        let observed_at = body
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .unwrap_or_else(Utc::now);

        Ok(Quote {
            price: mid_price(NAME, feed, side("bid")?, side("ask")?)?,
            observed_at,
        })
    }
}

#[async_trait]
impl PriceSource for Bitstamp {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}/{symbol}/", self.base_url);
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
    fn parses_a_real_response_shape() {
        let body = serde_json::json!({
            "timestamp": "1735689600",
            "open": "64000.00",
            "last": "64231.55",
            "bid": "64231.50",
            "ask": "64231.60"
        });
        let quote = Bitstamp::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
        assert_eq!(quote.observed_at.to_rfc3339(), "2025-01-01T00:00:00+00:00");
    }

    #[test]
    fn surfaces_the_error_status() {
        let body = serde_json::json!({
            "status": "error",
            "reason": "Invalid currency pair"
        });
        let err = Bitstamp::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("Invalid currency pair"), "{err}");
    }

    #[test]
    fn rejects_a_crossed_book() {
        let body = serde_json::json!({
            "timestamp": "1735689600",
            "bid": "64231.70",
            "ask": "64231.60"
        });
        let err = Bitstamp::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("crossed book"), "{err}");
    }

    #[test]
    fn falls_back_to_now_when_the_stamp_is_missing() {
        let body = serde_json::json!({ "bid": "100.00", "ask": "100.02" });
        assert!(Bitstamp::parse(&feed(), &body).is_ok());
    }
}
