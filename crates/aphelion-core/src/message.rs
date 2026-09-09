//! The canonical signing payload.
//!
//! # Why a hand-rolled layout
//!
//! The `aggregator` contract must reconstruct, byte for byte, the message that
//! a node signed — otherwise `ed25519_verify` fails and the submission is
//! dropped. Soroban contracts cannot depend on `serde`, so rather than share a
//! codec we share a *specification*: this module is the reference
//! implementation, `contracts/aggregator/src/message.rs` is the on-chain
//! mirror, and `tests/vectors/price_message.json` is asserted by both.
//!
//! # Layout (all integers big-endian, total 117 bytes)
//!
//! ```text
//! offset  len  field
//!      0   17  domain separator, ASCII "APHELION_PRICE_V1"
//!     17   32  aggregator contract id (raw 32-byte Stellar contract id)
//!     49   32  feed id, ASCII, right-padded with 0x00
//!     81   16  price, i128, scaled by 1e8
//!     97    8  observation timestamp, u64 unix seconds
//!    105    4  confidence interval, u32 basis points
//!    109    8  nonce, u64, strictly increasing per (node, feed)
//! ```
//!
//! Each field earns its place:
//!
//! * **domain separator** — a signature over an Aphelion price can never be
//!   replayed as a signature over anything else the same key signs.
//! * **contract id** — binds the signature to one deployment, so a testnet
//!   submission cannot be replayed onto mainnet.
//! * **feed id** — prevents a valid BTC quote being replayed as an XLM quote.
//! * **timestamp** — lets the contract reject stale observations.
//! * **nonce** — prevents replay of a still-fresh observation within its
//!   staleness window.

use crate::feed::{FeedId, FEED_ID_PADDED_LEN};
use crate::price::Price;

/// ASCII domain separator, 17 bytes.
pub const DOMAIN_SEPARATOR: &[u8; 17] = b"APHELION_PRICE_V1";

const OFF_DOMAIN: usize = 0;
const OFF_CONTRACT: usize = OFF_DOMAIN + 17;
const OFF_FEED: usize = OFF_CONTRACT + 32;
const OFF_PRICE: usize = OFF_FEED + FEED_ID_PADDED_LEN;
const OFF_TIMESTAMP: usize = OFF_PRICE + 16;
const OFF_CONFIDENCE: usize = OFF_TIMESTAMP + 8;
const OFF_NONCE: usize = OFF_CONFIDENCE + 4;

/// Total length of the signing payload.
pub const MESSAGE_LEN: usize = OFF_NONCE + 8; // 117

/// One node's observation of one feed, in the exact form that is signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceMessage {
    /// Raw 32-byte contract id of the aggregator this submission targets.
    pub aggregator: [u8; 32],
    pub feed: FeedId,
    pub price: Price,
    /// Unix seconds at which the underlying observation was made — not when
    /// the message was signed.
    pub timestamp: u64,
    /// Half-width of the node's confidence interval, in basis points.
    pub confidence_bps: u32,
    pub nonce: u64,
}

impl PriceMessage {
    /// Serialise into the canonical payload.
    pub fn to_bytes(&self) -> [u8; MESSAGE_LEN] {
        let mut buf = [0u8; MESSAGE_LEN];
        buf[OFF_DOMAIN..OFF_CONTRACT].copy_from_slice(DOMAIN_SEPARATOR);
        buf[OFF_CONTRACT..OFF_FEED].copy_from_slice(&self.aggregator);
        buf[OFF_FEED..OFF_PRICE].copy_from_slice(&self.feed.to_padded_bytes());
        buf[OFF_PRICE..OFF_TIMESTAMP].copy_from_slice(&self.price.raw().to_be_bytes());
        buf[OFF_TIMESTAMP..OFF_CONFIDENCE].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[OFF_CONFIDENCE..OFF_NONCE].copy_from_slice(&self.confidence_bps.to_be_bytes());
        buf[OFF_NONCE..MESSAGE_LEN].copy_from_slice(&self.nonce.to_be_bytes());
        buf
    }

    /// Hex encoding of [`Self::to_bytes`], used by the test vectors and by
    /// `aphelion-node debug sign` when reproducing a rejected submission.
    pub fn to_hex(&self) -> String {
        hex::encode(self.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn sample() -> PriceMessage {
        PriceMessage {
            aggregator: [0x11; 32],
            feed: FeedId::new("BTC_USD").unwrap(),
            price: Price::parse_decimal("64231.55").unwrap(),
            timestamp: 1_735_689_600,
            confidence_bps: 25,
            nonce: 42,
        }
    }

    #[test]
    fn layout_is_the_documented_length() {
        assert_eq!(MESSAGE_LEN, 117);
        assert_eq!(sample().to_bytes().len(), MESSAGE_LEN);
    }

    #[test]
    fn fields_land_at_the_documented_offsets() {
        let m = sample();
        let b = m.to_bytes();
        assert_eq!(&b[0..17], DOMAIN_SEPARATOR);
        assert_eq!(&b[17..49], &[0x11; 32]);
        assert_eq!(&b[49..56], b"BTC_USD");
        assert_eq!(&b[56..81], &[0u8; 25]);
        assert_eq!(
            i128::from_be_bytes(b[81..97].try_into().unwrap()),
            m.price.raw()
        );
        assert_eq!(
            u64::from_be_bytes(b[97..105].try_into().unwrap()),
            m.timestamp
        );
        assert_eq!(u32::from_be_bytes(b[105..109].try_into().unwrap()), 25);
        assert_eq!(u64::from_be_bytes(b[109..117].try_into().unwrap()), 42);
    }

    #[test]
    fn changing_any_field_changes_the_payload() {
        let base = sample().to_bytes();
        let mut other = sample();
        other.nonce += 1;
        assert_ne!(base, other.to_bytes());
        let mut other = sample();
        other.feed = FeedId::new("ETH_USD").unwrap();
        assert_ne!(base, other.to_bytes());
        let mut other = sample();
        other.aggregator = [0x22; 32];
        assert_ne!(base, other.to_bytes());
    }

    /// The contract-side mirror asserts the same file. If this test fails, the
    /// two implementations have drifted and every submission would be rejected
    /// on chain.
    #[test]
    fn matches_the_shared_test_vectors() {
        let raw = include_str!("../../../tests/vectors/price_message.json");
        let vectors: Value = serde_json::from_str(raw).expect("vector file is valid JSON");
        let cases = vectors["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty());

        for case in cases {
            let msg = PriceMessage {
                aggregator: hex::decode(case["aggregator_hex"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap(),
                feed: FeedId::new(case["feed"].as_str().unwrap()).unwrap(),
                price: Price::from_raw(case["price_raw"].as_str().unwrap().parse().unwrap()),
                timestamp: case["timestamp"].as_u64().unwrap(),
                confidence_bps: case["confidence_bps"].as_u64().unwrap() as u32,
                nonce: case["nonce"].as_u64().unwrap(),
            };
            assert_eq!(
                msg.to_hex(),
                case["message_hex"].as_str().unwrap(),
                "vector `{}` drifted",
                case["name"]
            );
        }
    }
}
