//! Kraken spot.
//!
//! `/0/public/Ticker` answers with a result map keyed by Kraken's *canonical*
//! pair name, which is frequently not the name you asked for — request
//! `XBTUSD`, receive `XXBTZUSD`. The parser therefore takes the single entry in
//! the map rather than looking up by the requested symbol, and treats a
//! multi-entry result as an error since it would be ambiguous.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::Utc;

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "kraken";
const BASE_URL: &str = "https://api.kraken.com/0/public/Ticker";

pub struct Kraken {
    client: reqwest::Client,
    base_url: String,
}

impl Kraken {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: super::base_url(NAME, BASE_URL),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        // Kraken reports failures in a 200 response, so the error array has to
        // be checked explicitly.
        if let Some(errors) = body.get("error").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                return Err(source_err(NAME, feed, format!("kraken error: {errors:?}")));
            }
        }
        let result = body
            .get("result")
            .and_then(|r| r.as_object())
            .ok_or_else(|| source_err(NAME, feed, "response has no result object"))?;

        if result.len() != 1 {
            return Err(source_err(
                NAME,
                feed,
                format!("expected exactly one pair in result, got {}", result.len()),
            ));
        }
        let (_pair, ticker) = result.iter().next().expect("len checked above");

        // "a" = ask [price, whole_lot_volume, lot_volume]
        // "b" = bid [price, whole_lot_volume, lot_volume]
        let side = |key: &str| -> Result<Price> {
            let raw = ticker
                .get(key)
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .ok_or_else(|| source_err(NAME, feed, format!("ticker has no `{key}` price")))?;
            Price::parse_decimal(raw).map_err(|e| source_err(NAME, feed, e))
        };

        Ok(Quote {
            price: mid_price(NAME, feed, side("b")?, side("a")?)?,
            observed_at: Utc::now(),
        })
    }
}

#[async_trait]
impl PriceSource for Kraken {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}?pair={symbol}", self.base_url);
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
    fn parses_a_renamed_pair() {
        // Asked for XBTUSD, Kraken answers under XXBTZUSD.
        let body = serde_json::json!({
            "error": [],
            "result": {
                "XXBTZUSD": {
                    "a": ["64231.60000", "1", "1.000"],
                    "b": ["64231.50000", "2", "2.000"],
                    "c": ["64231.55000", "0.001"]
                }
            }
        });
        let quote = Kraken::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
    }

    #[test]
    fn surfaces_the_in_band_error_array() {
        let body = serde_json::json!({ "error": ["EQuery:Unknown asset pair"], "result": {} });
        let err = Kraken::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("Unknown asset pair"), "{err}");
    }

    #[test]
    fn refuses_an_ambiguous_multi_pair_result() {
        let body = serde_json::json!({
            "error": [],
            "result": {
                "XXBTZUSD": { "a": ["1", "1", "1"], "b": ["1", "1", "1"] },
                "XETHZUSD": { "a": ["2", "1", "1"], "b": ["2", "1", "1"] }
            }
        });
        assert!(Kraken::parse(&feed(), &body).is_err());
    }
}
