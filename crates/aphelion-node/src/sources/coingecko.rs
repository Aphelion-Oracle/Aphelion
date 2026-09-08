//! CoinGecko.
//!
//! Off by default. CoinGecko is not an exchange — it publishes a volume-
//! weighted composite of other venues, several of which are already polled
//! directly, so enabling it adds correlation as well as coverage. It earns its
//! place for long-tail Stellar assets that the three built-in exchanges do not
//! list at all, and as a tie-breaker when exactly two exchanges disagree.
//!
//! Its price arrives as a JSON number rather than a string, so this is the one
//! source that goes through a float on the way in.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{get_json, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "coingecko";
const PUBLIC_URL: &str = "https://api.coingecko.com/api/v3/simple/price";
const PRO_URL: &str = "https://pro-api.coingecko.com/api/v3/simple/price";

pub struct CoinGecko {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl CoinGecko {
    pub fn new(client: reqwest::Client, api_key: Option<String>) -> Self {
        let base_url = if api_key.is_some() { PRO_URL } else { PUBLIC_URL };
        Self {
            client,
            base_url: base_url.to_string(),
            api_key,
        }
    }

    pub(crate) fn parse(feed: &FeedId, coin_id: &str, body: &serde_json::Value) -> Result<Quote> {
        let entry = body
            .get(coin_id)
            .ok_or_else(|| source_err(NAME, feed, format!("response has no entry for `{coin_id}`")))?;

        let price = entry
            .get("usd")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| source_err(NAME, feed, "entry has no `usd` price"))?;
        let price = Price::from_f64(price).map_err(|e| source_err(NAME, feed, e))?;

        let observed_at = entry
            .get("last_updated_at")
            .and_then(|v| v.as_i64())
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .unwrap_or_else(Utc::now);

        Ok(Quote { price, observed_at })
    }
}

#[async_trait]
impl PriceSource for CoinGecko {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!(
            "{}?ids={symbol}&vs_currencies=usd&include_last_updated_at=true",
            self.base_url
        );
        let headers = match &self.api_key {
            Some(key) => vec![("x-cg-pro-api-key", key.clone())],
            None => vec![],
        };
        let body = get_json(&self.client, NAME, feed, &url, &headers).await?;
        Self::parse(feed, symbol, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> FeedId {
        FeedId::new("XLM_USD").unwrap()
    }

    #[test]
    fn parses_a_sub_dollar_price_and_its_timestamp() {
        let body = serde_json::json!({
            "stellar": { "usd": 0.125, "last_updated_at": 1735689600 }
        });
        let quote = CoinGecko::parse(&feed(), "stellar", &body).unwrap();
        assert_eq!(quote.price.to_string(), "0.12500000");
        assert_eq!(quote.observed_at.timestamp(), 1735689600);
    }

    #[test]
    fn rejects_an_empty_object_for_an_unknown_coin() {
        // CoinGecko answers an unknown id with `{}` and HTTP 200.
        let body = serde_json::json!({});
        let err = CoinGecko::parse(&feed(), "not-a-coin", &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no entry"), "{err}");
    }

    #[test]
    fn pro_key_selects_the_pro_endpoint() {
        let client = reqwest::Client::new();
        let free = CoinGecko::new(client.clone(), None);
        let pro = CoinGecko::new(client, Some("key".into()));
        assert!(free.base_url.contains("api.coingecko.com"));
        assert!(pro.base_url.contains("pro-api.coingecko.com"));
    }
}
