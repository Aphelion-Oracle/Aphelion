//! Reading and acting on the slashing contract.
//!
//! # Why this is not part of `ChainClient`
//!
//! [`super::ChainClient`] is what the round loop holds: ledger time, a
//! submission, this node's record. Disputes and elections are on a different
//! clock entirely — a voting period is days, a term is months — and nothing in
//! the round loop has any business with them. Folding them into one trait
//! would put a dozen methods that the collector, the aggregator and the mock
//! chain all have to carry and none of them will ever call.
//!
//! They are also authorised by a *different account*. Submissions are paid for
//! by the relayer account, which deliberately holds no authority over the
//! node's identity; committee actions are authorised by the account that
//! bonded the stake, because that is who the registry will answer `owner_of`
//! with. Two traits make that separation visible rather than something an
//! operator discovers from a `require_auth` failure.
//!
//! # Why the node has this at all
//!
//! The committee that can take an operator's stake is elected by operators,
//! and a dispute filed against a node has a deadline attached to it. Both are
//! already permissionless on chain and neither was reachable from the software
//! an operator actually runs: participating meant hand-writing
//! `stellar contract invoke` against a contract whose arguments include a
//! 32-byte key and an election id nothing printed. A franchise nobody can
//! exercise is not a franchise, and an appeal window nobody is told about is a
//! penalty by default.

use async_trait::async_trait;
use serde::Serialize;

use super::stellar::{as_i128, as_u32, as_u64, as_variant, StellarCli};
use crate::config::NetworkConfig;
use crate::error::{NodeError, Result};

/// Where a dispute has got to. Mirrors the contract's `DisputeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisputeStatus {
    /// The committee is voting.
    Voting,
    /// Found against the node. Stake moves when the appeal window closes.
    Upheld,
    /// Found for the node. The reporter's bond moves to the operator when the
    /// appeal window closes.
    Dismissed,
    /// Money has moved; closed for good.
    Settled,
}

impl DisputeStatus {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "Voting" => Some(Self::Voting),
            "Upheld" => Some(Self::Upheld),
            "Dismissed" => Some(Self::Dismissed),
            "Settled" => Some(Self::Settled),
            _ => None,
        }
    }
}

/// What an election is doing right now. Mirrors the contract's
/// `ElectionPhase`, and is computed the same way — see [`ElectionRecord::phase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ElectionPhase {
    Nominating,
    Balloting,
    /// The ballot has closed and nobody has counted it yet.
    Counting,
    Seated,
    Failed,
}

/// The slashing contract's parameters, as the deployment currently holds them.
#[derive(Debug, Clone, Serialize)]
pub struct SlashingParams {
    pub quorum: u32,
    pub voting_period: u64,
    pub appeal_period: u64,
    pub dispute_bond: i128,
    pub appeal_bond: i128,
    pub seats: u32,
    pub nomination_period: u64,
    pub election_period: u64,
    pub term_length: u64,
}

/// An allegation on the ledger.
#[derive(Debug, Clone, Serialize)]
pub struct DisputeRecord {
    pub id: u64,
    /// The accused node, by signing key.
    pub accused: String,
    pub reporter: String,
    pub feed: String,
    /// The nonce the accused signed the disputed submission under. The same
    /// number `replay <feed> <nonce>` takes, which is the point of it: an
    /// allegation names something the signature covers.
    pub nonce: u64,
    pub evidence: String,
    pub bond: i128,
    pub opened_at: u64,
    /// Voting closes here.
    pub deadline: u64,
    /// When the current phase resolved. Zero while voting.
    pub resolved_at: u64,
    /// Bumped by an appeal, so first-round votes are not counted twice.
    pub vote_round: u32,
    pub votes_for: u32,
    pub votes_against: u32,
    pub status: DisputeStatus,
    pub appellant: Option<String>,
    pub appeal_bond: i128,
}

impl DisputeRecord {
    /// When money moves, if nobody appeals. Meaningless while voting, which is
    /// why it is an `Option` rather than a number that happens to be in the
    /// past.
    pub fn settles_at(&self, appeal_period: u64) -> Option<u64> {
        (self.resolved_at > 0).then_some(self.resolved_at + appeal_period)
    }
}

/// An answer the accused put on the record.
///
/// The digest, not the document. What it establishes is narrow: that a file
/// with these bytes existed at `at`, which was while the vote was open and
/// before the accused could know how it was going. Whether the document behind
/// it is worth anything is [`crate::engine::verify`]'s question, and this record
/// cannot help with it.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseRecord {
    pub dispute: u64,
    pub vote_round: u32,
    /// The account that filed it: the accused node's owner.
    pub by: String,
    /// SHA-256 of the document, in hex — what `verify-evidence --digest` takes.
    pub digest: String,
    /// Where the accused said it can be found. Often empty, which is allowed:
    /// a file handed over privately is still a file that was fixed.
    pub uri: String,
    pub at: u64,
}

/// Somebody standing for a seat, and the weight cast for them so far.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateRecord {
    pub address: String,
    /// The node the candidacy rests on.
    pub node: String,
    pub weight: u64,
}

/// An election on the ledger.
#[derive(Debug, Clone, Serialize)]
pub struct ElectionRecord {
    pub id: u64,
    pub opened_at: u64,
    /// Nominations close and the ballot opens here.
    pub ballot_opens: u64,
    /// The ballot closes here and finalisation becomes possible.
    pub closes: u64,
    /// Seats and quorum as they stood when it was opened — a `set_config`
    /// landing mid-election does not move the bar under a ballot already
    /// being cast against it.
    pub seats: u32,
    pub quorum: u32,
    /// `Running`, `Seated` or `Failed`, as stored.
    pub status: String,
    pub finalized_at: u64,
    pub ballots: u32,
    pub turnout: u64,
    pub seated: u32,
}

impl ElectionRecord {
    /// The stored status combined with the clock.
    ///
    /// Computed here rather than read back from `election_phase` for two
    /// reasons: it saves a round trip on a value entirely derivable from the
    /// record already in hand, and it lets everything downstream of it be a
    /// pure function of a record and a timestamp, which is what makes the duty
    /// derivation testable without a chain. It mirrors the contract's
    /// `phase_of` exactly; `mirrors_the_contracts_phase_rule` is the test that
    /// says so.
    pub fn phase(&self, now: u64) -> ElectionPhase {
        match self.status.as_str() {
            "Seated" => ElectionPhase::Seated,
            "Failed" => ElectionPhase::Failed,
            _ if now < self.ballot_opens => ElectionPhase::Nominating,
            _ if now < self.closes => ElectionPhase::Balloting,
            _ => ElectionPhase::Counting,
        }
    }
}

/// Everything the node does against the slashing contract.
///
/// Every write is authorised by one account — the one that bonded the stake —
/// so the address arguments the contract takes (`candidate`, `voter`,
/// `member`, `reporter`, `appellant`) are not parameters here: passing an
/// address the client cannot sign for would only produce a `require_auth`
/// failure a transaction fee later.
#[async_trait]
pub trait CommitteeClient: Send + Sync {
    /// The account these calls are signed by, for error messages that would
    /// otherwise say only "not eligible".
    fn account(&self) -> &str;

    async fn params(&self) -> Result<SlashingParams>;

    /// The addresses entitled to vote on disputes.
    async fn committee(&self) -> Result<Vec<String>>;

    /// The account that bonded a node's stake, which is the account entitled
    /// to stand, vote and appeal on its behalf.
    async fn owner_of(&self, public_key_hex: &str) -> Result<Option<String>>;

    /// The election that has not been finalised yet, if there is one.
    async fn current_election(&self) -> Result<Option<u64>>;

    /// Ledger time from which another election may be opened.
    async fn next_election(&self) -> Result<u64>;

    async fn election(&self, id: u64) -> Result<Option<ElectionRecord>>;
    async fn candidates(&self, id: u64) -> Result<Vec<CandidateRecord>>;

    /// Who this node voted for in an election, if it has voted.
    async fn ballot_of(&self, id: u64, public_key_hex: &str) -> Result<Option<String>>;

    async fn dispute_count(&self) -> Result<u64>;
    async fn dispute(&self, id: u64) -> Result<Option<DisputeRecord>>;

    /// How a committee member voted in a dispute's current voting round.
    async fn vote_of(&self, dispute_id: u64, member: &str) -> Result<Option<bool>>;

    /// What the accused answered a voting round with, oldest first. Empty for a
    /// round nobody answered, which is itself worth reading.
    async fn responses(&self, dispute_id: u64, vote_round: u32) -> Result<Vec<ResponseRecord>>;

    // -- writes -------------------------------------------------------------

    async fn open_election(&self) -> Result<Receipt<u64>>;
    async fn nominate(&self, public_key_hex: &str) -> Result<Receipt<()>>;
    async fn cast_ballot(&self, public_key_hex: &str, candidate: &str) -> Result<Receipt<()>>;
    async fn finalize_election(&self) -> Result<Receipt<String>>;

    async fn open_dispute(
        &self,
        accused: &str,
        feed: &str,
        nonce: u64,
        evidence: &str,
    ) -> Result<Receipt<u64>>;
    /// Put the digest of an answer on the record. Only the accused's owner may,
    /// which is the account this client signs as.
    async fn respond(&self, dispute_id: u64, digest_hex: &str, uri: &str) -> Result<Receipt<()>>;
    async fn vote(&self, dispute_id: u64, uphold: bool) -> Result<Receipt<()>>;
    async fn resolve(&self, dispute_id: u64) -> Result<Receipt<DisputeStatus>>;
    async fn appeal(&self, dispute_id: u64) -> Result<Receipt<()>>;
    async fn settle(&self, dispute_id: u64) -> Result<Receipt<()>>;
}

/// What a write returned, and where it landed.
#[derive(Debug, Clone, Serialize)]
pub struct Receipt<T> {
    pub value: T,
    /// `None` when the hash could not be recovered from the CLI's output. The
    /// call still landed — see [`super::stellar::extract_tx_hash`].
    pub tx_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// decoding
// ---------------------------------------------------------------------------

fn field<'a>(v: &'a serde_json::Value, name: &str, ctx: &str) -> Result<&'a serde_json::Value> {
    v.get(name)
        .ok_or_else(|| NodeError::Chain(format!("{ctx} has no `{name}`: {v}")))
}

fn u64_field(v: &serde_json::Value, name: &str, ctx: &str) -> Result<u64> {
    as_u64(field(v, name, ctx)?)
        .ok_or_else(|| NodeError::Chain(format!("{ctx}.{name} is not a u64: {v}")))
}

fn u32_field(v: &serde_json::Value, name: &str, ctx: &str) -> Result<u32> {
    as_u32(field(v, name, ctx)?)
        .ok_or_else(|| NodeError::Chain(format!("{ctx}.{name} is not a u32: {v}")))
}

fn i128_field(v: &serde_json::Value, name: &str, ctx: &str) -> Result<i128> {
    as_i128(field(v, name, ctx)?)
        .ok_or_else(|| NodeError::Chain(format!("{ctx}.{name} is not an i128: {v}")))
}

fn str_field(v: &serde_json::Value, name: &str, ctx: &str) -> Result<String> {
    field(v, name, ctx)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| NodeError::Chain(format!("{ctx}.{name} is not a string: {v}")))
}

/// Normalise a 32-byte key however the CLI rendered it.
fn hex_key(v: &serde_json::Value) -> Option<String> {
    let s = v.as_str()?.trim_start_matches("0x").to_ascii_lowercase();
    (s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

pub(crate) fn decode_params(v: &serde_json::Value) -> Result<SlashingParams> {
    let ctx = "slashing.get_config";
    Ok(SlashingParams {
        quorum: u32_field(v, "quorum", ctx)?,
        voting_period: u64_field(v, "voting_period", ctx)?,
        appeal_period: u64_field(v, "appeal_period", ctx)?,
        dispute_bond: i128_field(v, "dispute_bond", ctx)?,
        appeal_bond: i128_field(v, "appeal_bond", ctx)?,
        seats: u32_field(v, "seats", ctx)?,
        nomination_period: u64_field(v, "nomination_period", ctx)?,
        election_period: u64_field(v, "election_period", ctx)?,
        term_length: u64_field(v, "term_length", ctx)?,
    })
}

pub(crate) fn decode_dispute(v: &serde_json::Value) -> Result<DisputeRecord> {
    let ctx = "slashing.get_dispute";
    let status_name = as_variant(field(v, "status", ctx)?)
        .ok_or_else(|| NodeError::Chain(format!("{ctx}.status is not an enum: {v}")))?;
    let status = DisputeStatus::parse(&status_name).ok_or_else(|| {
        // A status this build does not know about must not be guessed at: the
        // duty derived from it decides whether an operator is told to appeal.
        NodeError::Chain(format!(
            "{ctx} returned an unknown status `{status_name}`; this node is \
             older than the slashing contract it is reading"
        ))
    })?;

    Ok(DisputeRecord {
        id: u64_field(v, "id", ctx)?,
        accused: hex_key(field(v, "accused", ctx)?)
            .ok_or_else(|| NodeError::Chain(format!("{ctx}.accused is not a 32-byte key: {v}")))?,
        reporter: str_field(v, "reporter", ctx)?,
        feed: str_field(v, "feed", ctx)?,
        nonce: u64_field(v, "nonce", ctx)?,
        evidence: str_field(v, "evidence", ctx)?,
        bond: i128_field(v, "bond", ctx)?,
        opened_at: u64_field(v, "opened_at", ctx)?,
        deadline: u64_field(v, "deadline", ctx)?,
        resolved_at: u64_field(v, "resolved_at", ctx)?,
        vote_round: u32_field(v, "vote_round", ctx)?,
        votes_for: u32_field(v, "votes_for", ctx)?,
        votes_against: u32_field(v, "votes_against", ctx)?,
        status,
        // `Option<Address>` renders as the address or as null.
        appellant: v
            .get("appellant")
            .and_then(|a| a.as_str())
            .map(str::to_string),
        appeal_bond: i128_field(v, "appeal_bond", ctx)?,
    })
}

pub(crate) fn decode_responses(v: &serde_json::Value) -> Result<Vec<ResponseRecord>> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let items = v.as_array().ok_or_else(|| {
        NodeError::Chain(format!("slashing.responses returned {v}, expected a list"))
    })?;
    let ctx = "slashing.responses";
    items
        .iter()
        .map(|r| {
            Ok(ResponseRecord {
                dispute: u64_field(r, "dispute", ctx)?,
                vote_round: u32_field(r, "vote_round", ctx)?,
                by: str_field(r, "by", ctx)?,
                digest: hex_key(field(r, "digest", ctx)?).ok_or_else(|| {
                    NodeError::Chain(format!("{ctx}.digest is not a 32-byte hash: {r}"))
                })?,
                uri: str_field(r, "uri", ctx)?,
                at: u64_field(r, "at", ctx)?,
            })
        })
        .collect()
}

pub(crate) fn decode_election(v: &serde_json::Value) -> Result<ElectionRecord> {
    let ctx = "slashing.get_election";
    Ok(ElectionRecord {
        id: u64_field(v, "id", ctx)?,
        opened_at: u64_field(v, "opened_at", ctx)?,
        ballot_opens: u64_field(v, "ballot_opens", ctx)?,
        closes: u64_field(v, "closes", ctx)?,
        seats: u32_field(v, "seats", ctx)?,
        quorum: u32_field(v, "quorum", ctx)?,
        status: as_variant(field(v, "status", ctx)?)
            .ok_or_else(|| NodeError::Chain(format!("{ctx}.status is not an enum: {v}")))?,
        finalized_at: u64_field(v, "finalized_at", ctx)?,
        ballots: u32_field(v, "ballots", ctx)?,
        turnout: u64_field(v, "turnout", ctx)?,
        seated: u32_field(v, "seated", ctx)?,
    })
}

pub(crate) fn decode_candidates(v: &serde_json::Value) -> Result<Vec<CandidateRecord>> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let items = v.as_array().ok_or_else(|| {
        NodeError::Chain(format!("slashing.candidates returned {v}, expected a list"))
    })?;
    let ctx = "slashing.candidates";
    items
        .iter()
        .map(|c| {
            Ok(CandidateRecord {
                address: str_field(c, "address", ctx)?,
                node: hex_key(field(c, "node", ctx)?).ok_or_else(|| {
                    NodeError::Chain(format!("{ctx}.node is not a 32-byte key: {c}"))
                })?,
                weight: u64_field(c, "weight", ctx)?,
            })
        })
        .collect()
}

fn decode_addresses(v: &serde_json::Value, ctx: &str) -> Result<Vec<String>> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let items = v
        .as_array()
        .ok_or_else(|| NodeError::Chain(format!("{ctx} returned {v}, expected a list")))?;
    Ok(items
        .iter()
        .filter_map(|a| a.as_str())
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------------------
// the live client
// ---------------------------------------------------------------------------

/// [`CommitteeClient`] driving the `stellar` CLI.
pub struct CliCommittee {
    cli: StellarCli,
    slashing: String,
    registry: String,
}

impl CliCommittee {
    /// Fails with a pointer rather than a contract error when the deployment's
    /// slashing contract was never configured: a node that has not been told
    /// where the committee lives cannot be shown its own disputes, and saying
    /// so at construction is better than a CLI invocation against an empty id.
    pub fn new(network: &NetworkConfig) -> Result<Self> {
        let slashing = network.slashing_contract.clone().ok_or_else(|| {
            NodeError::Config(
                "no `slashing_contract` in the [network] section. Committee and \
                 dispute commands need it; copy the `slashing` id out of the \
                 deployment record written by scripts/deploy.sh."
                    .into(),
            )
        })?;
        let cli = StellarCli::new(
            &network.rpc_url,
            &network.network_passphrase,
            network.operator_account(),
            network.operator_secret_env(),
        )?;
        Ok(Self {
            cli,
            slashing,
            registry: network.registry_contract.clone(),
        })
    }
}

#[async_trait]
impl CommitteeClient for CliCommittee {
    fn account(&self) -> &str {
        self.cli.account()
    }

    async fn params(&self) -> Result<SlashingParams> {
        decode_params(&self.cli.view(&self.slashing, "get_config", &[]).await?)
    }

    async fn committee(&self) -> Result<Vec<String>> {
        let v = self.cli.view(&self.slashing, "committee", &[]).await?;
        decode_addresses(&v, "slashing.committee")
    }

    async fn owner_of(&self, public_key_hex: &str) -> Result<Option<String>> {
        let v = self
            .cli
            .view(
                &self.registry,
                "owner_of",
                &[("pubkey", public_key_hex.to_string())],
            )
            .await?;
        Ok(v.as_str().map(str::to_string))
    }

    async fn current_election(&self) -> Result<Option<u64>> {
        let v = self
            .cli
            .view(&self.slashing, "current_election", &[])
            .await?;
        Ok(as_u64(&v))
    }

    async fn next_election(&self) -> Result<u64> {
        let v = self.cli.view(&self.slashing, "next_election", &[]).await?;
        Ok(as_u64(&v).unwrap_or(0))
    }

    async fn election(&self, id: u64) -> Result<Option<ElectionRecord>> {
        let v = self
            .cli
            .view(&self.slashing, "get_election", &[("id", id.to_string())])
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        decode_election(&v).map(Some)
    }

    async fn candidates(&self, id: u64) -> Result<Vec<CandidateRecord>> {
        let v = self
            .cli
            .view(&self.slashing, "candidates", &[("id", id.to_string())])
            .await?;
        decode_candidates(&v)
    }

    async fn ballot_of(&self, id: u64, public_key_hex: &str) -> Result<Option<String>> {
        let v = self
            .cli
            .view(
                &self.slashing,
                "ballot_of",
                &[("id", id.to_string()), ("node", public_key_hex.to_string())],
            )
            .await?;
        Ok(v.as_str().map(str::to_string))
    }

    async fn dispute_count(&self) -> Result<u64> {
        let v = self.cli.view(&self.slashing, "dispute_count", &[]).await?;
        Ok(as_u64(&v).unwrap_or(0))
    }

    async fn dispute(&self, id: u64) -> Result<Option<DisputeRecord>> {
        let v = self
            .cli
            .view(
                &self.slashing,
                "get_dispute",
                &[("dispute_id", id.to_string())],
            )
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        decode_dispute(&v).map(Some)
    }

    async fn vote_of(&self, dispute_id: u64, member: &str) -> Result<Option<bool>> {
        let v = self
            .cli
            .view(
                &self.slashing,
                "vote_of",
                &[
                    ("dispute_id", dispute_id.to_string()),
                    ("member", member.to_string()),
                ],
            )
            .await?;
        Ok(v.as_bool())
    }

    async fn responses(&self, dispute_id: u64, vote_round: u32) -> Result<Vec<ResponseRecord>> {
        let v = self
            .cli
            .view(
                &self.slashing,
                "responses",
                &[
                    ("dispute_id", dispute_id.to_string()),
                    ("vote_round", vote_round.to_string()),
                ],
            )
            .await?;
        decode_responses(&v)
    }

    async fn open_election(&self) -> Result<Receipt<u64>> {
        let (v, tx_hash) = self
            .cli
            .invoke(&self.slashing, "open_election", &[])
            .await?;
        Ok(Receipt {
            value: as_u64(&v).unwrap_or(0),
            tx_hash,
        })
    }

    async fn nominate(&self, public_key_hex: &str) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "nominate",
                &[
                    ("candidate", self.account().to_string()),
                    ("node", public_key_hex.to_string()),
                ],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }

    async fn cast_ballot(&self, public_key_hex: &str, candidate: &str) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "cast_ballot",
                &[
                    ("voter", self.account().to_string()),
                    ("node", public_key_hex.to_string()),
                    ("candidate", candidate.to_string()),
                ],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }

    async fn finalize_election(&self) -> Result<Receipt<String>> {
        let (v, tx_hash) = self
            .cli
            .invoke(&self.slashing, "finalize_election", &[])
            .await?;
        Ok(Receipt {
            value: as_variant(&v).unwrap_or_else(|| "unknown".into()),
            tx_hash,
        })
    }

    async fn open_dispute(
        &self,
        accused: &str,
        feed: &str,
        nonce: u64,
        evidence: &str,
    ) -> Result<Receipt<u64>> {
        let (v, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "open_dispute",
                &[
                    ("reporter", self.account().to_string()),
                    ("accused", accused.to_string()),
                    ("feed", feed.to_string()),
                    ("nonce", nonce.to_string()),
                    ("evidence", evidence.to_string()),
                ],
            )
            .await?;
        Ok(Receipt {
            value: as_u64(&v).unwrap_or(0),
            tx_hash,
        })
    }

    async fn respond(&self, dispute_id: u64, digest_hex: &str, uri: &str) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "respond",
                &[
                    ("responder", self.account().to_string()),
                    ("dispute_id", dispute_id.to_string()),
                    ("digest", digest_hex.to_string()),
                    ("uri", uri.to_string()),
                ],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }

    async fn vote(&self, dispute_id: u64, uphold: bool) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "vote",
                &[
                    ("member", self.account().to_string()),
                    ("dispute_id", dispute_id.to_string()),
                    ("uphold", uphold.to_string()),
                ],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }

    async fn resolve(&self, dispute_id: u64) -> Result<Receipt<DisputeStatus>> {
        let (v, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "resolve",
                &[("dispute_id", dispute_id.to_string())],
            )
            .await?;
        let name = as_variant(&v)
            .ok_or_else(|| NodeError::Chain(format!("slashing.resolve returned {v}")))?;
        let value = DisputeStatus::parse(&name).ok_or_else(|| {
            NodeError::Chain(format!("slashing.resolve returned unknown status `{name}`"))
        })?;
        Ok(Receipt { value, tx_hash })
    }

    async fn appeal(&self, dispute_id: u64) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "appeal",
                &[
                    ("appellant", self.account().to_string()),
                    ("dispute_id", dispute_id.to_string()),
                ],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }

    async fn settle(&self, dispute_id: u64) -> Result<Receipt<()>> {
        let (_, tx_hash) = self
            .cli
            .invoke(
                &self.slashing,
                "settle",
                &[("dispute_id", dispute_id.to_string())],
            )
            .await?;
        Ok(Receipt { value: (), tx_hash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_dispute_as_the_cli_renders_one() {
        let v = serde_json::json!({
            "id": 3,
            "accused": "AB".repeat(32),
            "reporter": "GREPORTER",
            "feed": "BTC_USD",
            "nonce": 91,
            "evidence": "ipfs://bafy",
            "bond": "1000000000",
            "opened_at": 1_700_000_000u64,
            "deadline": 1_700_086_400u64,
            "resolved_at": 0,
            "vote_round": 1,
            "votes_for": 2,
            "votes_against": 1,
            "status": "Voting",
            "appellant": null,
            "appeal_bond": 0,
        });
        let d = decode_dispute(&v).unwrap();
        assert_eq!(d.id, 3);
        // Normalised to lower case: the same key has to compare equal to the
        // one the node prints for itself.
        assert_eq!(d.accused, "ab".repeat(32));
        // The allegation's identity, and the argument to `replay`.
        assert_eq!(d.nonce, 91);
        assert_eq!(d.status, DisputeStatus::Voting);
        assert_eq!(d.bond, 1_000_000_000);
        assert!(d.appellant.is_none());
        // Nothing settles while voting is open.
        assert_eq!(d.settles_at(86_400), None);
    }

    #[test]
    fn a_resolved_dispute_knows_when_its_money_moves() {
        let mut v = serde_json::json!({
            "id": 1, "accused": "cd".repeat(32), "reporter": "G", "feed": "ETH_USD",
            "nonce": 1, "evidence": "", "bond": 1, "opened_at": 10, "deadline": 20,
            "resolved_at": 25, "vote_round": 1, "votes_for": 3, "votes_against": 0,
            "status": { "Upheld": [] }, "appellant": "GAPPEAL", "appeal_bond": 5,
        });
        let d = decode_dispute(&v).unwrap();
        assert_eq!(d.status, DisputeStatus::Upheld);
        assert_eq!(d.appellant.as_deref(), Some("GAPPEAL"));
        assert_eq!(d.settles_at(100), Some(125));

        // A status from a newer contract is refused rather than guessed: the
        // duty derived from it is whether to tell an operator to appeal.
        v["status"] = serde_json::json!("Reheard");
        assert!(decode_dispute(&v).is_err());
    }

    #[test]
    fn a_dispute_missing_a_field_names_the_field() {
        // A truncated read should say which field was not there. "expected a
        // struct" sends an operator to the wrong contract.
        let e = decode_dispute(&serde_json::json!({ "id": 1 }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("status"), "unhelpful error: {e}");

        let e = decode_dispute(&serde_json::json!({ "id": 1, "status": "Voting" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("accused"), "unhelpful error: {e}");
    }

    #[test]
    fn decodes_the_answers_on_a_disputes_record() {
        let v = serde_json::json!([
            {
                "dispute": 3, "vote_round": 1, "by": "GOWNER",
                "digest": "0x".to_string() + &"cd".repeat(32),
                "uri": "ipfs://bafyanswer", "at": 1_700_000_100u64,
            },
            {
                "dispute": 3, "vote_round": 1, "by": "GOWNER",
                "digest": "ef".repeat(32), "uri": "", "at": 1_700_000_200u64,
            },
        ]);
        let r = decode_responses(&v).unwrap();
        // Oldest first, and both of them: a correction is on the record beside
        // what it corrected, and a reader that showed only the last one would
        // be hiding the substitution.
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].digest, "cd".repeat(32));
        assert_eq!(r[1].digest, "ef".repeat(32));
        assert!(r[1].uri.is_empty());

        // A round nobody answered. Not an error: silence is a normal state of
        // a dispute and a committee is entitled to weigh it.
        assert!(decode_responses(&serde_json::json!(null))
            .unwrap()
            .is_empty());

        // Anything that is not a 32-byte hash is refused rather than passed on
        // to be compared against a file, where it could only ever mismatch.
        let bad = serde_json::json!([
            { "dispute": 1, "vote_round": 1, "by": "G", "digest": "ff", "uri": "", "at": 1 },
        ]);
        assert!(decode_responses(&bad).is_err());
    }

    fn election(status: &str, ballot_opens: u64, closes: u64) -> ElectionRecord {
        ElectionRecord {
            id: 1,
            opened_at: 0,
            ballot_opens,
            closes,
            seats: 5,
            quorum: 3,
            status: status.into(),
            finalized_at: 0,
            ballots: 0,
            turnout: 0,
            seated: 0,
        }
    }

    #[test]
    fn mirrors_the_contracts_phase_rule() {
        let e = election("Running", 100, 200);
        assert_eq!(e.phase(99), ElectionPhase::Nominating);
        // The boundaries are the contract's: `now < ballot_opens` is
        // nominating, `now < closes` is balloting, and the rest is counting.
        assert_eq!(e.phase(100), ElectionPhase::Balloting);
        assert_eq!(e.phase(199), ElectionPhase::Balloting);
        assert_eq!(e.phase(200), ElectionPhase::Counting);

        // A finalised election is what its status says, whatever the clock is
        // doing.
        assert_eq!(election("Seated", 100, 200).phase(0), ElectionPhase::Seated);
        assert_eq!(
            election("Failed", 100, 200).phase(9_999),
            ElectionPhase::Failed
        );
    }

    #[test]
    fn decodes_candidates_and_an_empty_field() {
        let v = serde_json::json!([
            { "address": "GONE", "node": "0x".to_string() + &"11".repeat(32), "weight": "7500" },
        ]);
        let c = decode_candidates(&v).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].node, "11".repeat(32));
        assert_eq!(c[0].weight, 7500);

        // A nomination period nobody stood in, which is the normal state of an
        // election in its first hour.
        assert!(decode_candidates(&serde_json::json!(null))
            .unwrap()
            .is_empty());
        assert!(decode_candidates(&serde_json::json!([]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_empty_committee_is_a_list_not_an_error() {
        assert!(decode_addresses(&serde_json::json!(null), "x")
            .unwrap()
            .is_empty());
        assert!(decode_addresses(&serde_json::json!(42), "x").is_err());
    }
}
