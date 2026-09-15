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

    /// Recover the fields from a canonical payload.
    ///
    /// The inverse of [`Self::to_bytes`], and the reason it exists is not
    /// symmetry. A signature covers *bytes*; everything anybody says those bytes
    /// mean is a claim on top of them. Checking a signature and then reading the
    /// price out of the JSON next to it verifies nothing — the two can disagree,
    /// and a payload is 117 opaque bytes to anyone eyeballing it. Decoding is
    /// what closes that gap: the fields a verifier compares are the fields the
    /// signature was over.
    ///
    /// Strict, because a lenient decoder reopens it. A payload with the wrong
    /// domain separator is refused rather than read as a price, a
    /// non-positive price is refused rather than returned as one no node could
    /// have published, and the feed id must be padded exactly as
    /// [`FeedId::to_padded_bytes`] pads it.
    pub fn from_bytes(raw: &[u8]) -> Result<Self, MessageError> {
        if raw.len() != MESSAGE_LEN {
            return Err(MessageError::WrongLength(raw.len()));
        }
        if &raw[OFF_DOMAIN..OFF_CONTRACT] != DOMAIN_SEPARATOR.as_slice() {
            return Err(MessageError::WrongDomain);
        }

        let mut aggregator = [0u8; 32];
        aggregator.copy_from_slice(&raw[OFF_CONTRACT..OFF_FEED]);

        let mut feed_raw = [0u8; FEED_ID_PADDED_LEN];
        feed_raw.copy_from_slice(&raw[OFF_FEED..OFF_PRICE]);
        let feed = FeedId::from_padded_bytes(&feed_raw)?;

        let price = i128::from_be_bytes(
            raw[OFF_PRICE..OFF_TIMESTAMP]
                .try_into()
                .expect("16 bytes by construction"),
        );
        if price <= 0 {
            return Err(MessageError::NonPositivePrice(price));
        }

        Ok(Self {
            aggregator,
            feed,
            price: Price::from_raw(price),
            timestamp: u64::from_be_bytes(
                raw[OFF_TIMESTAMP..OFF_CONFIDENCE]
                    .try_into()
                    .expect("8 bytes by construction"),
            ),
            confidence_bps: u32::from_be_bytes(
                raw[OFF_CONFIDENCE..OFF_NONCE]
                    .try_into()
                    .expect("4 bytes by construction"),
            ),
            nonce: u64::from_be_bytes(
                raw[OFF_NONCE..MESSAGE_LEN]
                    .try_into()
                    .expect("8 bytes by construction"),
            ),
        })
    }

    /// [`Self::from_bytes`] over a hex string, which is how a payload travels
    /// in an evidence bundle or a bug report.
    pub fn from_hex(s: &str) -> Result<Self, MessageError> {
        let raw = hex::decode(s.trim()).map_err(|e| MessageError::NotHex(e.to_string()))?;
        Self::from_bytes(&raw)
    }
}

/// Why a payload could not be read as an Aphelion price message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MessageError {
    #[error("payload is not hex: {0}")]
    NotHex(String),
    #[error("payload is {0} bytes, expected {MESSAGE_LEN}")]
    WrongLength(usize),
    #[error(
        "payload does not begin with the Aphelion price domain separator; it may be a \
         signature over something else entirely"
    )]
    WrongDomain,
    #[error("payload carries a non-positive price ({0}), which no node could have published")]
    NonPositivePrice(i128),
    #[error("payload carries an invalid feed id: {0}")]
    Feed(#[from] crate::feed::FeedIdError),
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

    /// Every vector decodes back to the fields it was built from. The vectors
    /// are the shared specification, so this asserts the decoder against the
    /// same file the encoder and the contract are asserted against, rather than
    /// against the encoder alone — which would only prove the two agree.
    #[test]
    fn every_vector_decodes_back_to_its_fields() {
        let raw = include_str!("../../../tests/vectors/price_message.json");
        let vectors: Value = serde_json::from_str(raw).expect("vector file is valid JSON");

        for case in vectors["cases"].as_array().expect("cases array") {
            let name = case["name"].as_str().unwrap();
            let decoded = PriceMessage::from_hex(case["message_hex"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("vector `{name}` did not decode: {e}"));

            assert_eq!(
                decoded.feed.as_str(),
                case["feed"].as_str().unwrap(),
                "vector `{name}` feed"
            );
            assert_eq!(
                decoded.price.raw().to_string(),
                case["price_raw"].as_str().unwrap(),
                "vector `{name}` price"
            );
            assert_eq!(
                decoded.timestamp,
                case["timestamp"].as_u64().unwrap(),
                "vector `{name}` timestamp"
            );
            assert_eq!(
                decoded.nonce,
                case["nonce"].as_u64().unwrap(),
                "vector `{name}` nonce"
            );
            // And the round trip closes: re-encoding must give the same bytes.
            assert_eq!(decoded.to_hex(), case["message_hex"].as_str().unwrap());
        }
    }

    #[test]
    fn decoding_round_trips_the_sample() {
        let msg = sample();
        assert_eq!(PriceMessage::from_bytes(&msg.to_bytes()).unwrap(), msg);
    }

    /// A signature over some *other* Aphelion payload must not be readable as a
    /// price. This is the domain separator doing the job it exists for, checked
    /// from the decoding side.
    #[test]
    fn a_payload_from_another_domain_is_refused_rather_than_reinterpreted() {
        let mut raw = sample().to_bytes();
        raw[..17].copy_from_slice(b"APHELION_OTHER_V1");
        assert_eq!(
            PriceMessage::from_bytes(&raw),
            Err(MessageError::WrongDomain)
        );
    }

    #[test]
    fn a_truncated_payload_is_refused() {
        let raw = sample().to_bytes();
        assert_eq!(
            PriceMessage::from_bytes(&raw[..MESSAGE_LEN - 1]),
            Err(MessageError::WrongLength(MESSAGE_LEN - 1))
        );
    }

    /// No node can publish a non-positive price -- the database refuses it and
    /// so does the contract -- so a payload carrying one is a crafted payload,
    /// and reading it back as a `Price` would launder it into something that
    /// looks like evidence.
    #[test]
    fn a_non_positive_price_is_refused_rather_than_returned() {
        let mut raw = sample().to_bytes();
        raw[81..97].copy_from_slice(&0i128.to_be_bytes());
        assert_eq!(
            PriceMessage::from_bytes(&raw),
            Err(MessageError::NonPositivePrice(0))
        );

        raw[81..97].copy_from_slice(&(-1i128).to_be_bytes());
        assert_eq!(
            PriceMessage::from_bytes(&raw),
            Err(MessageError::NonPositivePrice(-1))
        );
    }

    /// Padding must be a suffix. Two byte strings that decode to one feed id
    /// would mean a signature over one could be presented as a signature over
    /// the other.
    #[test]
    fn a_feed_id_with_interior_padding_is_refused() {
        let mut raw = sample().to_bytes();
        let mut feed = [0u8; 32];
        feed[..3].copy_from_slice(b"BTC");
        feed[4..7].copy_from_slice(b"USD");
        raw[49..81].copy_from_slice(&feed);

        assert!(matches!(
            PriceMessage::from_bytes(&raw),
            Err(MessageError::Feed(_))
        ));
    }

    /// Nothing decodes to a feed id that `FeedId::new` would have rejected.
    #[test]
    fn a_feed_id_with_an_illegal_character_is_refused() {
        let mut raw = sample().to_bytes();
        raw[49] = b'-';
        assert!(matches!(
            PriceMessage::from_bytes(&raw),
            Err(MessageError::Feed(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// The beacon's two payloads
// ---------------------------------------------------------------------------
//
// Same arrangement as the price message above: this is the reference
// implementation and `contracts/randomness/src/message.rs` is the on-chain
// mirror. Neither depends on the other, and a disagreement surfaces as a
// commitment that will not open rather than as a difference of opinion.

/// ASCII domain separator for a beacon commitment's preimage, 18 bytes.
pub const COMMITMENT_DOMAIN: &[u8; 18] = b"APHELION_RANDOM_V1";

/// ASCII domain separator for the signature over a commitment, 18 bytes.
pub const COMMIT_SIGNATURE_DOMAIN: &[u8; 18] = b"APHELION_COMMIT_V1";

pub const COMMITMENT_PREIMAGE_LEN: usize = 18 + 32 + 8 + 32 + 32; // 122
pub const COMMIT_MESSAGE_LEN: usize = 18 + 32 + 8 + 32; // 90

/// The bytes a beacon commitment is the SHA-256 of.
///
/// ```text
/// offset  len  field
///      0   18  domain separator, ASCII "APHELION_RANDOM_V1"
///     18   32  randomness contract id
///     50    8  round id, u64
///     58   32  the committing node's public key
///     90   32  the secret
/// ```
///
/// The public key is the field worth naming. Without it a node can copy
/// somebody else's published commitment and reveal the same secret once its
/// owner does — and because the contract combines secrets by XOR, two copies
/// of one secret cancel, so the copier could subtract another node's
/// contribution from the beacon entirely.
pub fn commitment_preimage(
    randomness_contract: &[u8; 32],
    round_id: u64,
    pubkey: &[u8; 32],
    secret: &[u8; 32],
) -> [u8; COMMITMENT_PREIMAGE_LEN] {
    let mut out = [0u8; COMMITMENT_PREIMAGE_LEN];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(COMMITMENT_DOMAIN);
    put(randomness_contract);
    put(&round_id.to_be_bytes());
    put(pubkey);
    put(secret);
    debug_assert_eq!(at, COMMITMENT_PREIMAGE_LEN);
    out
}

/// The bytes a node signs when it submits a commitment.
///
/// ```text
/// offset  len  field
///      0   18  domain separator, ASCII "APHELION_COMMIT_V1"
///     18   32  randomness contract id
///     50    8  round id, u64
///     58   32  the commitment
/// ```
pub fn commit_message(
    randomness_contract: &[u8; 32],
    round_id: u64,
    commitment: &[u8; 32],
) -> [u8; COMMIT_MESSAGE_LEN] {
    let mut out = [0u8; COMMIT_MESSAGE_LEN];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(COMMIT_SIGNATURE_DOMAIN);
    put(randomness_contract);
    put(&round_id.to_be_bytes());
    put(commitment);
    debug_assert_eq!(at, COMMIT_MESSAGE_LEN);
    out
}

#[cfg(test)]
mod beacon_message_tests {
    use super::*;

    #[test]
    fn the_commitment_preimage_has_the_documented_layout() {
        let preimage = commitment_preimage(&[0xAA; 32], 7, &[0xBB; 32], &[0xCC; 32]);
        assert_eq!(preimage.len(), 122);
        assert_eq!(&preimage[..18], b"APHELION_RANDOM_V1");
        assert_eq!(&preimage[18..50], &[0xAA; 32]);
        assert_eq!(&preimage[50..58], &7u64.to_be_bytes());
        assert_eq!(&preimage[58..90], &[0xBB; 32]);
        assert_eq!(&preimage[90..], &[0xCC; 32]);
    }

    #[test]
    fn the_commit_message_has_the_documented_layout() {
        let msg = commit_message(&[0xAA; 32], 7, &[0xDD; 32]);
        assert_eq!(msg.len(), 90);
        assert_eq!(&msg[..18], b"APHELION_COMMIT_V1");
        assert_eq!(&msg[18..50], &[0xAA; 32]);
        assert_eq!(&msg[50..58], &7u64.to_be_bytes());
        assert_eq!(&msg[58..], &[0xDD; 32]);
    }

    /// The contract-side mirror asserts the same file.
    ///
    /// A failure here is worse than a rejected submission. The node publishes a
    /// commitment the contract cannot reproduce, so its reveal is refused as a
    /// bad one — and it is then penalised for withholding a secret it did in
    /// fact publish.
    #[test]
    fn matches_the_shared_test_vectors() {
        let raw = include_str!("../../../tests/vectors/beacon_commitment.json");
        let vectors: serde_json::Value =
            serde_json::from_str(raw).expect("vector file is valid JSON");

        assert_eq!(
            vectors["commitment_domain"].as_str().unwrap().as_bytes(),
            COMMITMENT_DOMAIN
        );
        assert_eq!(
            vectors["signature_domain"].as_str().unwrap().as_bytes(),
            COMMIT_SIGNATURE_DOMAIN
        );
        assert_eq!(
            vectors["preimage_len"].as_u64().unwrap() as usize,
            COMMITMENT_PREIMAGE_LEN
        );
        assert_eq!(
            vectors["commit_message_len"].as_u64().unwrap() as usize,
            COMMIT_MESSAGE_LEN
        );

        let cases = vectors["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty());

        let bytes32 = |s: &str| -> [u8; 32] { hex::decode(s).unwrap().try_into().unwrap() };

        for case in cases {
            let contract = bytes32(case["contract_hex"].as_str().unwrap());
            let round_id = case["round_id"].as_u64().unwrap();
            let pubkey = bytes32(case["pubkey_hex"].as_str().unwrap());
            let secret = bytes32(case["secret_hex"].as_str().unwrap());
            let name = &case["name"];

            let preimage = commitment_preimage(&contract, round_id, &pubkey, &secret);
            assert_eq!(
                hex::encode(preimage),
                case["preimage_hex"].as_str().unwrap(),
                "vector `{name}` preimage drifted"
            );

            // The commitment itself, which is what actually reaches the ledger.
            let commitment: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(preimage).into();
            assert_eq!(
                hex::encode(commitment),
                case["commitment_hex"].as_str().unwrap(),
                "vector `{name}` commitment drifted"
            );

            assert_eq!(
                hex::encode(commit_message(&contract, round_id, &commitment)),
                case["commit_message_hex"].as_str().unwrap(),
                "vector `{name}` signed message drifted"
            );
        }
    }

    #[test]
    fn the_two_domains_cannot_be_confused_for_one_another() {
        // Both are 18 bytes and both start "APHELION_". A signature over one
        // must never verify as the other, which is only true while the bytes
        // differ -- so this is asserted rather than assumed.
        assert_ne!(COMMITMENT_DOMAIN, COMMIT_SIGNATURE_DOMAIN);
    }

    #[test]
    fn changing_any_field_changes_the_preimage() {
        let base = commitment_preimage(&[0xAA; 32], 7, &[0xBB; 32], &[0xCC; 32]);
        assert_ne!(
            base,
            commitment_preimage(&[0xAB; 32], 7, &[0xBB; 32], &[0xCC; 32])
        );
        assert_ne!(
            base,
            commitment_preimage(&[0xAA; 32], 8, &[0xBB; 32], &[0xCC; 32])
        );
        assert_ne!(
            base,
            commitment_preimage(&[0xAA; 32], 7, &[0xBC; 32], &[0xCC; 32])
        );
        assert_ne!(
            base,
            commitment_preimage(&[0xAA; 32], 7, &[0xBB; 32], &[0xCD; 32])
        );
    }
}
