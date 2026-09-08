//! Binance spot.
//!
//! Uses `/api/v3/ticker/bookTicker`, which returns best bid and ask for one
//! symbol in a single small response. Binance does not stamp this endpoint, so
//! the observation time is the moment the response was received.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::Utc;

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "binance";
const BASE_URL: &str = "https://api.binance.com/api/v3/ticker/bookTicker";

pub struct Binance {
    client: reqwest::Client,
    base_url: String,
}

impl Binance {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: BASE_URL.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        let bid = body
            .get("bidPrice")
            .and_then(|v| v.as_str())
            .ok_or_else(|| source_err(NAME, feed, "response has no bidPrice"))?;
        let ask = body
            .get("askPrice")
            .and_then(|v| v.as_str())
            .ok_or_else(|| source_err(NAME, feed, "response has no askPrice"))?;

        let bid = Price::parse_decimal(bid).map_err(|e| source_err(NAME, feed, e))?;
        let ask = Price::parse_decimal(ask).map_err(|e| source_err(NAME, feed, e))?;

        Ok(Quote {
            price: mid_price(NAME, feed, bid, ask)?,
            observed_at: Utc::now(),
        })
    }
}

#[async_trait]
impl PriceSource for Binance {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}?symbol={symbol}", self.base_url);
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
            "symbol": "BTCUSDT",
            "bidPrice": "64231.50000000",
            "bidQty": "0.01",
            "askPrice": "64231.60000000",
            "askQty": "0.02"
        });
        let quote = Binance::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
    }

    #[test]
    fn rejects_the_error_envelope_binance_returns_for_a_bad_symbol() {
        // Binance answers an unknown symbol with 400 and this body; if the
        // status check were ever removed, parsing must still refuse it.
        let body = serde_json::json!({ "code": -1121, "msg": "Invalid symbol." });
        let err = Binance::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("no bidPrice"), "{err}");
    }

    #[test]
    fn rejects_a_zero_side() {
        let body = serde_json::json!({ "bidPrice": "0.00", "askPrice": "64231.60" });
        assert!(Binance::parse(&feed(), &body).is_err());
    }
}
