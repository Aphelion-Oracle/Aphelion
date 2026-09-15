//! The two halves of a dispute, against each other.
//!
//! `replay` writes an evidence bundle and `verify` reads one, and they are
//! deliberately built out of different types: one serialises the node's own
//! structs, the other deserialises hostile input into shapes that share no code
//! with them. That separation is what makes "the bundle's verdict is ignored" a
//! property of the types rather than a promise in a comment — and it is also
//! exactly how the pair could rot apart without either side's unit tests
//! noticing. Rename a field on one, and `replay` keeps emitting valid JSON,
//! `verify` keeps passing its own tests against bundles it built itself, and the
//! only thing that breaks is the one path that matters: an operator handing a
//! committee a file it cannot read.
//!
//! So these tests run the real encoder into the real decoder through actual
//! JSON. Nothing here constructs a `Bundle` by hand; every one of them comes out
//! of `replay`.

use aphelion_core::{FeedId, Price};
use aphelion_node::db::{Observation, RecordedRound};
use aphelion_node::engine::replay::{self, ParamsUsed};
use aphelion_node::engine::verify;
use aphelion_node::engine::AggregationParams;
use chrono::{DateTime, TimeZone, Utc};
use ed25519_dalek::{Signer, SigningKey};

const AGGREGATOR: [u8; 32] = [7u8; 32];

fn key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn at(unix: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(unix, 0).unwrap()
}

fn feed() -> FeedId {
    FeedId::new("BTC_USD").unwrap()
}

fn obs(source: &str, price: &str, observed: i64) -> Observation {
    Observation {
        feed: feed(),
        source: source.into(),
        price: Price::parse_decimal(price).unwrap(),
        observed_at: at(observed),
        received_at: at(observed),
    }
}

fn params() -> replay::Params {
    replay::Params {
        aggregation: AggregationParams {
            min_sources: 3,
            max_source_deviation_bps: 100,
        },
        confidence_floor: 10,
        aggregator: AGGREGATOR,
    }
}

/// A round signed honestly over its own contents, as the node would have.
fn round(price: &str, confidence_bps: u32, observed_at: i64, nonce: u64) -> RecordedRound {
    let price = Price::parse_decimal(price).unwrap();
    let message = aphelion_core::PriceMessage {
        aggregator: AGGREGATOR,
        feed: feed(),
        price,
        timestamp: observed_at as u64,
        confidence_bps,
        nonce,
    };
    RecordedRound {
        id: 1,
        feed: feed(),
        nonce,
        price,
        confidence_bps,
        source_count: 3,
        spread_bps: 0,
        stddev: Price::from_raw(0),
        observed_at: at(observed_at),
        signature: hex::encode(key().sign(&message.to_bytes()).to_bytes()),
        status: "submitted".into(),
        tx_hash: Some("deadbeef".into()),
        error: None,
        created_at: at(1_000),
    }
}

fn window(observations: &[Observation]) -> replay::Window {
    replay::Window {
        cutoff: at(900),
        as_of: at(1_000),
        max_observation_age_secs: 100,
        observations: observations.to_vec(),
    }
}

fn honest_observations() -> Vec<Observation> {
    vec![
        obs("binance", "100.00", 950),
        obs("kraken", "100.00", 960),
        obs("coinbase", "100.00", 970),
    ]
}

/// Produce a bundle the way the command does: replay, then serialise.
fn bundle_json(round: &RecordedRound, observations: &[Observation]) -> String {
    let result = replay::replay(
        round,
        observations,
        window(observations),
        params(),
        &key().verifying_key(),
    );
    serde_json::to_string_pretty(&result).expect("a bundle serialises")
}

/// The path the whole feature exists for: an operator replays a good round,
/// hands the file over, and the other side can read it and agree.
#[test]
fn a_bundle_the_operator_produces_is_one_the_committee_can_check() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());

    let bundle: verify::Bundle =
        serde_json::from_str(&json).expect("replay's output must parse as a bundle");
    let audit = verify::verify(&bundle);

    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
    let signed = audit.signed.expect("established from the signed bytes");
    assert_eq!(signed.feed, "BTC_USD");
    assert_eq!(signed.nonce, 4);
    assert_eq!(signed.price, "100.00000000");
    assert_eq!(audit.recomputed.as_deref(), Some("100.00000000"));
}

/// The verifier reaches its conclusion from the bytes, so it must agree with the
/// operator's own replay about a round that genuinely does not reproduce —
/// without having been told what that replay concluded.
#[test]
fn both_halves_agree_when_the_observations_do_not_support_the_price() {
    let observations = honest_observations();
    let round = round("110.00", 10, 950, 4);

    let replayed = replay::replay(
        &round,
        &observations,
        window(&observations),
        params(),
        &key().verifying_key(),
    );
    assert_eq!(replayed.verdict, replay::Verdict::Diverged);

    let json = serde_json::to_string(&replayed).unwrap();
    let bundle: verify::Bundle = serde_json::from_str(&json).unwrap();
    assert_eq!(
        verify::verify(&bundle).verdict,
        verify::Verdict::Unsupported
    );
}

/// Every field the verifier needs has to survive the trip. Deleting any one of
/// them must fail loudly at parse time rather than be defaulted into a silent
/// pass — a bundle missing its signature is not a bundle with an empty
/// signature.
#[test]
fn a_bundle_missing_any_field_the_verifier_needs_is_refused() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let full: serde_json::Value = serde_json::from_str(&json).unwrap();

    for (section, field) in [
        ("recorded", "feed"),
        ("recorded", "nonce"),
        ("recorded", "price"),
        ("recorded", "confidence_bps"),
        ("recorded", "observed_at"),
        ("recorded", "signature"),
        ("provenance", "public_key"),
        ("provenance", "aggregator"),
        ("provenance", "message_hex"),
        ("params", "min_sources"),
        ("params", "max_source_deviation_bps"),
        ("params", "confidence_floor_bps"),
        ("window", "cutoff"),
        ("window", "as_of"),
        ("window", "observations"),
    ] {
        let mut broken = full.clone();
        broken[section]
            .as_object_mut()
            .unwrap_or_else(|| panic!("`{section}` is an object in replay's output"))
            .remove(field)
            .unwrap_or_else(|| {
                panic!("replay's output has no `{section}.{field}` — the pair has drifted")
            });

        assert!(
            serde_json::from_value::<verify::Bundle>(broken).is_err(),
            "a bundle without `{section}.{field}` must be refused, not defaulted"
        );
    }
}

/// The observations in a bundle must carry every field the verifier's window
/// checks need. `received_at` in particular: without it there is no way to tell
/// a row the round could read from one that arrived afterwards.
#[test]
fn an_observation_missing_a_field_the_verifier_needs_is_refused() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let full: serde_json::Value = serde_json::from_str(&json).unwrap();

    for field in ["source", "price", "observed_at", "received_at"] {
        let mut broken = full.clone();
        broken["window"]["observations"][0]
            .as_object_mut()
            .unwrap()
            .remove(field)
            .unwrap_or_else(|| panic!("replay's observations have no `{field}`"));

        assert!(
            serde_json::from_value::<verify::Bundle>(broken).is_err(),
            "an observation without `{field}` must be refused"
        );
    }
}

/// The parameters have to travel, or the verifier is guessing at the arithmetic.
/// This checks they arrive with the values the replay actually used, rather than
/// merely being present.
#[test]
fn the_parameters_the_replay_used_reach_the_verifier_intact() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["params"]["min_sources"], 3);
    assert_eq!(value["params"]["max_source_deviation_bps"], 100);
    assert_eq!(value["params"]["confidence_floor_bps"], 10);

    // And a bundle whose stated filter is too strict for its own observations
    // reports that it cannot support its price, rather than quietly using a
    // filter the verifier preferred.
    let mut strict = value.clone();
    strict["params"]["min_sources"] = serde_json::json!(9);
    let bundle: verify::Bundle = serde_json::from_value(strict).unwrap();
    assert_eq!(
        verify::verify(&bundle).verdict,
        verify::Verdict::Unsupported
    );
}

/// A bundle whose prose has been edited after the fact, in transit or by its
/// author, is caught on the far side. The signature and payload here are
/// untouched and valid; only the story around them changed.
#[test]
fn editing_a_bundle_after_replay_produced_it_is_caught() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();

    value["recorded"]["price"] = serde_json::json!("150.00000000");
    for o in value["window"]["observations"].as_array_mut().unwrap() {
        o["price"] = serde_json::json!("150.00000000");
    }
    // Including the operator's own conclusion, which the verifier must ignore.
    value["verdict"] = serde_json::json!("reproduced");

    let bundle: verify::Bundle = serde_json::from_value(value).unwrap();
    let audit = verify::verify(&bundle);

    assert_eq!(audit.verdict, verify::Verdict::Misdescribed);
    // The signed price, not the asserted one.
    assert_eq!(audit.signed.unwrap().price, "100.00000000");
}

/// `ParamsUsed` is re-exported for anyone building a bundle outside this crate;
/// if that stops compiling the bundle format has changed shape.
#[test]
fn the_bundle_parameter_shape_is_part_of_the_public_surface() {
    let p = ParamsUsed {
        min_sources: 3,
        max_source_deviation_bps: 100,
        confidence_floor_bps: 10,
    };
    let json = serde_json::to_value(p).unwrap();
    assert_eq!(json["min_sources"], 3);
}
