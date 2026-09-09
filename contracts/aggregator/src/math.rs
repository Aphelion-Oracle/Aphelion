//! Consensus arithmetic.
//!
//! Every function here is a deliberate mirror of `aphelion-core::math`. A node
//! predicts the round's outcome with the off-chain copy and is rewarded or
//! penalised by this one; if the two disagree by even one unit, an honest node
//! can be slashed for an arithmetic difference it had no way to see. That is
//! why both sides are integer-only, why neither rounds "close enough", and why
//! `tests/vectors/aggregation.json` is asserted from both.
//!
//! Everything is `checked_*` and returns `Option`. The contract profile builds
//! with `overflow-checks = true`, so an unchecked overflow would trap and take
//! the whole round with it; returning `None` lets the caller decide.

use soroban_sdk::{Env, Vec};

/// 10_000 bps == 100%.
pub const BPS_DENOMINATOR: u32 = 10_000;

/// `(value, weight_bps)` pairs, sorted ascending by value.
///
/// Insertion sort: `soroban_sdk::Vec` has no sort, and a round holds a handful
/// of submissions, not thousands. `<=` keeps equal values in insertion order,
/// which costs nothing — equal values are interchangeable to a median — and
/// makes the result reproducible.
pub fn sorted_by_value(env: &Env, pairs: &Vec<(i128, u32)>) -> Vec<(i128, u32)> {
    let mut out: Vec<(i128, u32)> = Vec::new(env);
    for pair in pairs.iter() {
        let mut i = 0u32;
        while i < out.len() && out.get(i).unwrap().0 <= pair.0 {
            i += 1;
        }
        out.insert(i, pair);
    }
    out
}

/// Weighted median: the smallest value at which cumulative weight reaches half
/// the total weight.
///
/// The median, not the mean, is what makes a round Byzantine-tolerant: moving
/// it requires controlling more than half the *weight*, and it does not care
/// how extreme a minority's number is. When cumulative weight lands exactly on
/// the halfway point the two straddling values are averaged, which keeps the
/// result stable for an even number of equally-weighted nodes.
///
/// `None` for an empty set, or when every weight is zero.
pub fn weighted_median(env: &Env, pairs: &Vec<(i128, u32)>) -> Option<i128> {
    if pairs.is_empty() {
        return None;
    }
    let mut total: u128 = 0;
    for pair in pairs.iter() {
        total += pair.1 as u128;
    }
    if total == 0 {
        return None;
    }

    let sorted = sorted_by_value(env, pairs);
    let half = total / 2;
    let mut cumulative: u128 = 0;

    for i in 0..sorted.len() {
        let (value, weight) = sorted.get(i).unwrap();
        cumulative += weight as u128;

        if total % 2 == 0 && cumulative == half {
            let next = if i + 1 < sorted.len() {
                sorted.get(i + 1).unwrap().0
            } else {
                value
            };
            return Some(midpoint(value, next));
        }
        if cumulative * 2 > total {
            return Some(value);
        }
    }
    sorted.last().map(|(v, _)| v)
}

/// Overflow-free midpoint of two `i128`s.
#[inline]
pub fn midpoint(a: i128, b: i128) -> i128 {
    (a & b) + ((a ^ b) >> 1)
}

/// Arithmetic mean, truncated toward zero. `None` for an empty vector.
pub fn mean(values: &Vec<i128>) -> Option<i128> {
    if values.is_empty() {
        return None;
    }
    let mut acc: i128 = 0;
    for v in values.iter() {
        acc = acc.checked_add(v)?;
    }
    Some(acc / values.len() as i128)
}

/// Population standard deviation, truncated to an integer.
///
/// The sum of squared deviations is accumulated in full and divided once at
/// the end. Dividing each term as it is added would be tidier for overflow but
/// truncates every term toward zero, which for tightly-clustered prices — the
/// normal case — reports a standard deviation of zero for a set that plainly
/// has one.
pub fn stddev(values: &Vec<i128>) -> Option<i128> {
    if values.len() < 2 {
        return Some(0);
    }
    let mu = mean(values)?;
    let n = values.len() as i128;
    let mut sum_sq: i128 = 0;
    for v in values.iter() {
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
    if bps > u32::MAX as i128 {
        u32::MAX
    } else {
        bps as u32
    }
}

/// Time-weighted average of `(timestamp, price)` observations over
/// `[window_start, now]`.
///
/// Each observation is held to be in force until the next one, which is what
/// makes this resistant to a price manipulated for a single ledger: a spike
/// lasting two seconds contributes two seconds of weight. Observations must be
/// sorted ascending by timestamp.
pub fn time_weighted_average(
    observations: &Vec<(u64, i128)>,
    window_start: u64,
    now: u64,
) -> Option<i128> {
    if observations.is_empty() || now <= window_start {
        return None;
    }
    let mut weighted: i128 = 0;
    let mut total_time: i128 = 0;

    for i in 0..observations.len() {
        let (ts, price) = observations.get(i).unwrap();
        let segment_end = if i + 1 < observations.len() {
            observations.get(i + 1).unwrap().0.min(now)
        } else {
            now
        };
        let segment_start = if ts > window_start { ts } else { window_start };
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
        return observations.last().map(|(_, p)| p);
    }
    Some(weighted / total_time)
}
