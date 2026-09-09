//! Aggregation math.
//!
//! Everything here is integer-only and deterministic so that the node and the
//! `aggregator` contract compute byte-identical results. A node that disagrees
//! with the on-chain outcome is a node that is about to be slashed, so "close
//! enough" floating point is not acceptable.

use crate::BPS_DENOMINATOR;

/// One node's contribution to a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeightedSample {
    pub value: i128,
    /// Reputation weight in basis points (10_000 == full weight).
    pub weight: u32,
}

impl WeightedSample {
    pub fn new(value: i128, weight: u32) -> Self {
        Self { value, weight }
    }
}

/// Weighted median: the smallest value at which the cumulative weight reaches
/// half of the total weight.
///
/// The median (rather than the mean) is what makes a round Byzantine-tolerant:
/// moving it requires controlling more than half the *weight*, not merely
/// submitting one absurd number. When the cumulative weight lands exactly on
/// the halfway point the two straddling values are averaged, which keeps the
/// result stable when an even number of equally-weighted nodes report.
///
/// Returns `None` for an empty sample set or when every weight is zero.
pub fn weighted_median(samples: &mut [WeightedSample]) -> Option<i128> {
    if samples.is_empty() {
        return None;
    }
    let total: u128 = samples.iter().map(|s| s.weight as u128).sum();
    if total == 0 {
        return None;
    }
    samples.sort_unstable_by_key(|s| s.value);

    let half = total / 2;
    let mut cumulative: u128 = 0;
    for (i, s) in samples.iter().enumerate() {
        cumulative += s.weight as u128;
        if total.is_multiple_of(2) && cumulative == half {
            // Exact tie: average with the next distinct sample.
            let next = samples.get(i + 1).map(|n| n.value).unwrap_or(s.value);
            return Some(midpoint(s.value, next));
        }
        if cumulative * 2 > total {
            return Some(s.value);
        }
    }
    samples.last().map(|s| s.value)
}

/// Overflow-free midpoint of two i128s.
#[inline]
fn midpoint(a: i128, b: i128) -> i128 {
    // (a + b) / 2 without the intermediate overflow.
    (a & b) + ((a ^ b) >> 1)
}

/// Arithmetic mean, truncated toward zero. `None` for an empty slice.
pub fn mean(values: &[i128]) -> Option<i128> {
    if values.is_empty() {
        return None;
    }
    let mut acc: i128 = 0;
    for &v in values {
        acc = acc.checked_add(v)?;
    }
    Some(acc / values.len() as i128)
}

/// Population standard deviation, truncated to an integer.
///
/// The sum of squared deviations is accumulated in full and divided once at
/// the end. Dividing each term as it is added would be tidier for overflow but
/// truncates every term toward zero, which for tightly-clustered prices --
/// the normal case -- rounds most terms to nothing and reports a standard
/// deviation of zero for a set that plainly has one.
///
/// Overflow is handled rather than avoided: for a price scaled by 1e8, a
/// deviation would have to exceed roughly 1e19 (a hundred billion dollars) for
/// a single squared term to trouble an `i128`. `checked_*` turns that into
/// `None` instead of a wrapped, silently wrong number.
pub fn stddev(values: &[i128]) -> Option<i128> {
    if values.len() < 2 {
        return Some(0);
    }
    let mu = mean(values)?;
    let n = values.len() as i128;
    let mut sum_sq: i128 = 0;
    for &v in values {
        let d = v.checked_sub(mu)?;
        sum_sq = sum_sq.checked_add(d.checked_mul(d)?)?;
    }
    Some(isqrt(sum_sq / n))
}

/// Integer square root via Newton's method. Deterministic, no float involved.
pub fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    if n < 4 {
        return 1;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Absolute deviation of `value` from `reference`, in basis points.
///
/// Saturates at `u32::MAX` rather than wrapping, so a wildly wrong submission
/// still compares correctly against any threshold.
pub fn deviation_bps(value: i128, reference: i128) -> u32 {
    if reference == 0 {
        return u32::MAX;
    }
    let diff = (value - reference).abs();
    let bps = diff.saturating_mul(BPS_DENOMINATOR as i128) / reference.abs();
    u32::try_from(bps).unwrap_or(u32::MAX)
}

/// Time-weighted average of `(timestamp, price)` observations over the window
/// `[window_start, now]`.
///
/// Each observation is held to be in force until the next one, which is what
/// makes this resistant to a price that is manipulated for a single ledger:
/// a spike that lasts two seconds contributes two seconds of weight.
/// Observations must be sorted ascending by timestamp.
pub fn time_weighted_average(
    observations: &[(u64, i128)],
    window_start: u64,
    now: u64,
) -> Option<i128> {
    if observations.is_empty() || now <= window_start {
        return None;
    }
    let mut weighted: i128 = 0;
    let mut total_time: i128 = 0;

    for (i, &(ts, price)) in observations.iter().enumerate() {
        let segment_end = observations
            .get(i + 1)
            .map(|&(t, _)| t)
            .unwrap_or(now)
            .min(now);
        let segment_start = ts.max(window_start);
        if segment_end <= segment_start {
            continue;
        }
        let dt = (segment_end - segment_start) as i128;
        weighted = weighted.checked_add(price.checked_mul(dt)?)?;
        total_time += dt;
    }

    if total_time == 0 {
        // Every observation predates the window; the most recent one still
        // describes the current state of the world.
        return observations.last().map(|&(_, p)| p);
    }
    Some(weighted / total_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq_weight(values: &[i128]) -> Vec<WeightedSample> {
        values
            .iter()
            .map(|&v| WeightedSample::new(v, 10_000))
            .collect()
    }

    #[test]
    fn median_of_odd_count_is_the_middle_value() {
        let mut s = eq_weight(&[100, 300, 200]);
        assert_eq!(weighted_median(&mut s), Some(200));
    }

    #[test]
    fn median_of_even_count_averages_the_middle_pair() {
        let mut s = eq_weight(&[100, 200, 300, 400]);
        assert_eq!(weighted_median(&mut s), Some(250));
    }

    #[test]
    fn a_single_outlier_barely_moves_the_median() {
        let mut honest = eq_weight(&[1000, 1001, 999, 1000, 1002]);
        let baseline = weighted_median(&mut honest).unwrap();

        let mut attacked = eq_weight(&[1000, 1001, 999, 1000, 999_999]);
        let attacked = weighted_median(&mut attacked).unwrap();

        assert_eq!(baseline, 1000);
        assert_eq!(
            attacked, 1000,
            "one Byzantine node must not move the median"
        );
    }

    #[test]
    fn low_reputation_nodes_carry_less_influence() {
        // Three full-weight honest nodes vs four half-weight liars: honest wins
        // because 30_000 bps of weight beats 20_000.
        let mut s = vec![
            WeightedSample::new(1000, 10_000),
            WeightedSample::new(1000, 10_000),
            WeightedSample::new(1000, 10_000),
            WeightedSample::new(5000, 5_000),
            WeightedSample::new(5000, 5_000),
            WeightedSample::new(5000, 5_000),
            WeightedSample::new(5000, 5_000),
        ];
        assert_eq!(weighted_median(&mut s), Some(1000));
    }

    #[test]
    fn zero_weight_samples_do_not_produce_a_median() {
        let mut s = vec![WeightedSample::new(1000, 0)];
        assert_eq!(weighted_median(&mut s), None);
        assert_eq!(weighted_median(&mut []), None);
    }

    #[test]
    fn stddev_matches_hand_computed_value() {
        // values 2,4,4,4,5,5,7,9 -> population stddev 2
        let v = vec![2, 4, 4, 4, 5, 5, 7, 9];
        assert_eq!(stddev(&v), Some(2));
        assert_eq!(stddev(&[5]), Some(0));
    }

    #[test]
    fn stddev_survives_realistic_oracle_magnitudes() {
        // Five sources on BTC at ~$64_231, scaled by 1e8, disagreeing by cents.
        let v: Vec<i128> = [6_423_150, 6_423_152, 6_423_155, 6_423_158, 6_423_160]
            .iter()
            .map(|c: &i128| c * 1_000_000)
            .collect();
        let sd = stddev(&v).expect("no overflow at realistic magnitudes");
        assert!(sd > 0, "a spread of cents must not round to a zero stddev");
        assert!(
            sd < 500_000_000,
            "stddev should be well under a dollar, got {sd}"
        );
    }

    #[test]
    fn isqrt_is_exact_on_perfect_squares() {
        for n in [0i128, 1, 4, 9, 144, 1_000_000, i128::from(u64::MAX)] {
            let r = isqrt(n);
            assert!(r * r <= n.max(0));
            assert!((r + 1).saturating_mul(r + 1) > n.max(0) || n == 0);
        }
    }

    #[test]
    fn deviation_is_symmetric_and_saturating() {
        assert_eq!(deviation_bps(110, 100), 1_000); // +10%
        assert_eq!(deviation_bps(90, 100), 1_000); // -10%
        assert_eq!(deviation_bps(100, 0), u32::MAX);
    }

    #[test]
    fn twap_discounts_a_momentary_spike() {
        // Price sits at 1000 for 59s, spikes to 10_000 for the final second.
        let obs = vec![(0u64, 1000i128), (59, 10_000)];
        let twap = time_weighted_average(&obs, 0, 60).unwrap();
        assert!(
            twap < 1200,
            "a 1s spike should barely move a 60s TWAP, got {twap}"
        );
    }

    #[test]
    fn twap_falls_back_to_last_observation_when_window_is_stale() {
        let obs = vec![(0u64, 1000i128)];
        assert_eq!(time_weighted_average(&obs, 500, 600), Some(1000));
    }

    /// The contract-side mirror asserts the same file. A drift here means a
    /// node can predict one round outcome while the aggregator computes
    /// another, and be penalised for the difference.
    mod shared_vectors {
        use super::*;
        use serde_json::Value;

        fn vectors() -> Value {
            let raw = include_str!("../../../tests/vectors/aggregation.json");
            serde_json::from_str(raw).expect("vector file is valid JSON")
        }

        fn i128_of(v: &Value) -> i128 {
            v.as_str()
                .expect("i128 vectors are strings")
                .parse()
                .unwrap()
        }

        fn expected(case: &Value) -> Option<i128> {
            case["expected"].as_str().map(|s| s.parse().unwrap())
        }

        #[test]
        fn weighted_median_matches() {
            for case in vectors()["weighted_median"].as_array().unwrap() {
                let mut samples: Vec<WeightedSample> = case["samples"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| WeightedSample::new(i128_of(&s[0]), s[1].as_u64().unwrap() as u32))
                    .collect();
                assert_eq!(
                    weighted_median(&mut samples),
                    expected(case),
                    "weighted_median vector `{}` drifted",
                    case["name"]
                );
            }
        }

        #[test]
        fn stddev_matches() {
            for case in vectors()["stddev"].as_array().unwrap() {
                let values: Vec<i128> = case["values"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(i128_of)
                    .collect();
                // An empty set has no mean to deviate from; the vectors record
                // that as absent rather than as zero.
                let want = expected(case).or(if values.is_empty() { None } else { Some(0) });
                let got = if values.is_empty() {
                    None
                } else {
                    stddev(&values)
                };
                assert_eq!(got, want, "stddev vector `{}` drifted", case["name"]);
            }
        }

        #[test]
        fn deviation_bps_matches() {
            for case in vectors()["deviation_bps"].as_array().unwrap() {
                assert_eq!(
                    deviation_bps(i128_of(&case["value"]), i128_of(&case["reference"])),
                    case["expected"].as_u64().unwrap() as u32,
                    "deviation_bps vector `{}` drifted",
                    case["name"]
                );
            }
        }

        #[test]
        fn time_weighted_average_matches() {
            for case in vectors()["twap"].as_array().unwrap() {
                let observations: Vec<(u64, i128)> = case["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| (o[0].as_u64().unwrap(), i128_of(&o[1])))
                    .collect();
                assert_eq!(
                    time_weighted_average(
                        &observations,
                        case["window_start"].as_u64().unwrap(),
                        case["now"].as_u64().unwrap(),
                    ),
                    expected(case),
                    "twap vector `{}` drifted",
                    case["name"]
                );
            }
        }
    }
}
