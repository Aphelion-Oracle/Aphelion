//! On-chain mirror of the canonical Aphelion signing payload.
//!
//! This module exists to reconstruct, byte for byte, the message a node
//! signed off chain. The reference implementation lives in
//! `crates/aphelion-core/src/message.rs`; this is the mirror, and
//! `tests/vectors/price_message.json` is asserted by both. If the two ever
//! disagree, `ed25519_verify` fails and *every* submission is rejected, so a
//! failure in `test_vectors.rs` is a consensus-breaking bug rather than a
//! flaky test.
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

use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{panic_with_error, Address, Bytes, BytesN, Env, Symbol};

use crate::error::AggregatorError;

/// ASCII domain separator, 17 bytes.
pub const DOMAIN_SEPARATOR: [u8; 17] = *b"APHELION_PRICE_V1";

/// Total length of the signing payload.
pub const MESSAGE_LEN: u32 = 117;

/// Fixed width of the feed id inside the payload.
pub const FEED_ID_PADDED_LEN: usize = 32;

/// The raw 32-byte contract id behind a contract [`Address`].
///
/// Derived from the address rather than stored in configuration on purpose:
/// a stored contract id is one more thing a deployment can get wrong, and
/// getting it wrong means every signature verifies against the wrong bytes.
///
/// The XDR of an `ScVal::Address` holding an `ScAddress::Contract` is
/// `[0,0,0,18][0,0,0,1][32-byte hash]`, so the id is a fixed slice.
pub fn contract_id_bytes(env: &Env, address: &Address) -> BytesN<32> {
    let xdr = address.clone().to_xdr(env);
    let discriminant: [u8; 4] = match (&xdr.slice(4..8)).try_into() {
        Ok(d) => d,
        Err(_) => panic_with_error!(env, AggregatorError::NotContractAddress),
    };
    if discriminant != [0, 0, 0, 1] {
        panic_with_error!(env, AggregatorError::NotContractAddress);
    }
    match BytesN::<32>::try_from(xdr.slice(8..40)) {
        Ok(b) => b,
        Err(_) => panic_with_error!(env, AggregatorError::NotContractAddress),
    }
}

/// A feed [`Symbol`] as ASCII, right-padded with zeros to 32 bytes.
///
/// `Symbol` has no direct byte accessor, so the value is round-tripped
/// through its XDR form: `[0,0,0,15][4-byte length][ascii]`.
pub fn feed_id_padded(env: &Env, feed: &Symbol) -> [u8; FEED_ID_PADDED_LEN] {
    let xdr = feed.clone().to_xdr(env);
    let len_bytes: [u8; 4] = match (&xdr.slice(4..8)).try_into() {
        Ok(l) => l,
        Err(_) => panic_with_error!(env, AggregatorError::UnknownFeed),
    };
    let len = u32::from_be_bytes(len_bytes);
    if len == 0 || len as usize > FEED_ID_PADDED_LEN {
        panic_with_error!(env, AggregatorError::UnknownFeed);
    }

    let mut out = [0u8; FEED_ID_PADDED_LEN];
    let body = xdr.slice(8..8 + len);
    for i in 0..len {
        out[i as usize] = match body.get(i) {
            Some(b) => b,
            None => panic_with_error!(env, AggregatorError::UnknownFeed),
        };
    }
    out
}

/// Build the exact bytes a node signs for one observation.
pub fn price_message(
    env: &Env,
    aggregator: &BytesN<32>,
    feed: &Symbol,
    price: i128,
    timestamp: u64,
    confidence_bps: u32,
    nonce: u64,
) -> Bytes {
    let mut buf = Bytes::from_array(env, &DOMAIN_SEPARATOR);
    buf.extend_from_array(&aggregator.to_array());
    buf.extend_from_array(&feed_id_padded(env, feed));
    buf.extend_from_array(&price.to_be_bytes());
    buf.extend_from_array(&timestamp.to_be_bytes());
    buf.extend_from_array(&confidence_bps.to_be_bytes());
    buf.extend_from_array(&nonce.to_be_bytes());
    debug_assert_eq!(buf.len(), MESSAGE_LEN);
    buf
}
