//! Price sources.
//!
//! Each source is an independent view of the same market. Independence is the
//! whole point: three venues that all resell the same upstream feed give the
//! appearance of redundancy without any of the substance, so the built-in set
//! is six separate order books (Binance, Kraken, Coinbase, OKX, Bybit,
//! Bitstamp) plus one aggregator (CoinGecko) that is off by default and, when
//! on, is treated as a tie-breaker rather than a peer.
//!
//! Three of the six are on by default and three are not. That is not a ranking
//! of the venues: the outlier filter needs a majority it can trust to be
//! honest, and a default that silently grows the set would change every
//! existing node's median the moment it was upgraded. An operator turns the
//! others on deliberately, which is also when they choose the symbol each one
//! covers.
//!
//! Where a venue exposes an order book, the quote is the **mid of best bid and
//! ask**, not the last trade. A single small trade at a bad price moves the
//! last-trade print; it does not move the mid.

mod binance;
mod bitstamp;
mod bybit;
mod coinbase;
mod coingecko;
mod kraken;
mod okx;

pub use binance::Binance;
pub use bitstamp::Bitstamp;
pub use bybit::Bybit;
pub use coinbase::Coinbase;
pub use coingecko::CoinGecko;
pub use kraken::Kraken;
pub use okx::Okx;

use std::sync::Arc;
use std::time::Duration;

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::config::SourcesConfig;
use crate::error::{NodeError, Result};

/// One observation from one venue.
#[derive(Debug, Clone)]
pub struct Quote {
    pub price: Price,
    /// The venue's own timestamp where it publishes one; otherwise the moment
    /// the response was received. Never fabricated to `now()` when the venue
    /// told us something older.
    pub observed_at: DateTime<Utc>,
}

#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Stable identifier, used as the `source` column and metric label.
    fn name(&self) -> &'static str;

    /// Fetch `symbol` as that venue spells it (e.g. `XBTUSD` on Kraken).
    async fn fetch(&self, feed: &FeedId, symbol: &str) -> Result<Quote>;
}

/// Build the enabled sources from configuration.
pub fn build(cfg: &SourcesConfig) -> Result<Vec<Arc<dyn PriceSource>>> {
    let client = http_client(cfg.timeout)?;
    let mut sources: Vec<Arc<dyn PriceSource>> = Vec::new();

    if cfg.binance {
        sources.push(Arc::new(Binance::new(client.clone())));
    }
    if cfg.kraken {
        sources.push(Arc::new(Kraken::new(client.clone())));
    }
    if cfg.coinbase {
        sources.push(Arc::new(Coinbase::new(client.clone())));
    }
    if cfg.okx {
        sources.push(Arc::new(Okx::new(client.clone())));
    }
    if cfg.bybit {
        sources.push(Arc::new(Bybit::new(client.clone())));
    }
    if cfg.bitstamp {
        sources.push(Arc::new(Bitstamp::new(client.clone())));
    }
    if cfg.coingecko {
        let api_key = cfg
            .coingecko_key_env
            .as_deref()
            .map(crate::Config::secret_from_env)
            .transpose()?;
        sources.push(Arc::new(CoinGecko::new(client, api_key)));
    }

    if sources.is_empty() {
        return Err(NodeError::Config(
            "every price source is disabled; the node has nothing to observe".into(),
        ));
    }
    Ok(sources)
}

/// The endpoint to poll for a venue, overridable by environment.
///
/// `APHELION_SOURCE_URL_<VENUE>` — `APHELION_SOURCE_URL_BINANCE`, and so on —
/// replaces the built-in address. It exists for two reasons. An operator may
/// front an exchange with their own cache or regional proxy, and pointing the
/// node at it should not require a fork. And the multi-process harness points
/// every node at one fake exchange, so that a test of what several nodes do to
/// each other never depends on a real venue being reachable.
///
/// Following `APHELION_STELLAR_BIN`, this is an environment variable rather
/// than a config field: it describes where this particular process happens to
/// run, not what the deployment is, and a config file that is copied between
/// machines should not carry it.
///
/// An empty or whitespace-only value is treated as unset, so `VAR=` in a shell
/// or a compose file means "use the default" rather than "poll the empty URL".
pub(crate) fn base_url(venue: &str, default: &str) -> String {
    std::env::var(format!("APHELION_SOURCE_URL_{}", venue.to_uppercase()))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        // A hung connection to one exchange must not stall the poll loop for
        // the others, so the connect phase is bounded separately.
        .connect_timeout(timeout / 2)
        .user_agent(concat!("aphelion-node/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| NodeError::Config(format!("cannot build HTTP client: {e}")))
}

/// Mid price of a two-sided quote, with the sanity checks a venue will
/// occasionally fail.
///
/// A crossed book (bid above ask) means the snapshot is inconsistent — usually
/// two sides read at different instants during a fast move. Publishing the mid
/// of a crossed book is publishing a number no one could trade at, so it is
/// rejected instead.
pub(crate) fn mid_price(
    source: &'static str,
    feed: &FeedId,
    bid: Price,
    ask: Price,
) -> Result<Price> {
    if !bid.is_positive() || !ask.is_positive() {
        return Err(NodeError::Source {
            venue: source,
            feed: feed.clone(),
            detail: format!("non-positive quote: bid={bid} ask={ask}"),
        });
    }
    if bid.raw() > ask.raw() {
        return Err(NodeError::Source {
            venue: source,
            feed: feed.clone(),
            detail: format!("crossed book: bid={bid} > ask={ask}"),
        });
    }
    // A spread this wide means the venue has effectively no liquidity for the
    // pair right now; the mid would be an invention.
    let spread_bps = aphelion_core::deviation_bps(ask.raw(), bid.raw());
    if spread_bps > MAX_SPREAD_BPS {
        return Err(NodeError::Source {
            venue: source,
            feed: feed.clone(),
            detail: format!("spread {spread_bps} bps exceeds {MAX_SPREAD_BPS} bps limit"),
        });
    }
    Ok(Price::from_raw((bid.raw() + ask.raw()) / 2))
}

/// 5% — generous enough for a thin long-tail pair, tight enough to catch a
/// venue publishing a placeholder on one side.
const MAX_SPREAD_BPS: u32 = 500;

pub(crate) fn source_err(
    source: &'static str,
    feed: &FeedId,
    detail: impl std::fmt::Display,
) -> NodeError {
    NodeError::Source {
        venue: source,
        feed: feed.clone(),
        detail: detail.to_string(),
    }
}

/// Shared JSON GET with a non-2xx status turned into a `Source` error that
/// names the venue, so the log line says who is broken.
pub(crate) async fn get_json(
    client: &reqwest::Client,
    source: &'static str,
    feed: &FeedId,
    url: &str,
    headers: &[(&str, String)],
) -> Result<serde_json::Value> {
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.map_err(|e| source_err(source, feed, e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| source_err(source, feed, e))?;

    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        return Err(source_err(
            source,
            feed,
            format!("HTTP {status}: {snippet}"),
        ));
    }
    serde_json::from_str(&body).map_err(|e| {
        let snippet: String = body.chars().take(200).collect();
        source_err(source, feed, format!("invalid JSON ({e}): {snippet}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    #[test]
    fn mid_is_the_average_of_a_healthy_book() {
        let bid = Price::parse_decimal("100").unwrap();
        let ask = Price::parse_decimal("100.02").unwrap();
        let mid = mid_price("test", &feed(), bid, ask).unwrap();
        assert_eq!(mid.to_string(), "100.01000000");
    }

    #[test]
    fn rejects_a_crossed_book() {
        let bid = Price::parse_decimal("101").unwrap();
        let ask = Price::parse_decimal("100").unwrap();
        let err = mid_price("test", &feed(), bid, ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("crossed book"), "{err}");
    }

    #[test]
    fn rejects_an_absurd_spread() {
        let bid = Price::parse_decimal("100").unwrap();
        let ask = Price::parse_decimal("200").unwrap();
        let err = mid_price("test", &feed(), bid, ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spread"), "{err}");
    }

    #[test]
    fn build_refuses_a_fully_disabled_source_set() {
        let cfg = SourcesConfig {
            binance: false,
            kraken: false,
            coinbase: false,
            okx: false,
            bybit: false,
            bitstamp: false,
            coingecko: false,
            coingecko_key_env: None,
            timeout: Duration::from_secs(5),
        };
        assert!(build(&cfg).is_err());
    }

    #[test]
    fn the_new_venues_are_off_until_asked_for() {
        // An operator who upgrades without touching their config must keep the
        // median they had, so the defaults cannot grow.
        let cfg = SourcesConfig::default();
        let names: Vec<_> = build(&cfg).unwrap().iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["binance", "kraken", "coinbase"]);
    }

    #[test]
    fn build_includes_every_venue_when_all_are_enabled() {
        let cfg = SourcesConfig {
            binance: true,
            kraken: true,
            coinbase: true,
            okx: true,
            bybit: true,
            bitstamp: true,
            coingecko: false,
            coingecko_key_env: None,
            timeout: Duration::from_secs(5),
        };
        let names: Vec<_> = build(&cfg).unwrap().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec!["binance", "kraken", "coinbase", "okx", "bybit", "bitstamp"]
        );
    }

    #[test]
    fn is_enabled_knows_every_venue_build_can_produce() {
        // These two must not drift: a venue `build` can start but `is_enabled`
        // does not recognise would be polled and then reported as unconfigured.
        let cfg = SourcesConfig {
            binance: true,
            kraken: true,
            coinbase: true,
            okx: true,
            bybit: true,
            bitstamp: true,
            coingecko: true,
            coingecko_key_env: None,
            timeout: Duration::from_secs(5),
        };
        for source in build(&cfg).unwrap() {
            assert!(cfg.is_enabled(source.name()), "{}", source.name());
        }
    }

    #[test]
    fn build_includes_exactly_the_enabled_sources() {
        let cfg = SourcesConfig {
            binance: true,
            kraken: true,
            coinbase: false,
            okx: false,
            bybit: false,
            bitstamp: false,
            coingecko: false,
            coingecko_key_env: None,
            timeout: Duration::from_secs(5),
        };
        let names: Vec<_> = build(&cfg).unwrap().iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["binance", "kraken"]);
    }
}
