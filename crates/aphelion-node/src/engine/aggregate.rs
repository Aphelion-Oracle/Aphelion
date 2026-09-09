//! Cross-source aggregation.
//!
//! This is the node's *local* consensus: reducing several venues' opinions to
//! the one number it is willing to sign. The chain then runs a second,
//! independent consensus across nodes. The two layers defend against different
//! things — this one against a single broken exchange, the on-chain one
//! against a single dishonest operator — and neither substitutes for the other.
//!
//! Everything here is a pure function of its inputs so that a disputed round
//! can be replayed exactly from the observations stored in `raw_prices`.

use aphelion_core::{deviation_bps, stddev, weighted_median, FeedId, Price, WeightedSample};
use chrono::{DateTime, Utc};

use crate::db::Observation;
use crate::error::{NodeError, Result};

/// Tunables, lifted from `EngineConfig` so this module can be tested without
/// constructing a whole configuration.
#[derive(Debug, Clone, Copy)]
pub struct AggregationParams {
    pub min_sources: usize,
    /// A source further than this from the provisional median is discarded.
    pub max_source_deviation_bps: u32,
}

/// What the node decided, and why.
#[derive(Debug, Clone)]
pub struct Aggregated {
    pub feed: FeedId,
    pub price: Price,
    /// Highest minus lowest surviving source, in basis points.
    pub spread_bps: u32,
    pub stddev: Price,
    /// Sources that contributed to the final price.
    pub used: Vec<ContributingSource>,
    /// Sources that were dropped, with the reason. Kept because "why is my
    /// node's price different from everyone else's?" is answered here.
    pub discarded: Vec<DiscardedSource>,
}

#[derive(Debug, Clone)]
pub struct ContributingSource {
    pub name: String,
    pub price: Price,
    pub deviation_bps: u32,
    /// When the venue reported this price, carried through so the round can
    /// sign the age of the data it actually used. See
    /// [`Aggregated::observed_at`].
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct DiscardedSource {
    pub name: String,
    pub price: Price,
    pub deviation_bps: u32,
    pub reason: &'static str,
}

/// Reduce per-source observations to a single price.
///
/// Two passes, deliberately:
///
/// 1. A provisional median over every observation. The median is used as the
///    reference rather than the mean precisely because the reference must not
///    already be dragged by the outlier we are about to look for.
/// 2. Drop anything further than `max_source_deviation_bps` from that
///    reference, then take the median again over what is left.
///
/// If the second pass leaves fewer than `min_sources` survivors the node signs
/// nothing. Publishing a price backed by one exchange is how an oracle
/// launders a single venue's outage into consensus.
pub fn aggregate(
    feed: &FeedId,
    observations: &[Observation],
    params: AggregationParams,
) -> Result<Aggregated> {
    if observations.len() < params.min_sources {
        return Err(NodeError::InsufficientSources {
            feed: feed.clone(),
            available: observations.len(),
            required: params.min_sources,
        });
    }

    // Pass 1: provisional reference. Every source counts equally here — this
    // node has no basis for ranking exchanges against each other.
    let mut samples: Vec<WeightedSample> = observations
        .iter()
        .map(|o| WeightedSample::new(o.price.raw(), 10_000))
        .collect();
    let provisional =
        weighted_median(&mut samples).ok_or_else(|| NodeError::InsufficientSources {
            feed: feed.clone(),
            available: 0,
            required: params.min_sources,
        })?;

    // Pass 2: filter, then re-median.
    let mut used = Vec::new();
    let mut discarded = Vec::new();
    for obs in observations {
        let dev = deviation_bps(obs.price.raw(), provisional);
        if dev > params.max_source_deviation_bps {
            discarded.push(DiscardedSource {
                name: obs.source.clone(),
                price: obs.price,
                deviation_bps: dev,
                reason: "deviates too far from the cross-source median",
            });
        } else {
            used.push(ContributingSource {
                name: obs.source.clone(),
                price: obs.price,
                deviation_bps: dev,
                observed_at: obs.observed_at,
            });
        }
    }

    if used.len() < params.min_sources {
        return Err(NodeError::InsufficientSources {
            feed: feed.clone(),
            available: used.len(),
            required: params.min_sources,
        });
    }

    let mut surviving: Vec<WeightedSample> = used
        .iter()
        .map(|s| WeightedSample::new(s.price.raw(), 10_000))
        .collect();
    let price = weighted_median(&mut surviving).expect("non-empty, non-zero weight");

    let values: Vec<i128> = used.iter().map(|s| s.price.raw()).collect();
    let lo = values.iter().min().copied().unwrap_or(price);
    let hi = values.iter().max().copied().unwrap_or(price);
    let spread_bps = deviation_bps(hi, lo);
    let sd = stddev(&values).unwrap_or(0);

    // Recompute each survivor's deviation against the final price, not the
    // provisional one, so the recorded numbers describe what was published.
    for s in &mut used {
        s.deviation_bps = deviation_bps(s.price.raw(), price);
    }

    Ok(Aggregated {
        feed: feed.clone(),
        price: Price::from_raw(price),
        spread_bps,
        stddev: Price::from_raw(sd),
        used,
        discarded,
    })
}

impl Aggregated {
    /// The timestamp to sign: the oldest observation that actually contributed
    /// to the price, never later than `ledger_time`.
    ///
    /// The oldest rather than the newest, because a consumer's freshness check
    /// has to be answerable by the weakest input, not the strongest — one fast
    /// venue must not make four stale ones look current.
    ///
    /// *Survivors only*, though. A source discarded as an outlier is by
    /// definition not part of what was published, and a venue that has frozen
    /// is discarded for a wrong price and stale in the same breath. Letting it
    /// drag the signed timestamp backwards understates the freshness of a price
    /// it did not contribute to, and past the aggregator's `max_staleness` it
    /// costs the node the whole submission — a rejected transaction, a burned
    /// nonce and a missed round, for the age of data the round never used.
    ///
    /// With no survivors at all there is nothing to date, so the caller's
    /// ledger time stands; `aggregate` never returns such a result.
    pub fn observed_at(&self, ledger_time: u64) -> u64 {
        self.used
            .iter()
            .map(|s| s.observed_at.timestamp().max(0) as u64)
            .min()
            .unwrap_or(ledger_time)
            .min(ledger_time)
    }
}

/// Confidence half-width to publish, in basis points.
///
/// The configured value is a floor, not a constant: when the surviving sources
/// disagree more than usual the node says so rather than claiming a precision
/// it does not have. Consumers use this to widen their own safety margins, so
/// understating it is the dangerous direction.
pub fn confidence_bps(agg: &Aggregated, configured_floor: u32) -> u32 {
    let observed = agg.spread_bps / 2;
    configured_floor
        .max(observed)
        .min(aphelion_core::BPS_DENOMINATOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn obs(source: &str, price: &str) -> Observation {
        Observation {
            feed: FeedId::new("BTC_USD").unwrap(),
            source: source.into(),
            price: Price::parse_decimal(price).unwrap(),
            observed_at: Utc::now(),
            received_at: Utc::now(),
        }
    }

    /// As `obs`, but dated: `seen_at` is a unix timestamp.
    fn obs_at(source: &str, price: &str, seen_at: i64) -> Observation {
        let at = DateTime::from_timestamp(seen_at, 0).unwrap();
        Observation {
            observed_at: at,
            received_at: at,
            ..obs(source, price)
        }
    }

    fn params() -> AggregationParams {
        AggregationParams {
            min_sources: 2,
            max_source_deviation_bps: 1_000, // 10%
        }
    }

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    #[test]
    fn agreeing_sources_produce_their_median() {
        let obs = vec![
            obs("binance", "64231.50"),
            obs("kraken", "64231.55"),
            obs("coinbase", "64231.60"),
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.price.to_string(), "64231.55000000");
        assert_eq!(agg.used.len(), 3);
        assert!(agg.discarded.is_empty());
    }

    #[test]
    fn a_broken_exchange_is_discarded_with_a_reason() {
        let obs = vec![
            obs("binance", "64231.50"),
            obs("kraken", "64231.60"),
            obs("coinbase", "1.00"), // stale placeholder
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.used.len(), 2);
        assert_eq!(agg.discarded.len(), 1);
        assert_eq!(agg.discarded[0].name, "coinbase");
        assert!(agg.price.to_f64() > 60_000.0);
    }

    #[test]
    fn refuses_to_publish_when_filtering_leaves_too_few_sources() {
        // Two sources, wildly apart: the median sits between them, both are
        // 50% away, both get dropped. Signing either would be a coin flip.
        let obs = vec![obs("binance", "64000.00"), obs("kraken", "32000.00")];
        let err = aggregate(&feed(), &obs, params()).unwrap_err();
        assert!(
            matches!(err, NodeError::InsufficientSources { .. }),
            "{err}"
        );
    }

    #[test]
    fn refuses_a_single_source_outright() {
        let err = aggregate(&feed(), &[obs("binance", "64231.50")], params()).unwrap_err();
        assert!(matches!(
            err,
            NodeError::InsufficientSources { available: 1, .. }
        ));
    }

    #[test]
    fn two_honest_sources_outvote_one_liar_even_when_the_liar_is_first() {
        let obs = vec![
            obs("coinbase", "0.01"),
            obs("binance", "64231.50"),
            obs("kraken", "64231.60"),
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.discarded.len(), 1);
        assert_eq!(agg.price.to_string(), "64231.55000000");
    }

    #[test]
    fn spread_and_stddev_describe_the_survivors_only() {
        let obs = vec![
            obs("binance", "100.00"),
            obs("kraken", "101.00"),
            obs("coinbase", "1000.00"), // discarded
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.used.len(), 2);
        // 1% spread between 100 and 101.
        assert_eq!(agg.spread_bps, 100);
    }

    #[test]
    fn confidence_widens_when_sources_disagree() {
        let tight = vec![obs("binance", "100.00"), obs("kraken", "100.01")];
        let wide = vec![obs("binance", "100.00"), obs("kraken", "104.00")];

        let tight = aggregate(&feed(), &tight, params()).unwrap();
        let wide = aggregate(&feed(), &wide, params()).unwrap();

        assert_eq!(
            confidence_bps(&tight, 50),
            50,
            "floor applies when sources agree"
        );
        assert!(
            confidence_bps(&wide, 50) > 50,
            "confidence must widen when sources disagree"
        );
    }

    #[test]
    fn aggregation_is_deterministic_regardless_of_input_order() {
        let a = vec![
            obs("binance", "64231.50"),
            obs("kraken", "64231.55"),
            obs("coinbase", "64231.60"),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(
            aggregate(&feed(), &a, params()).unwrap().price,
            aggregate(&feed(), &b, params()).unwrap().price
        );
    }

    // The signed timestamp.

    #[test]
    fn the_signed_timestamp_is_the_oldest_contributing_source() {
        // One fast venue must not make a round look fresher than its inputs.
        let obs = vec![
            obs_at("binance", "100.00", 1_000),
            obs_at("kraken", "100.01", 940),
            obs_at("coinbase", "100.02", 970),
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.observed_at(2_000), 940);
    }

    #[test]
    fn a_discarded_source_does_not_age_the_signed_timestamp() {
        // A frozen venue is wrong and stale in the same breath: it reports a
        // price from an hour ago and gets discarded for it. Dating the round
        // by that observation would understate the freshness of a price it did
        // not contribute to — and past the aggregator's `max_staleness` it
        // costs the node the whole submission.
        let obs = vec![
            obs_at("binance", "100.00", 1_000),
            obs_at("kraken", "100.01", 990),
            obs_at("coinbase", "1.00", 1_000 - 3_600),
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();

        assert_eq!(agg.discarded.len(), 1, "the frozen venue is discarded");
        assert_eq!(
            agg.observed_at(2_000),
            990,
            "the timestamp dates the survivors, not the venue that was dropped"
        );
    }

    #[test]
    fn the_signed_timestamp_never_runs_ahead_of_the_ledger() {
        // A node whose clock is fast would otherwise sign a timestamp in the
        // ledger's future, which the aggregator rejects as drift.
        let obs = vec![
            obs_at("binance", "100.00", 5_000),
            obs_at("kraken", "100.01", 5_000),
        ];
        let agg = aggregate(&feed(), &obs, params()).unwrap();
        assert_eq!(agg.observed_at(1_000), 1_000);
    }
}
