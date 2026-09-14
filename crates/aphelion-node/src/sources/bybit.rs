//! Bybit spot.
//!
//! `/v5/market/tickers` is a category-wide endpoint narrowed to one symbol by
//! query parameter, so `category=spot` is not optional: without it Bybit
//! defaults to a derivatives book, and a perpetual's mark is not the spot price
//! this network is quoting.
//!
//! Failures arrive inside a 200 response as a non-zero `retCode`, and the
//! stamp is at the top of the envelope rather than on the ticker itself.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "bybit";
const BASE_URL: &str = "https://api.bybit.com/v5/market/tickers";

pub struct Bybit {
    client: reqwest::Client,
    base_url: String,
}

impl Bybit {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: super::base_url(NAME, BASE_URL),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        let ret_code = body
            .get("retCode")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| source_err(NAME, feed, "response has no retCode"))?;
        if ret_code != 0 {
            let msg = body.get("retMsg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(source_err(
                NAME,
                feed,
                format!("bybit error {ret_code}: {msg}"),
            ));
        }

        let list = body
            .get("result")
            .and_then(|r| r.get("list"))
            .and_then(|l| l.as_array())
            .ok_or_else(|| source_err(NAME, feed, "response has no result list"))?;
        // Narrowed by `symbol`, so anything but one entry means the request was
        // not narrowed the way this code assumes.
        if list.len() != 1 {
            return Err(source_err(
                NAME,
                feed,
                format!("expected exactly one symbol, got {}", list.len()),
            ));
        }
        let ticker = &list[0];

        let side = |key: &str| -> Result<Price> {
            let raw = ticker
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| source_err(NAME, feed, format!("ticker has no `{key}`")))?;
            Price::parse_decimal(raw).map_err(|e| source_err(NAME, feed, e))
        };

        let observed_at = body
            .get("time")
            .and_then(|t| t.as_i64())
            .and_then(DateTime::from_timestamp_millis)
            .unwrap_or_else(Utc::now);

        Ok(Quote {
            price: mid_price(NAME, feed, side("bid1Price")?, side("ask1Price")?)?,
            observed_at,
        })
    }
}

#[async_trait]
impl PriceSource for Bybit {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}?category=spot&symbol={symbol}", self.base_url);
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
            "retCode": 0,
            "retMsg": "OK",
            "result": {
                "category": "spot",
                "list": [{
                    "symbol": "BTCUSDT",
                    "lastPrice": "64231.55",
                    "bid1Price": "64231.50",
                    "ask1Price": "64231.60"
                }]
            },
            "time": 1735689600000i64
        });
        let quote = Bybit::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
        assert_eq!(quote.observed_at.to_rfc3339(), "2025-01-01T00:00:00+00:00");
    }

    #[test]
    fn surfaces_the_in_band_error_code() {
        let body = serde_json::json!({
            "retCode": 10001,
            "retMsg": "params error: Symbol Is Invalid",
            "result": {}
        });
        let err = Bybit::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("Symbol Is Invalid"), "{err}");
    }

    #[test]
    fn refuses_an_empty_list() {
        let body = serde_json::json!({
            "retCode": 0,
            "result": { "list": [] },
            "time": 1735689600000i64
        });
        let err = Bybit::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("exactly one symbol"), "{err}");
    }

    #[test]
    fn rejects_a_zero_side() {
        let body = serde_json::json!({
            "retCode": 0,
            "result": { "list": [{ "bid1Price": "0", "ask1Price": "64231.60" }] },
            "time": 1735689600000i64
        });
        assert!(Bybit::parse(&feed(), &body).is_err());
    }
}
