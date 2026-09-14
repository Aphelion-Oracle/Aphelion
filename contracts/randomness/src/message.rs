//! The two payloads this contract hashes, byte for byte.
//!
//! Both exist for the same reason the price payload does: a node builds them
//! off chain and the contract rebuilds them here, so any disagreement is a
//! verification failure rather than a silent difference of opinion.
//!
//! **The commitment.** What a node commits to is not its secret but a hash
//! that binds the secret to this contract, this round and this node:
//!
//! ```text
//! offset  len  field
//!      0   18  domain separator, ASCII "APHELION_RANDOM_V1"
//!     18   32  randomness contract id
//!     50    8  round id, u64
//!     58   32  the committing node's public key
//!     90   32  the secret
//! ```
//!
//! Every field before the secret is doing work. Without the contract id, a
//! commitment made on testnet is a valid commitment on mainnet. Without the
//! round id, a commitment can be replayed into a later round — where the node
//! already knows the secret it is "committing" to, which is not a commitment
//! at all. Without the public key, a node can copy somebody else's published
//! commitment and, once that node reveals, reveal the same secret: two
//! participants contributing one party's entropy, which is exactly the
//! independence the beacon is counting.
//!
//! **The signature.** The commitment is also signed, over:
//!
//! ```text
//! offset  len  field
//!      0   19  domain separator, ASCII "APHELION_COMMIT_V1"
//!     19   32  randomness contract id
//!     51    8  round id, u64
//!     59   32  the commitment
//! ```
//!
//! The signature is what makes the commitment the node's own rather than the
//! submitter's. As with price submissions, whoever pays for the transaction
//! and whoever authorises its contents are deliberately separable.

use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{panic_with_error, Address, Bytes, BytesN, Env};

use crate::error::RandomnessError;

pub const COMMITMENT_DOMAIN: [u8; 18] = *b"APHELION_RANDOM_V1";
pub const SIGNATURE_DOMAIN: [u8; 18] = *b"APHELION_COMMIT_V1";

pub const COMMITMENT_PREIMAGE_LEN: u32 = 122;
pub const SIGNATURE_MESSAGE_LEN: u32 = 90;

/// The raw 32-byte contract id behind a contract [`Address`].
///
/// Derived rather than configured, for the reason the aggregator gives: a
/// stored id is one more thing a deployment can get wrong, and getting it
/// wrong means every commitment binds to the wrong bytes.
pub fn contract_id_bytes(env: &Env, address: &Address) -> BytesN<32> {
    let xdr = address.clone().to_xdr(env);
    let discriminant: [u8; 4] = match (&xdr.slice(4..8)).try_into() {
        Ok(d) => d,
        Err(_) => panic_with_error!(env, RandomnessError::NotContractAddress),
    };
    if discriminant != [0, 0, 0, 1] {
        panic_with_error!(env, RandomnessError::NotContractAddress);
    }
    match BytesN::<32>::try_from(xdr.slice(8..40)) {
        Ok(b) => b,
        Err(_) => panic_with_error!(env, RandomnessError::NotContractAddress),
    }
}

/// The preimage a commitment is the SHA-256 of.
pub fn commitment_preimage(
    env: &Env,
    contract: &BytesN<32>,
    round_id: u64,
    pubkey: &BytesN<32>,
    secret: &BytesN<32>,
) -> Bytes {
    let mut buf = Bytes::from_array(env, &COMMITMENT_DOMAIN);
    buf.extend_from_array(&contract.to_array());
    buf.extend_from_array(&round_id.to_be_bytes());
    buf.extend_from_array(&pubkey.to_array());
    buf.extend_from_array(&secret.to_array());
    debug_assert_eq!(buf.len(), COMMITMENT_PREIMAGE_LEN);
    buf
}

/// The bytes a node signs when it submits a commitment.
pub fn commit_message(
    env: &Env,
    contract: &BytesN<32>,
    round_id: u64,
    commitment: &BytesN<32>,
) -> Bytes {
    let mut buf = Bytes::from_array(env, &SIGNATURE_DOMAIN);
    buf.extend_from_array(&contract.to_array());
    buf.extend_from_array(&round_id.to_be_bytes());
    buf.extend_from_array(&commitment.to_array());
    debug_assert_eq!(buf.len(), SIGNATURE_MESSAGE_LEN);
    buf
}
