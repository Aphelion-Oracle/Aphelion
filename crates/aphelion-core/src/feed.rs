//! Feed identifiers.
//!
//! A feed id is deliberately constrained to the character set accepted by
//! `soroban_sdk::Symbol` (`a-z`, `A-Z`, `0-9`, `_`) and to 32 bytes, so that
//! the same identifier can be used as a storage key on chain, a column value
//! in Postgres, and a path segment in the node's HTTP API without escaping.

use core::fmt;
use core::str::FromStr;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum feed id length, matching `soroban_sdk::Symbol`.
pub const FEED_ID_MAX_LEN: usize = 32;

/// Width of a feed id inside a signed message: fixed so the payload layout is
/// constant-size and offsets never depend on the symbol.
pub const FEED_ID_PADDED_LEN: usize = 32;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FeedIdError {
    #[error("feed id must not be empty")]
    Empty,
    #[error("feed id `{0}` exceeds {FEED_ID_MAX_LEN} bytes")]
    TooLong(String),
    #[error("feed id `{0}` contains a character outside [A-Za-z0-9_]")]
    InvalidChar(String),
}

/// A canonical feed identifier, e.g. `BTC_USD`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedId(String);

impl FeedId {
    pub fn new(s: impl Into<String>) -> Result<Self, FeedIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(FeedIdError::Empty);
        }
        if s.len() > FEED_ID_MAX_LEN {
            return Err(FeedIdError::TooLong(s));
        }
        if !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(FeedIdError::InvalidChar(s));
        }
        Ok(FeedId(s))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Fixed-width encoding used inside the signed payload: the ASCII symbol
    /// followed by zero padding out to [`FEED_ID_PADDED_LEN`].
    pub fn to_padded_bytes(&self) -> [u8; FEED_ID_PADDED_LEN] {
        let mut out = [0u8; FEED_ID_PADDED_LEN];
        out[..self.0.len()].copy_from_slice(self.0.as_bytes());
        out
    }
}

impl FromStr for FeedId {
    type Err = FeedIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FeedId::new(s)
    }
}

impl fmt::Display for FeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for FeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FeedId({})", self.0)
    }
}

impl Serialize for FeedId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FeedId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        FeedId::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_symbol_safe_ids() {
        assert!(FeedId::new("BTC_USD").is_ok());
        assert!(FeedId::new("XLM").is_ok());
    }

    #[test]
    fn rejects_ids_soroban_symbol_would_reject() {
        assert!(FeedId::new("BTC/USD").is_err());
        assert!(FeedId::new("BTC-USD").is_err());
        assert!(FeedId::new("").is_err());
        assert!(FeedId::new("x".repeat(33)).is_err());
    }

    #[test]
    fn padding_is_right_aligned_with_zeros() {
        let padded = FeedId::new("BTC").unwrap().to_padded_bytes();
        assert_eq!(&padded[..3], b"BTC");
        assert!(padded[3..].iter().all(|&b| b == 0));
    }
}
