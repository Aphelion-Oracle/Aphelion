//! The contract side of the shared beacon vectors.
//!
//! `crates/aphelion-core/src/message.rs` asserts the same file, and
//! `scripts/gen_test_vectors.py` produces it — a third implementation, in
//! another language, for the reason the price vectors give: two
//! implementations that agree may simply share a misreading of a byte layout.
//!
//! A failure here is worse than a rejected submission. A node whose preimage
//! differs from this contract's publishes a commitment the contract cannot
//! reproduce, so its reveal is refused as a bad one — and it is then penalised
//! for withholding a secret it did in fact publish. Consensus break, not a
//! flaky test.

use soroban_sdk::{Bytes, BytesN, Env};
use std::string::String;

use crate::message::{
    commit_message, commitment_preimage, COMMITMENT_DOMAIN, COMMITMENT_PREIMAGE_LEN,
    SIGNATURE_DOMAIN, SIGNATURE_MESSAGE_LEN,
};

fn hex_decode(s: &str) -> std::vec::Vec<u8> {
    assert!(
        s.len().is_multiple_of(2),
        "hex string must have an even length"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&std::format!("{b:02x}"));
    }
    out
}

fn bytes32(env: &Env, s: &str) -> BytesN<32> {
    let raw: [u8; 32] = hex_decode(s).try_into().expect("32 bytes");
    BytesN::from_array(env, &raw)
}

fn to_vec(bytes: &Bytes) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::with_capacity(bytes.len() as usize);
    for i in 0..bytes.len() {
        out.push(bytes.get(i).expect("in range"));
    }
    out
}

#[test]
fn the_on_chain_encoder_matches_the_shared_vectors() {
    let env = Env::default();
    let raw = include_str!("../../../tests/vectors/beacon_commitment.json");
    let vectors: serde_json::Value = serde_json::from_str(raw).expect("vector file is valid JSON");

    assert_eq!(
        vectors["preimage_len"].as_u64().unwrap() as u32,
        COMMITMENT_PREIMAGE_LEN,
        "the vector file and the contract disagree on the preimage length"
    );
    assert_eq!(
        vectors["commit_message_len"].as_u64().unwrap() as u32,
        SIGNATURE_MESSAGE_LEN
    );
    assert_eq!(
        vectors["commitment_domain"].as_str().unwrap().as_bytes(),
        COMMITMENT_DOMAIN
    );
    assert_eq!(
        vectors["signature_domain"].as_str().unwrap().as_bytes(),
        SIGNATURE_DOMAIN
    );

    let cases = vectors["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty());

    for case in cases {
        let contract = bytes32(&env, case["contract_hex"].as_str().unwrap());
        let round_id = case["round_id"].as_u64().unwrap();
        let pubkey = bytes32(&env, case["pubkey_hex"].as_str().unwrap());
        let secret = bytes32(&env, case["secret_hex"].as_str().unwrap());
        let name = case["name"].as_str().unwrap();

        let preimage = commitment_preimage(&env, &contract, round_id, &pubkey, &secret);
        assert_eq!(
            hex_encode(&to_vec(&preimage)),
            case["preimage_hex"].as_str().unwrap(),
            "vector `{name}` preimage drifted"
        );

        // The commitment the contract will compare a reveal against, produced
        // by the host's SHA-256 rather than by any Rust crate -- which is the
        // half of this that a pure-Rust test could not check.
        let commitment = env.crypto().sha256(&preimage).to_bytes();
        assert_eq!(
            hex_encode(&commitment.to_array()),
            case["commitment_hex"].as_str().unwrap(),
            "vector `{name}` commitment drifted"
        );

        assert_eq!(
            hex_encode(&to_vec(&commit_message(
                &env,
                &contract,
                round_id,
                &commitment
            ))),
            case["commit_message_hex"].as_str().unwrap(),
            "vector `{name}` signed message drifted"
        );
    }
}

#[test]
fn the_two_domains_are_distinct_on_chain_too() {
    // Both are 18 bytes and both begin "APHELION_". A commitment preimage that
    // accidentally used the signature domain would hash cleanly and open
    // nothing, which is the failure this whole file exists to catch early.
    assert_ne!(COMMITMENT_DOMAIN, SIGNATURE_DOMAIN);
}
