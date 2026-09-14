//! OKX spot.
//!
//! `/api/v5/market/ticker` returns best bid and ask for one instrument, and
//! stamps the response in `ts` (epoch milliseconds), so the observation time is
//! the venue's own rather than ours.
//!
//! OKX reports failures inside a 200 response — `code` is a string, and `"0"`
//! is the only success — so the envelope has to be checked explicitly before
//! the payload is read. The payload itself is a one-element array; a request
//! for an unknown instrument comes back successful with that array empty.

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{get_json, mid_price, source_err, PriceSource, Quote};
use crate::error::Result;

const NAME: &str = "okx";
const BASE_URL: &str = "https://www.okx.com/api/v5/market/ticker";

pub struct Okx {
    client: reqwest::Client,
    base_url: String,
}

impl Okx {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: super::base_url(NAME, BASE_URL),
        }
    }

    pub(crate) fn parse(feed: &FeedId, body: &serde_json::Value) -> Result<Quote> {
        // `code` is a string on this API, not a number, and anything but "0"
        // means the `data` below is not worth reading.
        let code = body
            .get("code")
            .and_then(|c| c.as_str())
            .ok_or_else(|| source_err(NAME, feed, "response has no code"))?;
        if code != "0" {
            let msg = body.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(source_err(NAME, feed, format!("okx error {code}: {msg}")));
        }

        let data = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| source_err(NAME, feed, "response has no data array"))?;
        // An unknown instrument is a successful response with nothing in it,
        // and more than one entry would leave which to use ambiguous.
        if data.len() != 1 {
            return Err(source_err(
                NAME,
                feed,
                format!("expected exactly one instrument, got {}", data.len()),
            ));
        }
        let ticker = &data[0];

        let side = |key: &str| -> Result<Price> {
            let raw = ticker
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| source_err(NAME, feed, format!("ticker has no `{key}`")))?;
            Price::parse_decimal(raw).map_err(|e| source_err(NAME, feed, e))
        };

        Ok(Quote {
            price: mid_price(NAME, feed, side("bidPx")?, side("askPx")?)?,
            observed_at: parse_millis(ticker.get("ts")).unwrap_or_else(Utc::now),
        })
    }
}

/// OKX stamps in epoch milliseconds, quoted as a string.
fn parse_millis(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let ms: i64 = value?.as_str()?.parse().ok()?;
    DateTime::from_timestamp_millis(ms)
}

#[async_trait]
impl PriceSource for Okx {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote> {
        let url = format!("{}?instId={symbol}", self.base_url);
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
            "code": "0",
            "msg": "",
            "data": [{
                "instType": "SPOT",
                "instId": "BTC-USDT",
                "last": "64231.5",
                "bidPx": "64231.5",
                "askPx": "64231.6",
                "ts": "1735689600000"
            }]
        });
        let quote = Okx::parse(&feed(), &body).unwrap();
        assert_eq!(quote.price.to_string(), "64231.55000000");
    }

    #[test]
    fn uses_the_venue_supplied_timestamp() {
        let body = serde_json::json!({
            "code": "0",
            "data": [{ "bidPx": "100.00", "askPx": "100.02", "ts": "1735689600000" }]
        });
        let quote = Okx::parse(&feed(), &body).unwrap();
        assert_eq!(quote.observed_at.to_rfc3339(), "2025-01-01T00:00:00+00:00");
    }

    #[test]
    fn falls_back_to_now_when_the_stamp_is_unparseable() {
        let body = serde_json::json!({
            "code": "0",
            "data": [{ "bidPx": "100.00", "askPx": "100.02", "ts": "not-a-number" }]
        });
        // A venue that stops stamping is still quoting; only the age is ours.
        assert!(Okx::parse(&feed(), &body).is_ok());
    }

    #[test]
    fn surfaces_the_in_band_error_code() {
        let body = serde_json::json!({
            "code": "51001",
            "msg": "Instrument ID does not exist",
            "data": []
        });
        let err = Okx::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn refuses_an_empty_data_array() {
        let body = serde_json::json!({ "code": "0", "msg": "", "data": [] });
        let err = Okx::parse(&feed(), &body).unwrap_err().to_string();
        assert!(err.contains("exactly one instrument"), "{err}");
    }

    #[test]
    fn rejects_a_crossed_book() {
        let body = serde_json::json!({
            "code": "0",
            "data": [{ "bidPx": "100.10", "askPx": "100.00", "ts": "1735689600000" }]
        });
        assert!(Okx::parse(&feed(), &body).is_err());
    }
}
