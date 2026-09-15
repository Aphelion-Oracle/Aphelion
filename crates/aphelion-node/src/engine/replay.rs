//! Replaying a published round from the observations that produced it.
//!
//! The README has claimed since Phase 1 that every raw observation is retained
//! so that any published price can be reproduced from its inputs, and that this
//! is what makes a dispute answerable with evidence. The retention was real and
//! [`super::aggregate`] was written as a pure function precisely so that this
//! would be possible. Nothing actually did it. An operator with a dispute filed
//! against them had a table of numbers and an invitation to write the SQL
//! themselves, at the point in their week when they can least afford to.
//!
//! This module answers two questions that are easy to conflate and are worth
//! keeping apart, because they fail for different reasons and a dispute turns
//! on which one failed.
//!
//! **Provenance.** Does the signature stored beside the round verify, under
//! this node's key and this aggregator's contract id, over the round as
//! recorded? That asks nothing about whether the price was *right* — only
//! whether the row is the one that was signed. It is checked with the public
//! key alone, never the secret, so the same check runs for a third party
//! holding nothing but an evidence bundle. A failure here means the database
//! has been edited since the round was signed, and every other number on the
//! page is worth exactly nothing.
//!
//! **Reproduction.** Re-run the aggregation over the observations the node
//! could have read at the time, and see whether the same price comes out. This
//! is the answer to the accusation itself.
//!
//! The two are ordered that way deliberately. A tampered row that happens to
//! reproduce is not a defence, and reporting "reproduced" over a signature that
//! does not verify would be the most misleading thing this command could say.
//!
//! ## What "the observations it could have read" means
//!
//! Not "observations dated before the round". The round loop ran a single query
//! — the freshest row per source inside `max_observation_age` — against the
//! table *as it stood at that instant*, so an observation is in the replay's
//! window only if it was both recent enough to qualify and had already been
//! received. A venue that reports a 14:00 price at 14:03, after the 14:01 round
//! has run, is dated inside that round's window and was invisible to it.
//! Selecting on `observed_at` alone silently adds it to the evidence, and the
//! replay then "reproduces" a price the node never had the inputs to compute —
//! which, in the direction that matters, means manufacturing a discrepancy for
//! an honest node to explain.
//!
//! That rule is [`visible_at`], and it is a function here rather than a `WHERE`
//! clause in [`crate::db::Repo`] for one reason: it decides whether a dispute is
//! answerable, and no test in this repository can execute SQL.
//!
//! ## What a mismatch does not prove
//!
//! A great deal about a round is not recorded next to it. `min_sources`,
//! `max_source_deviation_bps` and the confidence floor live in the
//! configuration, and a round from before an operator retuned them was computed
//! under numbers this replay cannot recover. Neither is the ledger time the
//! round saw. So a divergence is reported as a divergence and the plausible
//! innocent explanations are printed beside it, rather than being graded into
//! an accusation. The one thing this module refuses to do is round a difference
//! down to "close enough": the price either came back byte-identical or it did
//! not.

use aphelion_core::{deviation_bps, FeedId, Price, PriceMessage};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;

use super::aggregate::{aggregate, confidence_bps, AggregationParams};
use crate::db::{Observation, RecordedRound};
use crate::error::NodeError;

/// Everything the replay needs that is not in the database.
///
/// All of it comes from the configuration as it stands *now*, which is the
/// whole reason [`Replay::findings`] hedges a divergence: these are not the
/// values the round was necessarily computed under.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub aggregation: AggregationParams,
    /// `feeds[].confidence_bps` — a floor, not a constant. See
    /// [`super::aggregate::confidence_bps`].
    pub confidence_floor: u32,
    /// The aggregator the node signs for. Bound into the payload, so it is
    /// part of what the provenance check proves.
    pub aggregator: [u8; 32],
}

/// The grade, worst first so a list of findings sorts by sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The stored signature does not verify over the stored round. Nothing
    /// else on the page can be trusted, including a price that reproduces.
    Tampered,
    /// The observations reproduce a different price than the one published.
    Diverged,
    /// Too little of the window survives to reproduce anything. Not a
    /// divergence — an absence of evidence, which usually means retention.
    Incomplete,
    /// The signature verifies and the arithmetic comes back identical.
    Reproduced,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tampered => "tampered",
            Self::Diverged => "diverged",
            Self::Incomplete => "incomplete",
            Self::Reproduced => "reproduced",
        }
    }

    /// Zero only for a clean reproduction, so a monitor that treats non-zero
    /// as failure is right without knowing the grades, and one that wants to
    /// tell "cannot answer" from "answered badly" can read the code.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Reproduced => 0,
            Self::Incomplete => 1,
            Self::Diverged | Self::Tampered => 2,
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing worth saying about the replay, and how much it matters.
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

/// The round as the database has it.
#[derive(Debug, Clone, Serialize)]
pub struct Recorded {
    pub feed: FeedId,
    pub nonce: u64,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub confidence_bps: u32,
    pub source_count: u32,
    pub spread_bps: u32,
    #[serde(serialize_with = "ser_price")]
    pub stddev: Price,
    /// The signed timestamp: the age of the data, not of the round.
    pub observed_at: DateTime<Utc>,
    pub status: String,
    pub tx_hash: Option<String>,
    pub signature: String,
    pub created_at: DateTime<Utc>,
}

/// Whether the row is the one that was signed.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub verified: bool,
    pub public_key: String,
    pub aggregator: String,
    /// The canonical 117-byte payload, hex encoded.
    ///
    /// This is the single most useful line in an evidence bundle: it is
    /// exactly what the contract verified, so a counterparty can check the
    /// signature without running any of this code, or agreeing with any of it.
    pub message_hex: String,
    /// Set when the stored signature is not 64 bytes of hex — a different
    /// failure from a signature that is well-formed and wrong.
    pub malformed: Option<String>,
}

/// The window of observations the round could actually have read.
#[derive(Debug, Clone, Serialize)]
pub struct Window {
    /// Oldest `observed_at` that qualified.
    pub cutoff: DateTime<Utc>,
    /// The instant the window was taken as of — the round's `created_at`.
    pub as_of: DateTime<Utc>,
    pub max_observation_age_secs: u64,
    pub observations: Vec<Observation>,
}

/// One source that survived filtering.
#[derive(Debug, Clone, Serialize)]
pub struct Used {
    pub source: String,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub deviation_bps: u32,
    pub observed_at: DateTime<Utc>,
}

/// One source that did not, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Dropped {
    pub source: String,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub deviation_bps: u32,
    pub reason: String,
}

/// What re-running the aggregation produced.
#[derive(Debug, Clone, Serialize)]
pub struct Recomputed {
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub confidence_bps: u32,
    pub source_count: u32,
    pub spread_bps: u32,
    #[serde(serialize_with = "ser_price")]
    pub stddev: Price,
    /// The oldest surviving observation, *unclamped*.
    ///
    /// The round signed `min(this, ledger_time)` and the ledger time it saw is
    /// not recorded, so this being later than what was published is the
    /// ordinary case of a node whose data ran ahead of the ledger, not a
    /// discrepancy. See [`super::aggregate::Aggregated::observed_at`].
    pub observed_at: DateTime<Utc>,
    pub used: Vec<Used>,
    pub discarded: Vec<Dropped>,
    /// The only comparison that carries a verdict.
    pub price_matches: bool,
    /// Distance from the published price, for a divergence worth sizing.
    pub deviation_from_recorded_bps: u32,
}

/// The parameters the recomputation ran under, recorded in the bundle.
///
/// Without these a bundle cannot be checked by anybody else: the same
/// observations produce different medians under different filters, so a
/// verifier handed only the numbers would have to guess at the arithmetic and
/// would be entitled to reach any answer it liked. They are a *claim* — nothing
/// binds them to the round, which is exactly why they are stated openly rather
/// than left implicit for a verifier to assume.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ParamsUsed {
    pub min_sources: usize,
    pub max_source_deviation_bps: u32,
    pub confidence_floor_bps: u32,
}

/// A round, re-derived.
#[derive(Debug, Clone, Serialize)]
pub struct Replay {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub recorded: Recorded,
    pub provenance: Provenance,
    pub params: ParamsUsed,
    pub window: Window,
    /// `None` when the surviving window could not be aggregated at all; the
    /// reason is in `findings`.
    pub recomputed: Option<Recomputed>,
}

/// The observations a round could actually have read, from the candidates
/// dated inside its window.
///
/// Two rules, and the first is the one worth stating out loud.
///
/// **Received by then.** The round loop queried the table as it stood at that
/// instant, so a row this node had not yet received was not there to be
/// selected — however early the venue says the price was observed. A venue that
/// reports a 14:00 price at 14:03 is dated inside the 14:01 round's window and
/// was invisible to it. Admitting it does not produce a harmless inaccuracy: it
/// re-runs the round against inputs the node never had, and in the direction
/// that matters that means manufacturing a discrepancy an honest operator is
/// then asked to account for. This is why the rule is a function with tests
/// rather than a clause in a query no test here can run.
///
/// **Freshest per source.** What `DISTINCT ON (source)` does for the live
/// round: one row per venue, so an exchange polled every second cannot outvote
/// a slower one inside a single round.
///
/// `candidates` is expected ordered by source then `observed_at` descending —
/// as [`crate::db::Repo::observations_in_window`] returns it — but the tie-break
/// does not rely on that: the maximum is taken explicitly.
pub fn visible_at(candidates: &[Observation], as_of: DateTime<Utc>) -> Vec<Observation> {
    let mut freshest: Vec<Observation> = Vec::new();

    for obs in candidates.iter().filter(|o| o.received_at <= as_of) {
        match freshest.iter_mut().find(|k| k.source == obs.source) {
            // Strictly later, so the first of two rows with identical
            // timestamps wins and the selection stays a function of the set
            // rather than of the order it arrived in.
            Some(kept) if obs.observed_at > kept.observed_at => *kept = obs.clone(),
            Some(_) => {}
            None => freshest.push(obs.clone()),
        }
    }

    freshest.sort_by(|a, b| a.source.cmp(&b.source));
    freshest
}

/// Reproduce one round.
///
/// Pure: everything it needs has already been read. That is what lets the
/// interesting cases — a tampered row, a pruned window, a genuine divergence —
/// be tested against struct literals rather than against a Postgres instance
/// and a time machine, the same split [`super::status`] and [`super::duty`]
/// use.
pub fn replay(
    round: &RecordedRound,
    observations: &[Observation],
    window: Window,
    params: Params,
    public_key: &VerifyingKey,
) -> Replay {
    let recorded = Recorded {
        feed: round.feed.clone(),
        nonce: round.nonce,
        price: round.price,
        confidence_bps: round.confidence_bps,
        source_count: round.source_count,
        spread_bps: round.spread_bps,
        stddev: round.stddev,
        observed_at: round.observed_at,
        status: round.status.clone(),
        tx_hash: round.tx_hash.clone(),
        signature: round.signature.clone(),
        created_at: round.created_at,
    };

    let mut findings = Vec::new();
    let provenance = check_provenance(&recorded, params.aggregator, public_key, &mut findings);
    let recomputed = reproduce(&recorded, observations, params, &mut findings);

    // Provenance outranks everything. A row that has been edited since it was
    // signed can reproduce by construction -- someone who changed the price
    // could change the observations too -- so a clean reproduction on top of a
    // broken signature is the one result that must not read as reassuring.
    let verdict = if !provenance.verified {
        Verdict::Tampered
    } else {
        match &recomputed {
            None => Verdict::Incomplete,
            Some(r) if r.price_matches => Verdict::Reproduced,
            Some(_) => Verdict::Diverged,
        }
    };

    findings.sort_by(|a, b| a.verdict.cmp(&b.verdict));

    Replay {
        verdict,
        findings,
        recorded,
        provenance,
        params: ParamsUsed {
            min_sources: params.aggregation.min_sources,
            max_source_deviation_bps: params.aggregation.max_source_deviation_bps,
            confidence_floor_bps: params.confidence_floor,
        },
        window,
        recomputed,
    }
}

/// Is this row the one that was signed?
fn check_provenance(
    recorded: &Recorded,
    aggregator: [u8; 32],
    public_key: &VerifyingKey,
    findings: &mut Vec<Finding>,
) -> Provenance {
    let message = PriceMessage {
        aggregator,
        feed: recorded.feed.clone(),
        price: recorded.price,
        timestamp: recorded.observed_at.timestamp().max(0) as u64,
        confidence_bps: recorded.confidence_bps,
        nonce: recorded.nonce,
    };
    let message_hex = message.to_hex();

    let mut malformed = None;
    let verified = match decode_signature(&recorded.signature) {
        Ok(sig) => public_key
            .verify(&message.to_bytes(), &Signature::from_bytes(&sig))
            .is_ok(),
        Err(why) => {
            malformed = Some(why);
            false
        }
    };

    if let Some(why) = &malformed {
        findings.push(Finding::new(
            Verdict::Tampered,
            format!("the stored signature is not a signature: {why}"),
        ));
    } else if !verified {
        findings.push(Finding::new(
            Verdict::Tampered,
            "the stored signature does not verify over the stored round under this node's key. \
             Either a column has been edited since the round was signed, or the key or \
             aggregator contract configured now is not the one that signed it — check \
             `network.aggregator_contract` before concluding the former.",
        ));
    } else {
        findings.push(Finding::new(
            Verdict::Reproduced,
            "the stored signature verifies over the stored round: this row is the one that \
             was signed",
        ));
    }

    Provenance {
        verified,
        public_key: hex::encode(public_key.to_bytes()),
        aggregator: hex::encode(aggregator),
        message_hex,
        malformed,
    }
}

fn decode_signature(hex_sig: &str) -> Result<[u8; 64], String> {
    let raw = hex::decode(hex_sig).map_err(|e| format!("not hex ({e})"))?;
    let len = raw.len();
    raw.try_into()
        .map_err(|_| format!("{len} bytes, expected 64"))
}

/// Re-run the aggregation and compare.
fn reproduce(
    recorded: &Recorded,
    observations: &[Observation],
    params: Params,
    findings: &mut Vec<Finding>,
) -> Option<Recomputed> {
    if observations.is_empty() {
        findings.push(Finding::new(
            Verdict::Incomplete,
            "no observations survive in this round's window. Raw observations are pruned on \
             `retention.raw_prices`, so a round older than that window cannot be replayed at \
             all — this says nothing about whether the price was right.",
        ));
        return None;
    }

    let agg = match aggregate(&recorded.feed, observations, params.aggregation) {
        Ok(a) => a,
        Err(e @ NodeError::InsufficientSources { .. }) => {
            findings.push(Finding::new(
                Verdict::Incomplete,
                format!(
                    "{e}. The window retains {} observation(s), which is fewer than the \
                     configured minimum, so the aggregation cannot be re-run. Partial \
                     retention looks exactly like this.",
                    observations.len()
                ),
            ));
            return None;
        }
        Err(e) => {
            findings.push(Finding::new(
                Verdict::Incomplete,
                format!("the aggregation could not be re-run: {e}"),
            ));
            return None;
        }
    };

    let confidence = confidence_bps(&agg, params.confidence_floor);
    let price_matches = agg.price.raw() == recorded.price.raw();
    let drift = deviation_bps(agg.price.raw(), recorded.price.raw());

    if price_matches {
        findings.push(Finding::new(
            Verdict::Reproduced,
            format!(
                "re-running the aggregation over {} retained observation(s) produces {} — \
                 the published price, exactly",
                observations.len(),
                agg.price
            ),
        ));
    } else {
        findings.push(Finding::new(
            Verdict::Diverged,
            format!(
                "the retained observations produce {}, not the published {} — {drift} bps \
                 apart. Before reading that as a bad round: `min_sources`, \
                 `max_source_deviation_bps` and the confidence floor are configuration and \
                 are not recorded beside a round, so a round composed before those were \
                 retuned was computed under numbers this replay cannot recover.",
                agg.price, recorded.price
            ),
        ));
    }

    // Everything below is corroboration, never a verdict of its own. Each one
    // has an ordinary explanation, and an operator reading the page is better
    // served by the explanation than by a second grade to interpret.
    let recomputed_observed_at = agg.observed_at(u64::MAX) as i64;
    let recorded_observed_at = recorded.observed_at.timestamp();
    if recomputed_observed_at != recorded_observed_at {
        let note = if recomputed_observed_at > recorded_observed_at {
            "the round signed the earlier of the two because ledger time was behind its data, \
             which is the ordinary case and is not recorded"
        } else {
            "the replay's oldest survivor is older than the timestamp published, which means \
             the window here is not the window the round saw"
        };
        findings.push(Finding::new(
            Verdict::Reproduced,
            format!(
                "signed timestamp {recorded_observed_at} against a recomputed \
                 {recomputed_observed_at}: {note}"
            ),
        ));
    }

    if confidence != recorded.confidence_bps {
        findings.push(Finding::new(
            Verdict::Reproduced,
            format!(
                "confidence {} bps published against {confidence} bps recomputed: the floor \
                 is `feeds[].confidence_bps` and may have been retuned since",
                recorded.confidence_bps
            ),
        ));
    }

    if agg.used.len() as u32 != recorded.source_count {
        findings.push(Finding::new(
            Verdict::Reproduced,
            format!(
                "{} source(s) contributed to the published round against {} here: partial \
                 retention removes sources from the window without changing the median that \
                 survives it",
                recorded.source_count,
                agg.used.len()
            ),
        ));
    }

    Some(Recomputed {
        price: agg.price,
        confidence_bps: confidence,
        source_count: agg.used.len() as u32,
        spread_bps: agg.spread_bps,
        stddev: agg.stddev,
        observed_at: DateTime::from_timestamp(recomputed_observed_at, 0)
            .unwrap_or(recorded.created_at),
        used: agg
            .used
            .iter()
            .map(|s| Used {
                source: s.name.clone(),
                price: s.price,
                deviation_bps: s.deviation_bps,
                observed_at: s.observed_at,
            })
            .collect(),
        discarded: agg
            .discarded
            .iter()
            .map(|d| Dropped {
                source: d.name.clone(),
                price: d.price,
                deviation_bps: d.deviation_bps,
                reason: d.reason.to_string(),
            })
            .collect(),
        price_matches,
        deviation_from_recorded_bps: drift,
    })
}

/// Prices serialise as decimal strings, never as JSON numbers: an evidence
/// bundle parsed into an IEEE double loses the low digits of a large price,
/// which is the one thing a dispute is arguing about.
fn ser_price<S: serde::Serializer>(p: &Price, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ed25519_dalek::{Signer, SigningKey};

    const AGGREGATOR: [u8; 32] = [7u8; 32];

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    fn at(unix: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(unix, 0).unwrap()
    }

    fn key() -> SigningKey {
        // Fixed, so a failure is reproducible rather than one run in a
        // thousand. Nothing here is secret: it signs test rounds.
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn params() -> Params {
        Params {
            aggregation: AggregationParams {
                min_sources: 3,
                max_source_deviation_bps: 100,
            },
            confidence_floor: 10,
            aggregator: AGGREGATOR,
        }
    }

    fn obs(source: &str, price: &str, observed: i64) -> Observation {
        Observation {
            feed: feed(),
            source: source.into(),
            price: Price::parse_decimal(price).unwrap(),
            observed_at: at(observed),
            // Received when observed unless a test says otherwise: the lag is
            // the thing under test in `a_late_arrival_...`, and noise here.
            received_at: at(observed),
        }
    }

    fn window(observations: &[Observation]) -> Window {
        Window {
            cutoff: at(900),
            as_of: at(1_000),
            max_observation_age_secs: 100,
            observations: observations.to_vec(),
        }
    }

    /// A round carrying a genuine signature over its own contents.
    fn signed_round(
        price: &str,
        confidence_bps: u32,
        observed_at: i64,
        nonce: u64,
    ) -> RecordedRound {
        let price = Price::parse_decimal(price).unwrap();
        let message = PriceMessage {
            aggregator: AGGREGATOR,
            feed: feed(),
            price,
            timestamp: observed_at as u64,
            confidence_bps,
            nonce,
        };
        let signature = key().sign(&message.to_bytes());
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
            signature: hex::encode(signature.to_bytes()),
            status: "submitted".into(),
            tx_hash: Some("deadbeef".into()),
            error: None,
            created_at: at(1_000),
        }
    }

    /// The whole point of the command: an honest round answers for itself.
    #[test]
    fn an_honest_round_reproduces_from_its_observations() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        // Three identical prices: median 100, spread 0, so the confidence is
        // the floor and the signed timestamp is the oldest survivor.
        let round = signed_round("100.00", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Reproduced);
        assert!(r.provenance.verified);
        let c = r.recomputed.expect("aggregated");
        assert!(c.price_matches);
        assert_eq!(c.price.to_string(), "100.00000000");
        assert_eq!(c.source_count, 3);
        assert_eq!(c.deviation_from_recorded_bps, 0);
    }

    /// The evidence bundle's load-bearing line: the canonical payload is the
    /// one the contract verified, so a counterparty can check the signature
    /// without running any of this.
    #[test]
    fn the_reported_payload_is_the_one_the_signature_covers() {
        let round = signed_round("100.00", 10, 950, 4);
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        let payload = hex::decode(&r.provenance.message_hex).unwrap();
        assert_eq!(payload.len(), aphelion_core::message::MESSAGE_LEN);
        let sig = hex::decode(&round.signature).unwrap();
        let sig: [u8; 64] = sig.try_into().unwrap();
        assert!(key()
            .verifying_key()
            .verify(&payload, &Signature::from_bytes(&sig))
            .is_ok());
    }

    /// An edited row must not be able to launder itself into a defence by
    /// bringing observations that agree with the edit.
    #[test]
    fn an_edited_price_reads_as_tampered_even_though_it_reproduces() {
        // Observations that reproduce 200 exactly, and a round whose signature
        // covers a *different* price than the one stored beside it: precisely
        // what editing `price_raw` after the fact produces.
        let observations = vec![
            obs("binance", "200.00", 950),
            obs("kraken", "200.00", 960),
            obs("coinbase", "200.00", 970),
        ];
        let mut round = signed_round("100.00", 10, 950, 4);
        round.price = Price::parse_decimal("200.00").unwrap();

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        // The arithmetic agrees with the row, and that is not a defence.
        assert!(r.recomputed.as_ref().unwrap().price_matches);
        assert!(!r.provenance.verified);
        assert_eq!(r.verdict, Verdict::Tampered);
        assert_eq!(r.verdict.exit_code(), 2);
    }

    /// A signature from the right key over the wrong aggregator is not a
    /// forgery, and the finding has to say so — otherwise an operator who has
    /// repointed their node reads "tampered" and panics.
    #[test]
    fn a_signature_for_another_aggregator_names_the_configuration_as_a_cause() {
        let round = signed_round("100.00", 10, 950, 4);
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        let mut p = params();
        p.aggregator = [9u8; 32]; // a different deployment

        let r = replay(
            &round,
            &observations,
            window(&observations),
            p,
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Tampered);
        let detail = r
            .findings
            .iter()
            .find(|f| f.verdict == Verdict::Tampered)
            .map(|f| f.detail.clone())
            .unwrap();
        assert!(detail.contains("aggregator_contract"), "{detail}");
    }

    /// A signature that is not 64 bytes of hex is a different failure from one
    /// that is well-formed and wrong, and is reported as one.
    #[test]
    fn a_malformed_signature_is_named_as_malformed() {
        let mut round = signed_round("100.00", 10, 950, 4);
        round.signature = "not hex at all".into();
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Tampered);
        assert!(r.provenance.malformed.is_some());
    }

    /// Retention is not an accusation. A window with nothing left in it must
    /// grade as unanswerable, never as a bad round.
    #[test]
    fn a_pruned_window_is_incomplete_rather_than_diverged() {
        let round = signed_round("100.00", 10, 950, 4);
        let r = replay(&round, &[], window(&[]), params(), &key().verifying_key());

        assert_eq!(r.verdict, Verdict::Incomplete);
        assert_eq!(r.verdict.exit_code(), 1);
        assert!(r.recomputed.is_none());
        assert!(r.provenance.verified, "the signature still stands");
        let detail = r
            .findings
            .iter()
            .find(|f| f.verdict == Verdict::Incomplete)
            .map(|f| f.detail.clone())
            .unwrap();
        assert!(detail.contains("retention"), "{detail}");
    }

    /// The same, one step less obvious: enough rows survive to look like
    /// evidence, but fewer than the aggregation needs.
    #[test]
    fn a_partly_pruned_window_is_also_incomplete() {
        let observations = vec![obs("binance", "100.00", 950), obs("kraken", "100.00", 960)];
        let round = signed_round("100.00", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Incomplete);
        assert!(r.recomputed.is_none());
    }

    /// And the case the command exists to be able to say out loud.
    #[test]
    fn a_price_the_observations_do_not_support_is_reported_as_diverged() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        // Signed honestly -- so provenance passes -- over a price the inputs
        // do not produce. A node that really did publish a bad number looks
        // exactly like this.
        let round = signed_round("110.00", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert!(r.provenance.verified);
        assert_eq!(r.verdict, Verdict::Diverged);
        let c = r.recomputed.unwrap();
        assert!(!c.price_matches);
        assert_eq!(c.price.to_string(), "100.00000000");
        // Measured against the published price as the reference: 10/110.
        assert_eq!(c.deviation_from_recorded_bps, 909);
    }

    /// A divergence never arrives unexplained. The findings have to carry the
    /// innocent readings, because the operator reading them is the one who has
    /// to decide which applies.
    #[test]
    fn a_divergence_says_what_else_could_explain_it() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        let round = signed_round("110.00", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        let detail = r
            .findings
            .iter()
            .find(|f| f.verdict == Verdict::Diverged)
            .map(|f| f.detail.clone())
            .unwrap();
        assert!(detail.contains("min_sources"), "{detail}");
        assert!(detail.contains("max_source_deviation_bps"), "{detail}");
    }

    /// An outlier is excluded with its reason, which is the other half of the
    /// answer to "why is my node's price different from everyone else's?".
    #[test]
    fn a_discarded_source_appears_in_the_evidence_with_its_reason() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
            obs("okx", "500.00", 980),
        ];
        let round = signed_round("100.00", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Reproduced);
        let c = r.recomputed.unwrap();
        assert_eq!(c.used.len(), 3);
        assert_eq!(c.discarded.len(), 1);
        assert_eq!(c.discarded[0].source, "okx");
        assert!(!c.discarded[0].reason.is_empty());
    }

    /// The timestamp is corroboration, not a verdict. A round whose data ran
    /// ahead of ledger time signed the earlier of the two, and the ledger time
    /// it saw is not recorded — so this must not downgrade a good round.
    #[test]
    fn a_timestamp_clamped_to_ledger_time_does_not_cost_the_verdict() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        // Published 950 by the observations, but signed 940: ledger time was
        // behind the data and the round clamped to it.
        let round = signed_round("100.00", 10, 940, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Reproduced);
        assert!(
            r.findings
                .iter()
                .any(|f| f.detail.contains("ledger time was behind")),
            "the difference should be reported, just not graded: {:?}",
            r.findings
        );
    }

    /// A confidence floor retuned since the round is likewise explained rather
    /// than graded.
    #[test]
    fn a_retuned_confidence_floor_does_not_cost_the_verdict() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        let round = signed_round("100.00", 25, 950, 4);
        let mut p = params();
        p.confidence_floor = 10; // lowered since

        let r = replay(
            &round,
            &observations,
            window(&observations),
            p,
            &key().verifying_key(),
        );

        assert_eq!(r.verdict, Verdict::Reproduced);
        assert!(r
            .findings
            .iter()
            .any(|f| f.detail.contains("confidence_bps")));
    }

    /// Worst first, so a caller can read the grade off the top of the list.
    #[test]
    fn findings_are_ordered_worst_first() {
        let observations = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
        ];
        let round = signed_round("110.00", 25, 940, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );

        let mut sorted = r.findings.clone();
        sorted.sort_by(|a, b| a.verdict.cmp(&b.verdict));
        assert_eq!(r.findings, sorted);
        assert_eq!(r.findings[0].verdict, Verdict::Diverged);
    }

    /// Prices leave as decimal strings. An evidence bundle parsed into an IEEE
    /// double loses the low digits of a large price, which is the one number a
    /// dispute is arguing about.
    #[test]
    fn prices_serialise_as_strings_not_json_numbers() {
        let observations = vec![
            obs("binance", "64231.55", 950),
            obs("kraken", "64231.55", 960),
            obs("coinbase", "64231.55", 970),
        ];
        let round = signed_round("64231.55", 10, 950, 4);

        let r = replay(
            &round,
            &observations,
            window(&observations),
            params(),
            &key().verifying_key(),
        );
        let json: serde_json::Value = serde_json::to_value(&r).unwrap();

        assert_eq!(
            json["recorded"]["price"],
            serde_json::json!("64231.55000000")
        );
        assert_eq!(
            json["recomputed"]["price"],
            serde_json::json!("64231.55000000")
        );
        assert!(json["recomputed"]["used"][0]["price"].is_string());
    }

    // -- the visibility rule ------------------------------------------------
    //
    // The part of the replay most able to be quietly wrong, and the reason it
    // is a function here rather than a `WHERE` clause.

    /// The load-bearing one. A late arrival is dated inside the window and was
    /// invisible to the round, and letting it in invents a discrepancy.
    #[test]
    fn an_observation_received_after_the_round_is_not_evidence_against_it() {
        let mut late = obs("okx", "500.00", 955);
        late.received_at = at(1_030); // delivered half a minute after the round

        let candidates = vec![
            obs("binance", "100.00", 950),
            obs("kraken", "100.00", 960),
            obs("coinbase", "100.00", 970),
            late,
        ];

        let visible = visible_at(&candidates, at(1_000));

        assert_eq!(
            visible.len(),
            3,
            "the late row must not appear: {visible:?}"
        );
        assert!(!visible.iter().any(|o| o.source == "okx"));

        // And the round it would have wrecked still reproduces.
        let round = signed_round("100.00", 10, 950, 4);
        let r = replay(
            &round,
            &visible,
            window(&visible),
            params(),
            &key().verifying_key(),
        );
        assert_eq!(r.verdict, Verdict::Reproduced);
    }

    /// Received exactly on the boundary counts as received: the round read the
    /// table at that instant.
    #[test]
    fn an_observation_received_exactly_at_the_boundary_is_visible() {
        let mut edge = obs("okx", "100.00", 940);
        edge.received_at = at(1_000);
        let visible = visible_at(&[edge], at(1_000));
        assert_eq!(visible.len(), 1);
    }

    /// One row per venue, so an exchange polled every second cannot outvote a
    /// slower one inside a single round.
    #[test]
    fn only_the_freshest_row_per_source_survives() {
        let candidates = vec![
            obs("binance", "101.00", 990),
            obs("binance", "100.00", 950),
            obs("binance", "99.00", 910),
            obs("kraken", "100.00", 960),
        ];

        let visible = visible_at(&candidates, at(1_000));

        assert_eq!(visible.len(), 2);
        let binance = visible.iter().find(|o| o.source == "binance").unwrap();
        assert_eq!(binance.price.to_string(), "101.00000000");
        assert_eq!(binance.observed_at, at(990));
    }

    /// A venue whose only recent row arrived late drops out of the window
    /// entirely rather than falling back to a stale one it had already
    /// superseded... except that a stale one inside the age window is exactly
    /// what the round would have used, so it is kept.
    #[test]
    fn a_source_falls_back_to_the_freshest_row_that_had_arrived() {
        let mut late = obs("binance", "101.00", 990);
        late.received_at = at(1_020);
        let candidates = vec![late, obs("binance", "100.00", 950)];

        let visible = visible_at(&candidates, at(1_000));

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].price.to_string(), "100.00000000");
    }

    /// The selection is a function of the set, not of the order rows arrive in.
    /// A replay that depended on row order would be a replay whose answer
    /// changed when Postgres changed its plan.
    #[test]
    fn the_window_does_not_depend_on_the_order_of_the_candidates() {
        let candidates = vec![
            obs("kraken", "100.00", 960),
            obs("binance", "101.00", 990),
            obs("coinbase", "100.00", 970),
            obs("binance", "100.00", 950),
        ];
        let mut reversed = candidates.clone();
        reversed.reverse();

        assert_eq!(
            visible_at(&candidates, at(1_000))
                .iter()
                .map(|o| (o.source.clone(), o.price.to_string()))
                .collect::<Vec<_>>(),
            visible_at(&reversed, at(1_000))
                .iter()
                .map(|o| (o.source.clone(), o.price.to_string()))
                .collect::<Vec<_>>()
        );
    }

    /// A venue with a fast clock dates an observation slightly ahead of the
    /// round that read it. The live query has no upper bound on `observed_at`,
    /// so neither does this: the round genuinely saw it.
    #[test]
    fn a_future_dated_observation_that_had_arrived_is_still_visible() {
        let mut ahead = obs("binance", "100.00", 1_010);
        ahead.received_at = at(995);
        let visible = visible_at(&[ahead], at(1_000));
        assert_eq!(visible.len(), 1, "a fast venue clock is not a late arrival");
    }
}
