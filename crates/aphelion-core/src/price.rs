//! Fixed-point price representation.
//!
//! Every price that crosses a network boundary — a node's signed submission, a
//! row in `raw_prices`, the aggregated value stored on chain — is an [`i128`]
//! scaled by [`PRICE_SCALE`]. Floating point is used only at the very edge,
//! when parsing an exchange's JSON, and is converted immediately.

use core::fmt;
use serde::{Deserialize, Serialize};

/// Number of decimal places carried by every Aphelion price.
///
/// Eight was chosen because it covers both ends of the range we care about:
/// BTC at ~$100_000 needs 6 integer digits, and a long-tail Stellar asset at
/// $0.00000042 still keeps two significant figures.
pub const PRICE_DECIMALS: u32 = 8;

/// `10^PRICE_DECIMALS`.
pub const PRICE_SCALE: i128 = 100_000_000;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PriceError {
    #[error("price is not finite")]
    NotFinite,
    #[error("price must be strictly positive, got {0}")]
    NotPositive(f64),
    #[error("price {0} overflows the fixed-point range")]
    Overflow(f64),
    #[error("could not parse `{0}` as a decimal price")]
    Parse(String),
}

/// A price quoted in USD, scaled by [`PRICE_SCALE`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(i128);

impl Price {
    pub const ZERO: Price = Price(0);

    /// Wrap an already-scaled integer.
    #[inline]
    pub const fn from_raw(raw: i128) -> Self {
        Price(raw)
    }

    /// The underlying scaled integer, i.e. what goes on chain.
    #[inline]
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// Convert from an exchange-supplied float.
    ///
    /// Rejects NaN/infinity and non-positive values rather than silently
    /// producing a price of zero, which downstream would look like a real
    /// quote and could liquidate somebody.
    pub fn from_f64(v: f64) -> Result<Self, PriceError> {
        if !v.is_finite() {
            return Err(PriceError::NotFinite);
        }
        if v <= 0.0 {
            return Err(PriceError::NotPositive(v));
        }
        let scaled = (v * PRICE_SCALE as f64).round();
        if scaled > i128::MAX as f64 {
            return Err(PriceError::Overflow(v));
        }
        Ok(Price(scaled as i128))
    }

    /// Parse a decimal string such as `"64231.55"` without going through
    /// binary floating point, so that exchange payloads round-trip exactly.
    pub fn parse_decimal(s: &str) -> Result<Self, PriceError> {
        let s = s.trim();
        let err = || PriceError::Parse(s.to_string());
        let (int_part, frac_part) = match s.split_once('.') {
            Some((i, f)) => (i, f),
            None => (s, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(err());
        }
        if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
            return Err(err());
        }
        let int_val: i128 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().map_err(|_| err())?
        };
        // Truncate or right-pad the fraction to exactly PRICE_DECIMALS digits.
        let mut frac_digits = frac_part.as_bytes().to_vec();
        frac_digits.resize(PRICE_DECIMALS as usize, b'0');
        let frac_val: i128 = frac_digits
            .iter()
            .take(PRICE_DECIMALS as usize)
            .try_fold(0i128, |acc, &b| {
                acc.checked_mul(10)?.checked_add((b - b'0') as i128)
            })
            .ok_or_else(err)?;

        let raw = int_val
            .checked_mul(PRICE_SCALE)
            .and_then(|v| v.checked_add(frac_val))
            .ok_or_else(err)?;
        if raw <= 0 {
            return Err(PriceError::NotPositive(0.0));
        }
        Ok(Price(raw))
    }

    /// Lossy conversion back to a float, for logs and dashboards only.
    #[inline]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / PRICE_SCALE as f64
    }

    #[inline]
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = PRICE_SCALE as u128;
        write!(
            f,
            "{sign}{}.{:0width$}",
            abs / scale,
            abs % scale,
            width = PRICE_DECIMALS as usize
        )
    }
}

impl fmt::Debug for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Price({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_decimals() {
        assert_eq!(Price::parse_decimal("1").unwrap().raw(), PRICE_SCALE);
        assert_eq!(Price::parse_decimal("0.5").unwrap().raw(), PRICE_SCALE / 2);
        assert_eq!(Price::parse_decimal("64231.55").unwrap().raw(), 6_423_155_000_000);
    }

    #[test]
    fn truncates_excess_precision_rather_than_erroring() {
        // Binance occasionally quotes more decimals than we carry.
        assert_eq!(
            Price::parse_decimal("0.123456789").unwrap().raw(),
            12_345_678
        );
    }

    #[test]
    fn rejects_garbage_and_non_positive() {
        assert!(Price::parse_decimal("abc").is_err());
        assert!(Price::parse_decimal("").is_err());
        assert!(Price::parse_decimal("0").is_err());
        assert!(Price::parse_decimal("-1").is_err());
        assert_eq!(Price::from_f64(f64::NAN), Err(PriceError::NotFinite));
        assert!(Price::from_f64(0.0).is_err());
    }

    #[test]
    fn display_round_trips() {
        let p = Price::parse_decimal("64231.55").unwrap();
        assert_eq!(p.to_string(), "64231.55000000");
        assert_eq!(Price::parse_decimal(&p.to_string()).unwrap(), p);
    }
}
