#![cfg(test)]
//! The contract side of the shared signing vectors.
//!
//! `crates/aphelion-core/src/message.rs` asserts the same file. The vectors
//! themselves are produced by a third implementation,
//! `scripts/gen_test_vectors.py`, written in another language on purpose: if
//! all three agree, the layout is almost certainly what the documentation
//! says it is.
//!
//! A failure here means a node's signature will not verify on chain. That is a
//! consensus break, not a flaky test.

use soroban_sdk::{BytesN, Env, Symbol};
use std::string::String;

use crate::message::{price_message, DOMAIN_SEPARATOR, MESSAGE_LEN};

fn hex_decode(s: &str) -> std::vec::Vec<u8> {
    assert!(s.len() % 2 == 0, "hex string must have an even length");
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

#[test]
fn the_on_chain_encoder_matches_the_shared_vectors() {
    let env = Env::default();
    let raw = include_str!("../../../tests/vectors/price_message.json");
    let vectors: serde_json::Value = serde_json::from_str(raw).expect("vector file is valid JSON");

    assert_eq!(
        vectors["message_len"].as_u64().unwrap() as u32,
        MESSAGE_LEN,
        "the vector file and the contract disagree on the payload length"
    );
    assert_eq!(
        vectors["domain_separator"].as_str().unwrap().as_bytes(),
        DOMAIN_SEPARATOR
    );

    let cases = vectors["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty());

    for case in cases {
        let aggregator: [u8; 32] = hex_decode(case["aggregator_hex"].as_str().unwrap())
            .try_into()
            .expect("32-byte contract id");
        let message = price_message(
            &env,
            &BytesN::from_array(&env, &aggregator),
            &Symbol::new(&env, case["feed"].as_str().unwrap()),
            case["price_raw"].as_str().unwrap().parse().unwrap(),
            case["timestamp"].as_u64().unwrap(),
            case["confidence_bps"].as_u64().unwrap() as u32,
            case["nonce"].as_u64().unwrap(),
        );

        let mut bytes = std::vec![0u8; message.len() as usize];
        message.copy_into_slice(&mut bytes);

        assert_eq!(
            hex_encode(&bytes),
            case["message_hex"].as_str().unwrap(),
            "vector `{}` drifted; every submission would be rejected on chain",
            case["name"]
        );
    }
}

#[test]
fn every_field_is_covered_by_at_least_one_vector() {
    // A vector set that never varies a field cannot catch that field being
    // dropped from the layout.
    let raw = include_str!("../../../tests/vectors/price_message.json");
    let vectors: serde_json::Value = serde_json::from_str(raw).unwrap();
    let cases = vectors["cases"].as_array().unwrap();

    let distinct = |field: &str| -> usize {
        let mut seen: std::vec::Vec<String> = std::vec::Vec::new();
        for case in cases {
            let v = std::format!("{}", case[field]);
            if !seen.contains(&v) {
                seen.push(v);
            }
        }
        seen.len()
    };

    for field in [
        "aggregator_hex",
        "feed",
        "price_raw",
        "timestamp",
        "confidence_bps",
        "nonce",
    ] {
        assert!(
            distinct(field) > 1,
            "every vector uses the same `{field}`, so the field could be dropped unnoticed"
        );
    }
}

// -- aggregation ------------------------------------------------------------

/// The off-chain mirror (`aphelion-core::math`) asserts the same file.
///
/// A node predicts a round's outcome with that implementation and is rewarded
/// or penalised by this one. A difference of a single unit is enough to slash
/// an honest node for arithmetic it had no way to see, so these are not
/// cosmetic assertions about rounding.
mod aggregation {
    use soroban_sdk::{Env, Vec};

    use crate::math::{deviation_bps, stddev, time_weighted_average, weighted_median};

    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../tests/vectors/aggregation.json");
        serde_json::from_str(raw).expect("vector file is valid JSON")
    }

    fn i128_of(v: &serde_json::Value) -> i128 {
        v.as_str().expect("i128 vectors are strings").parse().unwrap()
    }

    fn expected(case: &serde_json::Value) -> Option<i128> {
        case["expected"].as_str().map(|s| s.parse().unwrap())
    }

    #[test]
    fn weighted_median_matches() {
        let env = Env::default();
        for case in vectors()["weighted_median"].as_array().unwrap() {
            let mut samples: Vec<(i128, u32)> = Vec::new(&env);
            for s in case["samples"].as_array().unwrap() {
                samples.push_back((i128_of(&s[0]), s[1].as_u64().unwrap() as u32));
            }
            assert_eq!(
                weighted_median(&env, &samples),
                expected(case),
                "weighted_median vector `{}` drifted",
                case["name"]
            );
        }
    }

    #[test]
    fn stddev_matches() {
        let env = Env::default();
        for case in vectors()["stddev"].as_array().unwrap() {
            let mut values: Vec<i128> = Vec::new(&env);
            for v in case["values"].as_array().unwrap() {
                values.push_back(i128_of(v));
            }
            if values.is_empty() {
                continue;
            }
            assert_eq!(
                stddev(&values),
                expected(case),
                "stddev vector `{}` drifted",
                case["name"]
            );
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
        let env = Env::default();
        for case in vectors()["twap"].as_array().unwrap() {
            let mut observations: Vec<(u64, i128)> = Vec::new(&env);
            for o in case["observations"].as_array().unwrap() {
                observations.push_back((o[0].as_u64().unwrap(), i128_of(&o[1])));
            }
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
