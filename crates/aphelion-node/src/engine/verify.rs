//! Checking somebody else's evidence bundle.
//!
//! [`super::replay`] gives a disputed operator a bundle. This is the other half,
//! and without it the bundle is JSON with a verdict in it: the committee holding
//! it has the accused's own software's opinion of the accused's own conduct, and
//! no way to check it that does not amount to taking their word.
//!
//! So the input here is **hostile**. Not "possibly stale" — authored by the
//! party with the most to gain from it being believed. Every number in a bundle
//! is a claim, including the ones this codebase wrote, and the verdict and
//! findings are ignored outright: a verifier that read them would be quoting the
//! accused back to the committee.
//!
//! What is actually worth anything in a bundle is 117 bytes and a signature.
//! Those the accused cannot forge, because the aggregator's contract id and the
//! feed and the nonce are inside the bytes and the key is registered on chain.
//! Everything else is checked *against* them:
//!
//! 1. The payload decodes as an Aphelion price message at all. A lenient
//!    decoder would be the hole in the middle of this — see
//!    [`aphelion_core::PriceMessage::from_bytes`].
//! 2. The signature verifies over those exact bytes under the stated key.
//! 3. The bundle's prose agrees with the bytes. This is the check whose absence
//!    would make the rest ceremonial: a bundle can perfectly well carry a valid
//!    signature over a payload saying one thing and a `recorded` section saying
//!    another, and a verifier that checked the signature and then read the price
//!    out of the JSON beside it would accept exactly that. The price compared
//!    against the observations is the price *inside the signed bytes*, never the
//!    one the bundle says is there.
//! 4. The observations produce that price, under the parameters the bundle
//!    states it used.
//! 5. The observations are internally consistent with the window they claim.
//!
//! ## Is it about the allegation?
//!
//! Everything above is a question about the bundle on its own. There is a
//! sixth, and it is the one a committee actually has to answer: does this file
//! bear on *the dispute in front of us*? A bundle can pass every check here and
//! be about another node, another feed, another nonce, or another deployment
//! entirely — and it is worth being plain that this is the cheapest possible
//! attack on the whole scheme. The accused does not have to forge anything.
//! They hand over a genuine, sound, reproducible bundle for a round they
//! reported honestly, and a verifier with nothing to compare it against
//! confirms every cryptographic step and reports `sound`.
//!
//! So [`Expectations`] carries what the ledger says the allegation is, and each
//! field given is checked against the *signed bytes*: the accused's key, the
//! feed, the nonce and the aggregator's contract id are all inside the 117
//! bytes. A mismatch is [`Verdict::Unrelated`], which is its own grade because
//! it is its own answer — not "the evidence is bad" but "this is not evidence
//! in this case".
//!
//! The expectations are not read from the bundle, and cannot be: a file that
//! supplied the standard it is measured against would pass by construction.
//! They come from the dispute record, which is why [`crate::chain::committee::DisputeRecord`]
//! names the nonce the accused signed rather than the round id the aggregator
//! allocated. An allegation identified by something outside the signed payload
//! could not be bound to the evidence answering it by any amount of arithmetic.
//!
//! ## The limit, stated rather than smoothed over
//!
//! This cannot detect an **omission**. A bundle showing four venues that agree
//! is indistinguishable from a bundle showing four of six, where the two left
//! out would have moved the median. Only the operator holds the full table, so
//! no arithmetic performed on what they chose to hand over can close that gap,
//! and pretending otherwise would be worse than the gap: a committee told
//! "verified" would reasonably hear "complete".
//!
//! What a sound bundle does establish is narrower and still worth having: this
//! key signed this price for this feed at this nonce on this deployment, and the
//! observations offered are consistent with it. A bundle that fails is worth
//! more still, because the failures are not subtle — a signature that does not
//! verify, or prose that disagrees with the bytes it is attached to, is not
//! something an honest bundle does by accident.

use aphelion_core::{deviation_bps, FeedId, Price, PriceMessage};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::aggregate::{aggregate, confidence_bps, AggregationParams};
use crate::db::Observation;

/// How much the bundle is worth, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The payload does not decode, or the signature over it does not verify.
    /// The bundle establishes nothing whatsoever.
    Unsigned,
    /// The signature is good and the bundle describes something else. A bundle
    /// that proves one price and asserts another is not a mistake to correct,
    /// it is the shape of an attempt.
    Misdescribed,
    /// Genuine and honestly described, and about something other than the
    /// allegation it was handed in to answer. Only reachable when the caller
    /// said what the allegation is; see [`Expectations`].
    ///
    /// Ranked below `misdescribed` and above `unsupported` on purpose. It is
    /// less damning than a file that lies about its own bytes — the wrong
    /// bundle can be attached by accident — and more damning than one that
    /// fails to add up, because an unsupported bundle is at least an attempt
    /// to answer the question asked.
    Unrelated,
    /// Honestly described and genuinely signed, but the observations offered do
    /// not produce the price that was signed.
    Unsupported,
    /// Signed, honestly described, and supported by the observations offered.
    Sound,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned",
            Self::Misdescribed => "misdescribed",
            Self::Unrelated => "unrelated",
            Self::Unsupported => "unsupported",
            Self::Sound => "sound",
        }
    }

    /// Zero only for a sound bundle. Two grades of failure, because they are
    /// different accusations: 1 is evidence that does not carry its claim, 2 is
    /// a bundle that is not the evidence it was asked for — whether because it
    /// misreports its own bytes, or because it is about another round.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Sound => 0,
            Self::Unsupported => 1,
            Self::Unrelated | Self::Misdescribed | Self::Unsigned => 2,
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub verdict: Verdict,
    pub detail: String,
}

impl Finding {
    fn new(verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            verdict,
            detail: detail.into(),
        }
    }
}

// -- the allegation, as the ledger states it ---------------------------------

/// What the dispute says, for the bundle to be measured against.
///
/// Every field is optional and each given one is checked, so a committee member
/// who has only the accused's key is not forced to invent a nonce. Nothing here
/// is ever read out of the bundle: a file that supplied its own standard would
/// meet it by construction. These come from the dispute record — `dispute show`
/// prints all four.
///
/// Parsed rather than strings, and parsed before the audit runs, because a
/// mistyped key is a mistake by the person checking and must be reported as
/// one. Graded as `unrelated` it would read as a finding against the accused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expectations {
    /// The accused, by signing key.
    pub node: Option<[u8; 32]>,
    pub feed: Option<FeedId>,
    /// The nonce the allegation names — which is why it names one.
    pub nonce: Option<u64>,
    /// The deployment. A bundle from another network is sound and irrelevant.
    pub aggregator: Option<[u8; 32]>,
}

impl Expectations {
    /// Nothing to check against: the bundle is judged on its own terms, as it
    /// was before any of this existed.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Accept the four as an operator would paste them off a dispute.
    ///
    /// The error is a usage error, not a verdict. A key that is not a key says
    /// nothing whatsoever about the bundle.
    pub fn parse(
        node: Option<&str>,
        feed: Option<&str>,
        nonce: Option<u64>,
        aggregator: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            node: node.map(|s| parse_key32(s, "--node")).transpose()?,
            feed: feed
                .map(|s| {
                    FeedId::new(s.trim()).map_err(|e| format!("`--feed {s}` is not a feed id: {e}"))
                })
                .transpose()?,
            nonce,
            aggregator: aggregator.map(parse_contract_id).transpose()?,
        })
    }
}

/// 32 bytes of hex, however it was pasted.
fn parse_key32(s: &str, flag: &str) -> Result<[u8; 32], String> {
    let raw = s.trim().trim_start_matches("0x");
    hex::decode(raw)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| {
            format!("`{flag} {s}` is not a node key: expected 32 bytes of hex (64 characters)")
        })
}

/// A contract id as either a `C...` address or the raw 32 bytes in hex, since
/// the deployment file gives the first and the signed payload carries the
/// second.
fn parse_contract_id(s: &str) -> Result<[u8; 32], String> {
    let t = s.trim();
    if t.len() == 56 && t.starts_with('C') {
        return crate::strkey::contract_id_bytes(t).map_err(|e| e.to_string());
    }
    parse_key32(t, "--aggregator").map_err(|_| {
        format!("`--aggregator {s}` is neither a C... contract address nor 32 bytes of hex")
    })
}

// -- the bundle, as untrusted input ------------------------------------------
//
// A separate set of types from the ones `replay` serialises, rather than
// `Deserialize` on those. Two reasons, and the second is the real one.
//
// Prices arrive as decimal strings and have to be parsed rather than trusted,
// so the shapes genuinely differ. And declaring the input explicitly is what
// makes "the verdict is ignored" a fact about the code instead of a promise in
// a comment: there is no field here to read it into.

#[derive(Debug, Clone, Deserialize)]
pub struct Bundle {
    pub recorded: BundleRecorded,
    pub provenance: BundleProvenance,
    pub params: BundleParams,
    pub window: BundleWindow,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleRecorded {
    pub feed: String,
    pub nonce: u64,
    pub price: String,
    pub confidence_bps: u32,
    pub observed_at: DateTime<Utc>,
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleProvenance {
    pub public_key: String,
    pub aggregator: String,
    pub message_hex: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct BundleParams {
    pub min_sources: usize,
    pub max_source_deviation_bps: u32,
    pub confidence_floor_bps: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleWindow {
    pub cutoff: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub observations: Vec<BundleObservation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleObservation {
    pub source: String,
    pub price: String,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

// -- the result --------------------------------------------------------------

/// The fields recovered from the signed bytes.
///
/// Not the bundle's account of them. This is the only part of a bundle that
/// nobody could have written without the key.
#[derive(Debug, Clone, Serialize)]
pub struct Signed {
    pub feed: String,
    pub price: String,
    pub confidence_bps: u32,
    pub timestamp: u64,
    pub nonce: u64,
    pub aggregator: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Audit {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    /// Present once the payload decodes and the signature verifies — which is
    /// the point at which anything in a bundle becomes a fact.
    pub signed: Option<Signed>,
    /// What the observations offered actually produce, under the bundle's own
    /// stated parameters.
    pub recomputed: Option<String>,
    pub observation_count: usize,
    /// Whether the signed bytes name the allegation the caller gave, or `None`
    /// if they gave none. `None` is not a pass: it means nobody asked.
    pub bound_to_allegation: Option<bool>,
}

/// Audit one bundle on its own terms, with no allegation to measure it against.
///
/// Leaves the question a committee most needs answered — whether this file is
/// about the dispute at all — open and says so. Prefer [`verify_against`].
pub fn verify(bundle: &Bundle) -> Audit {
    verify_against(bundle, &Expectations::none())
}

/// Audit one bundle against what the ledger says the allegation is.
///
/// Pure, and takes no key, no chain and no database: a committee member has
/// none of those for the node they are judging. `expect` comes from the dispute
/// record, never from the bundle.
///
/// One thing is still outside this function's reach, and it is on the registry
/// rather than in any bundle: whether the key the dispute names belongs to the
/// operator it is against. `--node` binds the evidence to the allegation; what
/// binds the allegation to a person is `registry.owner_of`.
pub fn verify_against(bundle: &Bundle, expect: &Expectations) -> Audit {
    let mut findings = Vec::new();

    let (signed, message) = match decode_and_check_signature(bundle, &mut findings) {
        Some(pair) => pair,
        None => {
            return Audit {
                verdict: Verdict::Unsigned,
                findings,
                signed: None,
                recomputed: None,
                observation_count: bundle.window.observations.len(),
                // Unanswerable rather than false: the bytes never decoded, so
                // there is nothing to compare the allegation against.
                bound_to_allegation: None,
            };
        }
    };

    let described_honestly = check_description(bundle, &message, &mut findings);
    let related = check_relevance(expect, &message, &signed, &mut findings);
    let observations = check_window(bundle, &message.feed, &mut findings);
    let recomputed = check_arithmetic(bundle, &message, &observations, &mut findings);

    // Ordered by what a failure means, not by how bad it sounds. A bundle whose
    // prose disagrees with its own signed bytes is judged on that before its
    // arithmetic is weighed, because the arithmetic is then arithmetic about a
    // claim nobody made. Relevance sits between the two for the same reason in
    // the other direction: arithmetic about another round is arithmetic about a
    // claim nobody is disputing.
    let verdict = if !described_honestly {
        Verdict::Misdescribed
    } else if !related {
        Verdict::Unrelated
    } else if recomputed.as_ref().is_some_and(|(_, ok)| *ok) {
        Verdict::Sound
    } else {
        Verdict::Unsupported
    };

    findings.push(Finding::new(
        Verdict::Sound,
        "not checked, and not checkable from a bundle: whether any observation was left \
         out. Four venues that agree look the same as four of six whose absent two would \
         have moved the median, and only the operator holds the full table.",
    ));

    findings.sort_by(|a, b| a.verdict.cmp(&b.verdict));

    Audit {
        verdict,
        findings,
        signed: Some(signed),
        recomputed: recomputed.map(|(p, _)| p.to_string()),
        observation_count: observations.len(),
        bound_to_allegation: (!expect.is_empty()).then_some(related),
    }
}

/// Is this bundle about the allegation, or about something else?
///
/// Every comparison is against the signed bytes. `signed` is the same thing
/// re-rendered, used only for the key, which is checked as bytes rather than as
/// the hex the bundle happens to have printed.
fn check_relevance(
    expect: &Expectations,
    message: &PriceMessage,
    signed: &Signed,
    findings: &mut Vec<Finding>,
) -> bool {
    if expect.is_empty() {
        findings.push(Finding::new(
            Verdict::Sound,
            "not checked, because nothing was given to check against: whether this bundle \
             is about the allegation at all. A sound bundle about another node, feed, \
             nonce or deployment is still a sound bundle, and handing one over is the \
             cheapest move available to a dishonest operator — it forges nothing. Read \
             the accused, feed and nonce off the dispute and pass them as --node, --feed \
             and --nonce.",
        ));
        return true;
    }

    let mut wrong = Vec::new();
    let mut checked = Vec::new();

    if let Some(node) = expect.node {
        let actual = hex::encode(node);
        if actual == signed.public_key {
            checked.push("node");
        } else {
            wrong.push(format!(
                "the allegation is against {actual}, this was signed by {}",
                signed.public_key
            ));
        }
    }
    if let Some(feed) = &expect.feed {
        if feed == &message.feed {
            checked.push("feed");
        } else {
            wrong.push(format!(
                "the allegation is about {feed}, this signs {}",
                message.feed
            ));
        }
    }
    if let Some(nonce) = expect.nonce {
        if nonce == message.nonce {
            checked.push("nonce");
        } else {
            wrong.push(format!(
                "the allegation is about nonce {nonce}, this signs nonce {}",
                message.nonce
            ));
        }
    }
    if let Some(aggregator) = expect.aggregator {
        let actual = hex::encode(aggregator);
        if actual == hex::encode(message.aggregator) {
            checked.push("deployment");
        } else {
            wrong.push(format!(
                "the allegation is on aggregator {actual}, this was signed for {}",
                hex::encode(message.aggregator)
            ));
        }
    }

    if wrong.is_empty() {
        findings.push(Finding::new(
            Verdict::Sound,
            format!(
                "the signed payload names the allegation given ({}) — though only as it \
                 was typed. These are worth nothing unless they were read off the dispute \
                 on the ledger rather than off the bundle.",
                checked.join(", ")
            ),
        ));
        return true;
    }

    findings.push(Finding::new(
        Verdict::Unrelated,
        format!(
            "this bundle is not about the allegation — {}. Everything else here may be \
             perfectly sound and none of it bears on this dispute; a genuine bundle for \
             the wrong round is the one answer an accused operator can give that requires \
             forging nothing.",
            wrong.join("; ")
        ),
    ));
    false
}

/// Steps 1 and 2: the bytes decode, and the signature covers them.
fn decode_and_check_signature(
    bundle: &Bundle,
    findings: &mut Vec<Finding>,
) -> Option<(Signed, PriceMessage)> {
    let message = match PriceMessage::from_hex(&bundle.provenance.message_hex) {
        Ok(m) => m,
        Err(e) => {
            findings.push(Finding::new(
                Verdict::Unsigned,
                format!("the payload is not an Aphelion price message: {e}"),
            ));
            return None;
        }
    };

    let key = match decode_key(&bundle.provenance.public_key) {
        Ok(k) => k,
        Err(e) => {
            findings.push(Finding::new(
                Verdict::Unsigned,
                format!("the stated public key is unusable: {e}"),
            ));
            return None;
        }
    };

    let signature = match decode_signature(&bundle.recorded.signature) {
        Ok(s) => s,
        Err(e) => {
            findings.push(Finding::new(
                Verdict::Unsigned,
                format!("the stated signature is unusable: {e}"),
            ));
            return None;
        }
    };

    if key
        .verify(&message.to_bytes(), &Signature::from_bytes(&signature))
        .is_err()
    {
        findings.push(Finding::new(
            Verdict::Unsigned,
            "the signature does not verify over the payload it is attached to. This bundle \
             establishes nothing: without it, every other number in the file is unsupported \
             assertion.",
        ));
        return None;
    }

    findings.push(Finding::new(
        Verdict::Sound,
        "the signature verifies over the payload, under the key the bundle names",
    ));

    Some((
        Signed {
            feed: message.feed.to_string(),
            price: message.price.to_string(),
            confidence_bps: message.confidence_bps,
            timestamp: message.timestamp,
            nonce: message.nonce,
            aggregator: hex::encode(message.aggregator),
            public_key: hex::encode(key.to_bytes()),
        },
        message,
    ))
}

/// Step 3: does the bundle's account of itself match the bytes it proved?
fn check_description(bundle: &Bundle, message: &PriceMessage, findings: &mut Vec<Finding>) -> bool {
    let mut disagreements = Vec::new();

    if bundle.recorded.feed != message.feed.as_str() {
        disagreements.push(format!(
            "says feed `{}`, signed `{}`",
            bundle.recorded.feed, message.feed
        ));
    }
    if bundle.recorded.nonce != message.nonce {
        disagreements.push(format!(
            "says nonce {}, signed {}",
            bundle.recorded.nonce, message.nonce
        ));
    }
    match Price::parse_decimal(&bundle.recorded.price) {
        Ok(p) if p.raw() == message.price.raw() => {}
        Ok(p) => disagreements.push(format!("says price {p}, signed {}", message.price)),
        Err(e) => disagreements.push(format!("states an unparseable price: {e}")),
    }
    if bundle.recorded.confidence_bps != message.confidence_bps {
        disagreements.push(format!(
            "says confidence {} bps, signed {} bps",
            bundle.recorded.confidence_bps, message.confidence_bps
        ));
    }
    if bundle.recorded.observed_at.timestamp() != message.timestamp as i64 {
        disagreements.push(format!(
            "says the observation was dated {}, signed {}",
            bundle.recorded.observed_at.timestamp(),
            message.timestamp
        ));
    }
    if bundle.provenance.aggregator != hex::encode(message.aggregator) {
        disagreements.push(format!(
            "says aggregator {}, signed {}",
            bundle.provenance.aggregator,
            hex::encode(message.aggregator)
        ));
    }

    if disagreements.is_empty() {
        findings.push(Finding::new(
            Verdict::Sound,
            "the bundle's description agrees with the payload it signed, field for field",
        ));
        return true;
    }

    findings.push(Finding::new(
        Verdict::Misdescribed,
        format!(
            "the bundle describes something other than what it signed — {}. A valid \
             signature over a payload the surrounding file misreports is the one failure \
             here that cannot be a clerical accident.",
            disagreements.join("; ")
        ),
    ));
    false
}

/// Step 5: are the observations consistent with the window they claim?
///
/// Returns the ones that parse, since the arithmetic has to run on something;
/// each rejection is reported rather than dropped quietly.
fn check_window(bundle: &Bundle, feed: &FeedId, findings: &mut Vec<Finding>) -> Vec<Observation> {
    let mut parsed = Vec::new();
    let mut complaints = Vec::new();

    for o in &bundle.window.observations {
        let price = match Price::parse_decimal(&o.price) {
            Ok(p) => p,
            Err(e) => {
                complaints.push(format!("`{}` has an unparseable price: {e}", o.source));
                continue;
            }
        };
        if o.received_at > bundle.window.as_of {
            complaints.push(format!(
                "`{}` is dated as received after the round was composed, so the round \
                 could not have read it",
                o.source
            ));
            continue;
        }
        if o.observed_at < bundle.window.cutoff {
            complaints.push(format!(
                "`{}` is older than the window the bundle states",
                o.source
            ));
            continue;
        }
        parsed.push(Observation {
            feed: feed.clone(),
            source: o.source.clone(),
            price,
            observed_at: o.observed_at,
            received_at: o.received_at,
        });
    }

    let mut sources: Vec<&str> = parsed.iter().map(|o| o.source.as_str()).collect();
    sources.sort_unstable();
    let before = sources.len();
    sources.dedup();
    if sources.len() != before {
        complaints.push(
            "a venue appears more than once. One round reads one price per venue, so a \
             repeated venue is a venue voting twice in the median"
                .into(),
        );
    }

    for c in complaints {
        findings.push(Finding::new(Verdict::Unsupported, c));
    }

    parsed
}

/// Step 4: do the observations produce the price that was *signed*?
fn check_arithmetic(
    bundle: &Bundle,
    message: &PriceMessage,
    observations: &[Observation],
    findings: &mut Vec<Finding>,
) -> Option<(Price, bool)> {
    let params = AggregationParams {
        min_sources: bundle.params.min_sources,
        max_source_deviation_bps: bundle.params.max_source_deviation_bps,
    };

    let agg = match aggregate(&message.feed, observations, params) {
        Ok(a) => a,
        Err(e) => {
            findings.push(Finding::new(
                Verdict::Unsupported,
                format!(
                    "the observations offered cannot be aggregated at all: {e}. A bundle \
                     too thin to reproduce its own price does not support it, whatever the \
                     reason it is thin."
                ),
            ));
            return None;
        }
    };

    let matches = agg.price.raw() == message.price.raw();
    if matches {
        findings.push(Finding::new(
            Verdict::Sound,
            format!(
                "the {} observation(s) offered produce {}, which is the price inside the \
                 signed payload",
                observations.len(),
                agg.price
            ),
        ));
    } else {
        findings.push(Finding::new(
            Verdict::Unsupported,
            format!(
                "the observations offered produce {}, not the signed {} — {} bps apart",
                agg.price,
                message.price,
                deviation_bps(agg.price.raw(), message.price.raw())
            ),
        ));
    }

    // Corroboration only. The confidence floor is configuration the bundle
    // merely asserts, so a disagreement here is not evidence of anything.
    let confidence = confidence_bps(&agg, bundle.params.confidence_floor_bps);
    if confidence != message.confidence_bps {
        findings.push(Finding::new(
            Verdict::Sound,
            format!(
                "confidence recomputes to {confidence} bps against a signed {} bps, under \
                 the floor the bundle states it used",
                message.confidence_bps
            ),
        ));
    }

    Some((agg.price, matches))
}

fn decode_key(hex_key: &str) -> Result<VerifyingKey, String> {
    let raw = hex::decode(hex_key.trim()).map_err(|e| format!("not hex ({e})"))?;
    let len = raw.len();
    let raw: [u8; 32] = raw
        .try_into()
        .map_err(|_| format!("{len} bytes, expected 32"))?;
    VerifyingKey::from_bytes(&raw).map_err(|e| format!("not a valid Ed25519 public key ({e})"))
}

fn decode_signature(hex_sig: &str) -> Result<[u8; 64], String> {
    let raw = hex::decode(hex_sig.trim()).map_err(|e| format!("not hex ({e})"))?;
    let len = raw.len();
    raw.try_into()
        .map_err(|_| format!("{len} bytes, expected 64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ed25519_dalek::{Signer, SigningKey};

    const AGGREGATOR: [u8; 32] = [7u8; 32];

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn at(unix: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(unix, 0).unwrap()
    }

    fn obs(source: &str, price: &str, observed: i64) -> BundleObservation {
        BundleObservation {
            source: source.into(),
            price: price.into(),
            observed_at: at(observed),
            received_at: at(observed),
        }
    }

    /// A bundle of the shape `replay --json` emits, for a round that reproduces.
    fn sound_bundle() -> Bundle {
        let message = PriceMessage {
            aggregator: AGGREGATOR,
            feed: FeedId::new("BTC_USD").unwrap(),
            price: Price::parse_decimal("100.00").unwrap(),
            timestamp: 950,
            confidence_bps: 10,
            nonce: 4,
        };
        let signature = key().sign(&message.to_bytes());

        Bundle {
            recorded: BundleRecorded {
                feed: "BTC_USD".into(),
                nonce: 4,
                price: message.price.to_string(),
                confidence_bps: 10,
                observed_at: at(950),
                signature: hex::encode(signature.to_bytes()),
            },
            provenance: BundleProvenance {
                public_key: hex::encode(key().verifying_key().to_bytes()),
                aggregator: hex::encode(AGGREGATOR),
                message_hex: message.to_hex(),
            },
            params: BundleParams {
                min_sources: 3,
                max_source_deviation_bps: 100,
                confidence_floor_bps: 10,
            },
            window: BundleWindow {
                cutoff: at(900),
                as_of: at(1_000),
                observations: vec![
                    obs("binance", "100.00", 950),
                    obs("kraken", "100.00", 960),
                    obs("coinbase", "100.00", 970),
                ],
            },
        }
    }

    fn detail_for(a: &Audit, v: Verdict) -> String {
        a.findings
            .iter()
            .find(|f| f.verdict == v)
            .map(|f| f.detail.clone())
            .unwrap_or_else(|| panic!("no finding at {v}: {:?}", a.findings))
    }

    #[test]
    fn a_sound_bundle_verifies() {
        let a = verify(&sound_bundle());
        assert_eq!(a.verdict, Verdict::Sound);
        assert_eq!(a.verdict.exit_code(), 0);
        let signed = a.signed.expect("established");
        assert_eq!(signed.feed, "BTC_USD");
        assert_eq!(signed.nonce, 4);
        assert_eq!(a.recomputed.as_deref(), Some("100.00000000"));
    }

    /// The check that makes the rest more than ceremony. A bundle can carry a
    /// perfectly valid signature over a payload saying one thing and prose
    /// saying another, and a verifier that checked the signature and then read
    /// the price out of the JSON beside it would accept it.
    #[test]
    fn a_bundle_that_signs_one_price_and_reports_another_is_caught() {
        let mut b = sound_bundle();
        // The signature and payload stay untouched and valid. Only the story
        // told around them changes -- and the observations are made to agree
        // with the story, which is what an attempt would look like.
        b.recorded.price = "150.00".into();
        b.window.observations = vec![
            obs("binance", "150.00", 950),
            obs("kraken", "150.00", 960),
            obs("coinbase", "150.00", 970),
        ];

        let a = verify(&b);

        assert_eq!(a.verdict, Verdict::Misdescribed);
        assert_eq!(a.verdict.exit_code(), 2);
        let detail = detail_for(&a, Verdict::Misdescribed);
        assert!(detail.contains("says price"), "{detail}");
        // And the audit reports the signed price, never the asserted one.
        assert_eq!(a.signed.unwrap().price, "100.00000000");
    }

    /// The same trick on the identifying fields, which is how a sound bundle
    /// about one round would be passed off as being about another.
    #[test]
    fn a_bundle_relabelled_to_another_feed_or_nonce_is_caught() {
        for mutate in [
            (|b: &mut Bundle| b.recorded.feed = "ETH_USD".into()) as fn(&mut Bundle),
            |b: &mut Bundle| b.recorded.nonce = 9,
            |b: &mut Bundle| b.provenance.aggregator = hex::encode([9u8; 32]),
            |b: &mut Bundle| b.recorded.observed_at = at(123),
            |b: &mut Bundle| b.recorded.confidence_bps = 999,
        ] {
            let mut b = sound_bundle();
            mutate(&mut b);
            assert_eq!(
                verify(&b).verdict,
                Verdict::Misdescribed,
                "a relabelled bundle must not pass"
            );
        }
    }

    /// Without this the whole exercise is the accused's software agreeing with
    /// itself.
    #[test]
    fn a_signature_from_the_wrong_key_establishes_nothing() {
        let mut b = sound_bundle();
        let other = SigningKey::from_bytes(&[43u8; 32]);
        b.provenance.public_key = hex::encode(other.verifying_key().to_bytes());

        let a = verify(&b);

        assert_eq!(a.verdict, Verdict::Unsigned);
        assert_eq!(a.verdict.exit_code(), 2);
        assert!(a.signed.is_none(), "nothing may be reported as established");
        assert!(a.recomputed.is_none());
    }

    /// A payload from another domain must not be readable as a price, or a
    /// signature over some other Aphelion message becomes a signature over a
    /// price of the forger's choosing.
    #[test]
    fn a_payload_that_is_not_a_price_message_establishes_nothing() {
        let mut b = sound_bundle();
        let mut raw = hex::decode(&b.provenance.message_hex).unwrap();
        raw[..17].copy_from_slice(b"APHELION_OTHER_V1");
        b.provenance.message_hex = hex::encode(&raw);
        // Sign the altered payload honestly: the key is real, the bytes are not
        // a price message.
        b.recorded.signature = hex::encode(key().sign(&raw).to_bytes());

        let a = verify(&b);

        assert_eq!(a.verdict, Verdict::Unsigned);
        assert!(detail_for(&a, Verdict::Unsigned).contains("not an Aphelion price message"));
    }

    #[test]
    fn a_malformed_key_or_signature_establishes_nothing() {
        let mut b = sound_bundle();
        b.recorded.signature = "abcd".into();
        assert_eq!(verify(&b).verdict, Verdict::Unsigned);

        let mut b = sound_bundle();
        b.provenance.public_key = "not hex".into();
        assert_eq!(verify(&b).verdict, Verdict::Unsigned);
    }

    /// Honestly signed and honestly described, and the numbers still do not add
    /// up. This is the finding a committee acts on.
    #[test]
    fn observations_that_do_not_produce_the_signed_price_are_unsupported() {
        let mut b = sound_bundle();
        b.window.observations = vec![
            obs("binance", "120.00", 950),
            obs("kraken", "120.00", 960),
            obs("coinbase", "120.00", 970),
        ];

        let a = verify(&b);

        assert_eq!(a.verdict, Verdict::Unsupported);
        assert_eq!(a.verdict.exit_code(), 1);
        assert!(detail_for(&a, Verdict::Unsupported).contains("not the signed"));
    }

    /// A bundle too thin to reproduce its own price does not support it,
    /// whatever the reason it is thin. The operator's own replay would have
    /// said so before they sent it.
    #[test]
    fn a_bundle_too_thin_to_aggregate_is_unsupported() {
        let mut b = sound_bundle();
        b.window.observations.truncate(1);
        assert_eq!(verify(&b).verdict, Verdict::Unsupported);
    }

    /// The bundle's own window bounds are held against it: an observation it
    /// admits arrived after the round is one the round could not have read.
    #[test]
    fn an_observation_the_bundle_admits_arrived_late_is_rejected() {
        let mut b = sound_bundle();
        let mut late = obs("okx", "100.00", 955);
        late.received_at = at(1_050);
        b.window.observations.push(late);

        let a = verify(&b);

        // Still sound overall -- the three real venues carry the price -- but
        // the late row is excluded and said to be excluded.
        assert_eq!(a.observation_count, 3);
        assert!(a
            .findings
            .iter()
            .any(|f| f.detail.contains("could not have read it")));
    }

    #[test]
    fn an_observation_older_than_the_stated_window_is_rejected() {
        let mut b = sound_bundle();
        b.window.observations.push(obs("okx", "100.00", 500));
        let a = verify(&b);
        assert_eq!(a.observation_count, 3);
        assert!(a.findings.iter().any(|f| f.detail.contains("older than")));
    }

    /// One venue, one price per round. A repeated venue is a venue voting twice
    /// in the median, which is the cheapest way to shift one.
    #[test]
    fn a_venue_that_appears_twice_is_reported() {
        let mut b = sound_bundle();
        b.window.observations.push(obs("binance", "100.00", 965));
        let a = verify(&b);
        assert!(
            a.findings
                .iter()
                .any(|f| f.detail.contains("more than once")),
            "{:?}",
            a.findings
        );
    }

    /// The limit has to be in the output, not just in the module docs. A
    /// committee told "sound" will otherwise reasonably hear "complete".
    #[test]
    fn the_audit_always_says_that_omission_cannot_be_detected() {
        for b in [sound_bundle(), {
            let mut b = sound_bundle();
            b.window.observations.truncate(1);
            b
        }] {
            let a = verify(&b);
            assert!(
                a.findings
                    .iter()
                    .any(|f| f.detail.contains("whether any observation was left out")),
                "the limit must be stated on every audit that judged anything"
            );
        }
    }

    /// A price the bundle states in a form that will not parse cannot be
    /// quietly treated as agreeing with the payload.
    #[test]
    fn an_unparseable_stated_price_is_a_disagreement_not_a_pass() {
        let mut b = sound_bundle();
        b.recorded.price = "one hundred".into();
        assert_eq!(verify(&b).verdict, Verdict::Misdescribed);
    }

    /// Worst first, so the grade can be read off the top.
    #[test]
    fn findings_are_ordered_worst_first() {
        let mut b = sound_bundle();
        b.recorded.price = "150.00".into();
        let a = verify(&b);
        let mut sorted = a.findings.clone();
        sorted.sort_by(|x, y| x.verdict.cmp(&y.verdict));
        assert_eq!(a.findings, sorted);
    }

    // -- is it about the allegation? -----------------------------------------

    /// What the whole dispute record's identifiers exist for. The bundle is
    /// genuine, reproduces, and answers a different question.
    #[test]
    fn a_genuine_bundle_for_another_round_does_not_answer_this_allegation() {
        let b = sound_bundle();
        // On its own terms it is beyond reproach.
        assert_eq!(verify(&b).verdict, Verdict::Sound);

        let expect = Expectations {
            nonce: Some(5),
            ..Expectations::none()
        };
        let a = verify_against(&b, &expect);

        assert_eq!(a.verdict, Verdict::Unrelated);
        assert_eq!(a.verdict.exit_code(), 2);
        assert_eq!(a.bound_to_allegation, Some(false));
        let detail = detail_for(&a, Verdict::Unrelated);
        assert!(detail.contains("nonce 5"), "{detail}");
        assert!(detail.contains("nonce 4"), "{detail}");
    }

    /// The same for each of the other three. A bundle can be the wrong node's,
    /// the wrong feed's, or another deployment's, and nothing inside it says so.
    #[test]
    fn a_bundle_about_another_node_feed_or_deployment_is_unrelated() {
        let other_key = SigningKey::from_bytes(&[43u8; 32])
            .verifying_key()
            .to_bytes();
        for expect in [
            Expectations {
                node: Some(other_key),
                ..Expectations::none()
            },
            Expectations {
                feed: Some(FeedId::new("ETH_USD").unwrap()),
                ..Expectations::none()
            },
            Expectations {
                aggregator: Some([8u8; 32]),
                ..Expectations::none()
            },
        ] {
            let a = verify_against(&sound_bundle(), &expect);
            assert_eq!(a.verdict, Verdict::Unrelated, "{expect:?}");
            assert_eq!(a.bound_to_allegation, Some(false));
        }
    }

    /// And the bundle that is what it was asked for passes, with every field
    /// named in the finding so the committee can see what was compared.
    #[test]
    fn an_allegation_the_signed_bytes_match_leaves_the_bundle_sound() {
        let expect = Expectations {
            node: Some(key().verifying_key().to_bytes()),
            feed: Some(FeedId::new("BTC_USD").unwrap()),
            nonce: Some(4),
            aggregator: Some(AGGREGATOR),
        };
        let a = verify_against(&sound_bundle(), &expect);

        assert_eq!(a.verdict, Verdict::Sound);
        assert_eq!(a.bound_to_allegation, Some(true));
        assert!(
            a.findings
                .iter()
                .any(|f| f.detail.contains("node, feed, nonce, deployment")),
            "{:?}",
            a.findings
        );
    }

    /// A committee member who has only the accused's key is not made to invent
    /// a nonce to get anything checked.
    #[test]
    fn an_allegation_given_in_part_checks_that_part() {
        let expect = Expectations {
            node: Some(key().verifying_key().to_bytes()),
            ..Expectations::none()
        };
        let a = verify_against(&sound_bundle(), &expect);
        assert_eq!(a.verdict, Verdict::Sound);
        assert_eq!(a.bound_to_allegation, Some(true));
    }

    /// The comparison is against the signed bytes, like every other one here.
    /// A bundle relabelled to look like the round under dispute is graded on
    /// the lie, not congratulated for the label.
    #[test]
    fn relabelling_a_bundle_to_match_the_allegation_does_not_make_it_match() {
        let mut b = sound_bundle();
        b.recorded.nonce = 5;

        let expect = Expectations {
            nonce: Some(5),
            ..Expectations::none()
        };
        let a = verify_against(&b, &expect);

        // Misdescribed outranks unrelated: a file that disagrees with its own
        // signed bytes is judged on that first.
        assert_eq!(a.verdict, Verdict::Misdescribed);
        assert_eq!(a.bound_to_allegation, Some(false));
    }

    /// Silence about relevance has to be visible. An audit that simply did not
    /// ask the question must not read like one that asked and was satisfied.
    #[test]
    fn an_audit_with_no_allegation_says_the_question_was_not_asked() {
        let a = verify(&sound_bundle());
        assert_eq!(a.verdict, Verdict::Sound);
        assert_eq!(a.bound_to_allegation, None);
        assert!(
            a.findings
                .iter()
                .any(|f| f.detail.contains("nothing was given to check against")
                    && f.detail.contains("--nonce")),
            "{:?}",
            a.findings
        );
    }

    /// A bundle whose bytes never decoded is unanswerable on relevance too,
    /// rather than false: there was nothing to compare the allegation against.
    #[test]
    fn an_unsigned_bundle_is_not_reported_as_being_about_the_wrong_round() {
        let mut b = sound_bundle();
        b.recorded.signature = hex::encode([0u8; 64]);
        let a = verify_against(
            &b,
            &Expectations {
                nonce: Some(4),
                ..Expectations::none()
            },
        );
        assert_eq!(a.verdict, Verdict::Unsigned);
        assert_eq!(a.bound_to_allegation, None);
    }

    /// The same separation, in the one place it reaches a script: the exit
    /// status. `verify-evidence` answers a usage error with 64 rather than 1
    /// precisely because 1 is taken, and every grade has to stay distinct from
    /// it for that to be worth doing.
    #[test]
    fn no_grade_occupies_the_code_reserved_for_the_callers_own_mistake() {
        for v in [
            Verdict::Sound,
            Verdict::Unsupported,
            Verdict::Unrelated,
            Verdict::Misdescribed,
            Verdict::Unsigned,
        ] {
            assert!(
                (0..=2).contains(&v.exit_code()),
                "{v} exits {}, outside the range the command documents",
                v.exit_code()
            );
        }
    }

    /// A typo in what the checker typed is the checker's mistake. Graded as a
    /// verdict it would read as a finding against the accused.
    #[test]
    fn an_allegation_that_will_not_parse_is_a_usage_error_not_a_verdict() {
        let e = Expectations::parse(Some("not-a-key"), None, None, None).unwrap_err();
        assert!(e.contains("--node"), "{e}");
        assert!(e.contains("32 bytes of hex"), "{e}");

        let e = Expectations::parse(None, Some("BTC/USD"), None, None).unwrap_err();
        assert!(e.contains("--feed"), "{e}");

        let e = Expectations::parse(None, None, None, Some("CNOTACONTRACT")).unwrap_err();
        assert!(e.contains("--aggregator"), "{e}");
    }

    /// Accepted however it was pasted, and the aggregator in either of the two
    /// forms it is written in: the deployment file gives a `C...` address, the
    /// signed payload carries the raw bytes.
    #[test]
    fn an_allegation_is_accepted_however_it_was_pasted() {
        let k = "AB".repeat(32);
        let e = Expectations::parse(Some(&format!("  0x{k}  ")), None, None, None).unwrap();
        assert_eq!(e.node, Some([0xabu8; 32]));

        let hex_form = Expectations::parse(None, None, None, Some(&"07".repeat(32))).unwrap();
        assert_eq!(hex_form.aggregator, Some(AGGREGATOR));

        // The `C...` address the harness's fixture deployment already pins.
        let strkey = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
        assert_eq!(
            Expectations::parse(None, None, None, Some(strkey))
                .unwrap()
                .aggregator,
            Some(crate::strkey::contract_id_bytes(strkey).unwrap())
        );
    }
}
