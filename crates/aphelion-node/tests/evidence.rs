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

/// The substitution the allegation's identifiers exist to catch, run through
/// the real pair rather than a hand-built bundle.
///
/// An operator accused over nonce 4 replays nonce 7 instead — a round they
/// reported honestly, which reproduces, and whose bundle is sound in every way
/// a bundle can be sound on its own. Nothing is forged. The committee is simply
/// answered about a different round, and only the dispute's own identifiers
/// separate that from an answer.
#[test]
fn an_honest_bundle_for_a_round_nobody_disputed_does_not_answer_the_dispute() {
    let json = bundle_json(&round("100.00", 10, 950, 7), &honest_observations());
    let bundle: verify::Bundle = serde_json::from_str(&json).unwrap();

    // Read on its own terms, it is beyond reproach.
    assert_eq!(verify::verify(&bundle).verdict, verify::Verdict::Sound);

    // Measured against the allegation on the ledger, it is not evidence in
    // this case at all.
    let allegation = verify::Expectations {
        node: Some(key().verifying_key().to_bytes()),
        feed: Some(feed()),
        nonce: Some(4),
        aggregator: Some(AGGREGATOR),
        digest: None,
    };
    let audit = verify::verify_against(&bundle, &allegation);
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert_eq!(audit.bound_to_allegation, Some(false));
    assert_eq!(audit.verdict.exit_code(), 2);

    // And the bundle for the round actually disputed passes the same check,
    // so the grade above is about the round and not about the flags.
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let bundle: verify::Bundle = serde_json::from_str(&json).unwrap();
    let audit = verify::verify_against(&bundle, &allegation);
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
    assert_eq!(audit.bound_to_allegation, Some(true));
}

/// The digest the accused publishes and the digest the committee computes are
/// the same number, over the same bytes, or the whole scheme is decoration.
///
/// `dispute respond` hashes the file `replay` wrote; `verify-evidence --digest`
/// hashes the file it was handed. Nothing forces those two to be the same
/// operation except this test: a normalisation on either side — a re-serialise,
/// a trailing newline, a parse-and-print — would leave both commands working,
/// both outputs looking right, and every honest answer reading as a
/// substitution.
#[test]
fn the_digest_answered_with_is_the_digest_a_committee_computes() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let published = verify::sha256(json.as_bytes());

    let accused = hex::encode(key().verifying_key().to_bytes());
    let allegation = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        None,
        Some(&hex::encode(published)),
    )
    .unwrap();

    let audit = verify::verify_document(json.as_bytes(), &allegation).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
    assert_eq!(audit.bound_to_allegation, Some(true));
    assert_eq!(audit.document_digest, Some(hex::encode(published)));
}

/// The substitution the digest exists to catch, and the one the four
/// identifiers cannot.
///
/// The accused answers, the vote goes against them, and they produce a second
/// bundle for the same round: same node, same feed, same nonce, same
/// deployment, same signed bytes — with an extra venue in the window that was
/// not in the first. It is sound. It is about the allegation. It is not what
/// they answered with, and only the digest says so.
#[test]
fn a_second_sound_bundle_for_the_same_round_is_still_not_the_one_answered_with() {
    let disputed = round("100.00", 10, 950, 4);
    let answered = bundle_json(&disputed, &honest_observations());
    let published = verify::sha256(answered.as_bytes());

    let mut later = honest_observations();
    later.push(obs("okx", "100.00", 980));
    let produced_afterwards = bundle_json(&disputed, &later);

    let accused = hex::encode(key().verifying_key().to_bytes());
    let four =
        verify::Expectations::parse(Some(&accused), Some("BTC_USD"), Some(4), None, None).unwrap();
    let five = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        None,
        Some(&hex::encode(published)),
    )
    .unwrap();

    // Everything the ledger's four identifiers can ask, it passes.
    let audit = verify::verify_document(produced_afterwards.as_bytes(), &four).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );

    // And it is not the document the accused committed to while the vote was
    // open.
    let audit = verify::verify_document(produced_afterwards.as_bytes(), &five).unwrap();
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert_eq!(audit.bound_to_allegation, Some(false));
    assert_eq!(audit.verdict.exit_code(), 2);

    // The file that was answered with passes the same check, so the grade
    // above is about the substitution and not about the flag.
    let audit = verify::verify_document(answered.as_bytes(), &five).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
}

/// The identifiers a committee types come off the dispute record, so the shape
/// they arrive in has to be the shape `Expectations::parse` accepts. This is
/// the same rot the rest of this file guards against, one step further out:
/// `dispute show` prints a key as lower-case hex and a nonce as a number.
#[test]
fn what_a_dispute_record_prints_is_what_the_verifier_accepts() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let bundle: verify::Bundle = serde_json::from_str(&json).unwrap();

    let accused = hex::encode(key().verifying_key().to_bytes());
    let allegation =
        verify::Expectations::parse(Some(&accused), Some("BTC_USD"), Some(4), None, None).unwrap();

    assert_eq!(
        verify::verify_against(&bundle, &allegation).verdict,
        verify::Verdict::Sound
    );
}

/// What `dispute check` does, minus the RPC: the standard comes off the
/// ledger's list of answers and nothing is typed.
///
/// The seam this pins is the one between [`aphelion_node::chain::committee`]
/// and [`verify`]. The contract holds a digest as a hex string, `replay` emits
/// bytes, and the two meet nowhere else — so a change to how either spells a
/// digest would leave both sides passing their own tests while every answer on
/// the record read as a substitution.
fn answers(records: &[(&str, u64)]) -> Vec<aphelion_node::chain::committee::ResponseRecord> {
    records
        .iter()
        .map(
            |(digest, at)| aphelion_node::chain::committee::ResponseRecord {
                dispute: 7,
                vote_round: 0,
                by: "GOPERATOR".into(),
                digest: (*digest).into(),
                uri: String::new(),
                at: *at,
            },
        )
        .collect()
}

fn refs(records: &[aphelion_node::chain::committee::ResponseRecord]) -> Vec<verify::AnswerRef<'_>> {
    records
        .iter()
        .map(|r| verify::AnswerRef {
            digest: &r.digest,
            at: r.at,
        })
        .collect()
}

/// The audit a committee member gets without transcribing anything.
#[test]
fn an_answer_on_the_record_needs_no_flag_typed_at_it() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let digest = verify::sha256(json.as_bytes());

    let on_chain = answers(&[(&hex::encode(digest), 900)]);
    let standing = verify::locate_document(&digest, &refs(&on_chain));
    assert!(standing.is_on_record(), "{standing:?}");

    // Every one of the five comes from somewhere other than the file: four
    // from the dispute record, the fifth from the list of answers.
    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();

    let audit = verify::verify_document(json.as_bytes(), &expect).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
    assert_eq!(audit.bound_to_allegation, Some(true));
}

/// An operator who posts the wrong file and corrects themselves has two real
/// documents on the record, and the earlier one is not a substitution.
///
/// This is the case a single retyped `--digest` grades wrongly. A committee
/// checking the first file against the last digest would be told it is
/// `unrelated` — a finding against an operator for doing the honest thing in
/// public.
#[test]
fn a_corrected_answer_is_graded_as_a_correction_and_not_as_a_swap() {
    let disputed = round("100.00", 10, 950, 4);
    let first = bundle_json(&round("100.00", 10, 950, 9), &honest_observations());
    let second = bundle_json(&disputed, &honest_observations());

    let mut padded = honest_observations();
    padded.push(obs("okx", "100.00", 980));
    let never_answered_with = bundle_json(&disputed, &padded);

    let on_chain = answers(&[
        (&hex::encode(verify::sha256(first.as_bytes())), 900),
        (&hex::encode(verify::sha256(second.as_bytes())), 950),
    ]);
    let records = refs(&on_chain);

    let accused = hex::encode(key().verifying_key().to_bytes());
    let judge = |raw: &str| {
        let standing = verify::locate_document(&verify::sha256(raw.as_bytes()), &records);
        let expect = verify::Expectations::parse(
            Some(&accused),
            Some("BTC_USD"),
            Some(4),
            Some(&hex::encode(AGGREGATOR)),
            standing.expected_digest(),
        )
        .unwrap();
        (
            standing,
            verify::verify_document(raw.as_bytes(), &expect).unwrap(),
        )
    };

    // The correction: the answer being offered, and about the right round.
    let (standing, audit) = judge(&second);
    assert!(matches!(standing, verify::OnRecord::Offered { .. }));
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );

    // The file it replaced. Still the operator's own document, fixed at a real
    // time — and `unrelated` here for the reason it is a wrong answer rather
    // than a swapped one: it replays nonce 9, and the allegation is nonce 4.
    let (standing, audit) = judge(&first);
    assert!(
        matches!(standing, verify::OnRecord::Superseded { .. }),
        "{standing:?}"
    );
    assert!(standing.summary().contains("superseded"));
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert!(
        audit
            .findings
            .iter()
            .any(|f| f.detail.contains("nonce 4") && f.detail.contains("nonce 9")),
        "the finding should name the round, not the document: {:?}",
        audit.findings
    );

    // And the substitution: a sound bundle for the disputed round that nobody
    // ever committed to, measured against the answer that was offered.
    let (standing, audit) = judge(&never_answered_with);
    assert!(
        matches!(standing, verify::OnRecord::Absent { .. }),
        "{standing:?}"
    );
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert_eq!(audit.verdict.exit_code(), 2);
}

/// A dispute nobody has answered binds the file to nothing, and says so rather
/// than failing it. A committee member handed a bundle privately can still
/// check everything except which document the accused stands behind.
#[test]
fn an_unanswered_dispute_leaves_the_file_unbound_without_failing_it() {
    let json = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let standing = verify::locate_document(&verify::sha256(json.as_bytes()), &[]);
    assert_eq!(standing, verify::OnRecord::Unanswered);
    assert!(standing.summary().contains("no answer"));

    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();
    let audit = verify::verify_document(json.as_bytes(), &expect).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
}

// -- the reporter's side of the same record ----------------------------------
//
// Everything above judges the accused's answer. A dispute has two documents,
// and the other one is pinned harder: a case digest is fixed at filing and can
// never be corrected. These run the same real `replay` output through the same
// real `verify` on that side, because the two sides share one command and only
// the standard differs.

const REPORTER: &str = "GREPORTER";
const CASE_FILED_AT: u64 = 800;

fn case(digest: &str) -> verify::CaseRef<'_> {
    verify::CaseRef {
        digest,
        filed_at: CASE_FILED_AT,
    }
}

/// A second node, so the reporter's evidence can be signed by something other
/// than the key it is an allegation against — which is the ordinary shape of a
/// case and the one the answer-only reading gets wrong.
fn reporters_key() -> SigningKey {
    SigningKey::from_bytes(&[99u8; 32])
}

/// `bundle_json`, signed by whichever key is given rather than the node's.
///
/// The signature is re-made over the same canonical bytes, because a bundle
/// carrying somebody else's key and the original signature would be `unsigned`
/// and would prove nothing about the standard these tests are checking.
fn bundle_json_from(
    signer: &SigningKey,
    round: &RecordedRound,
    observations: &[Observation],
) -> String {
    let message = aphelion_core::PriceMessage {
        aggregator: AGGREGATOR,
        feed: round.feed.clone(),
        price: round.price,
        timestamp: round.observed_at.timestamp() as u64,
        confidence_bps: round.confidence_bps,
        nonce: round.nonce,
    };
    let mut round = round.clone();
    round.signature = hex::encode(signer.sign(&message.to_bytes()).to_bytes());

    let result = replay::replay(
        &round,
        observations,
        window(observations),
        params(),
        &signer.verifying_key(),
    );
    serde_json::to_string_pretty(&result).expect("a bundle serialises")
}

/// The document that opened the dispute, put through the command a committee
/// member actually runs.
///
/// Against the answers alone it is `absent` — "a file nobody committed to",
/// which is the description of a substitution said about the case itself. The
/// ledger does commit to it, earlier and more permanently than to anything the
/// accused has published.
#[test]
fn the_document_a_dispute_was_filed_on_is_on_the_record_not_missing_from_it() {
    let filed = bundle_json_from(
        &reporters_key(),
        &round("100.00", 10, 950, 4),
        &honest_observations(),
    );
    let digest = verify::sha256(filed.as_bytes());
    let pinned = hex::encode(digest);

    // The accused has answered, with a document of their own.
    let answered = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let on_chain = answers(&[(&hex::encode(verify::sha256(answered.as_bytes())), 900)]);

    let standing = verify::place(&digest, case(&pinned), &refs(&on_chain));
    assert!(standing.is_case());
    assert!(standing.is_on_record());
    assert!(
        !standing.binds_to_accused(),
        "a case is not held to the accused's key"
    );
    assert_eq!(standing.expected_digest(), Some(pinned.as_str()));

    let summary = standing.summary();
    assert!(summary.starts_with("the case:"), "{summary}");
    assert!(!summary.contains("assembled after the votes"), "{summary}");
}

/// The reporter's own node, saying what it saw. Sound, about the disputed
/// round, and signed by a key that is not the accused's — which is what the
/// standard has to allow for, and what the attribution has to name.
#[test]
fn a_case_signed_by_the_reporters_own_node_is_sound_and_says_whose_it_is() {
    let reporter = reporters_key();
    let filed = bundle_json_from(
        &reporter,
        &round("100.00", 10, 950, 4),
        &honest_observations(),
    );
    let digest = verify::sha256(filed.as_bytes());
    let pinned = hex::encode(digest);

    let standing = verify::place(&digest, case(&pinned), &[]);
    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        standing.binds_to_accused().then_some(accused.as_str()),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();

    let audit = verify::verify_document(filed.as_bytes(), &expect).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
    assert_eq!(audit.bound_to_allegation, Some(true));

    // And the key it is signed by is the reporter's, resolved through the
    // registry rather than read off the file's own account of itself.
    let signer = audit.signed.as_ref().unwrap().public_key.clone();
    assert_ne!(signer, accused);
    assert_eq!(
        verify::attribute(&signer, &accused, REPORTER, Some(REPORTER)),
        verify::Signatory::Reporter {
            key: signer.clone(),
            owner: REPORTER.into()
        }
    );
    assert!(
        verify::attribute(&signer, &accused, REPORTER, Some(REPORTER))
            .summary()
            .contains("is not, by itself, a finding")
    );
}

/// The same file measured the way an answer is measured. `unrelated`, for the
/// only reason it could be: the reporter did not sign it with the accused's
/// key, and could not have. Holding a case to that standard would leave a
/// reporter able to file nothing but the allegations the accused had already
/// signed for them.
#[test]
fn holding_a_case_to_the_accuseds_key_would_reject_every_honest_one() {
    let filed = bundle_json_from(
        &reporters_key(),
        &round("100.00", 10, 950, 4),
        &honest_observations(),
    );
    let accused = hex::encode(key().verifying_key().to_bytes());

    let as_an_answer = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        None,
    )
    .unwrap();
    let audit = verify::verify_document(filed.as_bytes(), &as_an_answer).unwrap();
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert_eq!(audit.verdict.exit_code(), 2);
}

/// A reporter holding the accused's own signed round. The strongest allegation
/// there is, and the standard does not change to accommodate it: the same
/// expectations grade it sound and the attribution names the key.
#[test]
fn a_case_carrying_the_accuseds_own_signature_needs_no_second_opinion() {
    let filed = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let digest = verify::sha256(filed.as_bytes());
    let pinned = hex::encode(digest);

    let standing = verify::place(&digest, case(&pinned), &[]);
    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        standing.binds_to_accused().then_some(accused.as_str()),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();

    let audit = verify::verify_document(filed.as_bytes(), &expect).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );

    let signer = audit.signed.as_ref().unwrap().public_key.clone();
    assert_eq!(
        verify::attribute(&signer, &accused, REPORTER, None),
        verify::Signatory::Accused { key: accused }
    );
}

/// The substitution, on the reporter's side. A second sound bundle for the same
/// round, produced after the case was filed, is not the case — and because a
/// case can never be corrected there is no `superseded` to soften it.
///
/// What catches it is the record rather than the arithmetic, and it has to be:
/// both files are genuine, both are about the disputed round, and the only
/// thing that separates them is which one the reporter committed to. The
/// summary names the digest that was filed so a committee member can see what
/// they were supposed to have been handed.
#[test]
fn a_case_cannot_be_swapped_for_a_better_one_after_it_is_filed() {
    let reporter = reporters_key();
    let disputed = round("100.00", 10, 950, 4);
    let filed = bundle_json_from(&reporter, &disputed, &honest_observations());

    let mut padded = honest_observations();
    padded.push(obs("okx", "100.00", 980));
    let improved = bundle_json_from(&reporter, &disputed, &padded);
    assert_ne!(filed, improved);

    let pinned = hex::encode(verify::sha256(filed.as_bytes()));
    let standing = verify::place(&verify::sha256(improved.as_bytes()), case(&pinned), &[]);

    assert!(!standing.is_case());
    assert!(!standing.is_on_record());
    let summary = standing.summary();
    assert!(summary.contains(&pinned), "{summary}");
    assert!(summary.contains("neither side of the record"), "{summary}");

    // And the audit under it is unchanged: the swapped file is a perfectly
    // sound bundle about the disputed round, which is exactly why the record
    // is the only thing that can tell the two apart.
    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        standing.binds_to_accused().then_some(accused.as_str()),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();
    let audit = verify::verify_document(improved.as_bytes(), &expect).unwrap();
    assert_eq!(audit.verdict, verify::Verdict::Unrelated);
    assert_eq!(audit.verdict.exit_code(), 2);
}

/// A bundle the accused hands over privately, before they have answered, is not
/// measured against the reporter's case.
///
/// The one thing [`verify::Standing::expected_digest`] must not do is fall back
/// to the case when there are no answers. It is the only commitment on the
/// record at that point, and reaching for it would grade every pre-answer
/// bundle `unrelated` — a substitution finding against an operator for showing
/// their working early.
#[test]
fn a_bundle_offered_before_any_answer_is_not_measured_against_the_case() {
    let filed = b"an exchange's own export, which is not a bundle".to_vec();
    let pinned = hex::encode(verify::sha256(&filed));

    let answering = bundle_json(&round("100.00", 10, 950, 4), &honest_observations());
    let standing = verify::place(&verify::sha256(answering.as_bytes()), case(&pinned), &[]);

    assert!(!standing.is_case());
    assert_eq!(standing.expected_digest(), None);
    assert!(standing.binds_to_accused());

    let accused = hex::encode(key().verifying_key().to_bytes());
    let expect = verify::Expectations::parse(
        Some(&accused),
        Some("BTC_USD"),
        Some(4),
        Some(&hex::encode(AGGREGATOR)),
        standing.expected_digest(),
    )
    .unwrap();
    let audit = verify::verify_document(answering.as_bytes(), &expect).unwrap();
    assert_eq!(
        audit.verdict,
        verify::Verdict::Sound,
        "{:?}",
        audit.findings
    );
}

/// The commonest document in a dispute is not a bundle at all, and on the
/// reporter's side it is commoner still: an exchange's own export, a written
/// account, a log archive. The ledger still answers the question that matters
/// about it, and nothing here grades it.
#[test]
fn a_case_that_is_not_a_bundle_is_placed_on_the_record_and_not_graded() {
    let written = b"The node published 100.00 while every venue we read was at 92.\n";
    let digest = verify::sha256(written);
    let pinned = hex::encode(digest);

    let standing = verify::place(&digest, case(&pinned), &[]);
    assert!(standing.is_case());
    assert!(standing.summary().contains(&CASE_FILED_AT.to_string()));

    // No verdict, and deliberately not a failed one.
    let expect = verify::Expectations::parse(None, None, None, None, Some(&pinned)).unwrap();
    assert!(verify::verify_document(written, &expect).is_err());
}
