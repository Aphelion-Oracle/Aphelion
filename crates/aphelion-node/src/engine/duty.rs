//! What the slashing contract is asking of this operator, and by when.
//!
//! # The gap this closes
//!
//! Two of the network's mechanisms run on a clock the node had no view of.
//! A dispute filed against a node opens a voting period and then an appeal
//! window, and an operator who does not know it exists loses stake to a
//! committee they never answered. An election opens a nomination period and
//! then a ballot, and an operator who does not know it is running is
//! disenfranchised by not being told.
//!
//! Both were already permissionless on chain, and neither was reachable from
//! the software an operator runs: taking part meant hand-writing
//! `stellar contract invoke` against a contract whose arguments include a
//! 32-byte key and an election id that nothing printed. A franchise nobody can
//! exercise is not a franchise, and an appeal window nobody is told about is a
//! penalty by default.
//!
//! # Why the derivation is a pure function
//!
//! [`derive`] takes a [`Snapshot`] and a clock and returns a list. It reads
//! nothing and writes nothing, because the interesting part is the judgement —
//! is this operator entitled to vote, has the window closed, does the money
//! move towards them or away — and a judgement that needs a chain to exercise
//! is a judgement that gets tested once. [`Watch`] is the thin part that
//! fetches a snapshot; every rule below is tested against a struct literal.
//!
//! # What it deliberately does not do
//!
//! It never votes. Every duty here is reported, and acting on one is a
//! separate command an operator runs on purpose. A node that cast committee
//! votes on a schedule would be a committee seat held by a cron job, which is
//! the failure mode the elected committee exists to avoid — and a node that
//! auto-appealed would spend the appeal bond on every dispute it ever lost.
//! The one thing automation is good for here is noticing, and that is what
//! this is.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::chain::committee::{
    CommitteeClient, DisputeRecord, DisputeStatus, ElectionPhase, ElectionRecord, SlashingParams,
};
use crate::chain::ChainClient;
use crate::error::Result;

/// How many disputes back from the newest a scan reads.
///
/// A bound rather than the whole ledger, because each one is an RPC round
/// trip and the interesting ones are recent: voting periods and appeal windows
/// are measured in days, and a dispute old enough to fall off the end of this
/// window has either settled or been abandoned by everyone involved. The scan
/// reports the range it covered so a miss is visible rather than silent.
pub const DEFAULT_SCAN_DEPTH: u64 = 50;

/// Where this operator stands: who they are to the registry, and what that
/// entitles them to.
#[derive(Debug, Clone, Serialize)]
pub struct Standing {
    /// This node's signing key, lower-case hex.
    pub node: String,
    /// The account that bonded it. `None` when the key is not registered,
    /// which is every duty's answer to "is this mine": nothing is.
    pub owner: Option<String>,
    /// The account this node's committee actions would be signed by. Equal to
    /// `owner` in a correctly configured deployment, and the thing to look at
    /// first when the contract answers `NotEligible`.
    pub signer: String,
    /// Whether `signer` holds a committee seat.
    pub on_committee: bool,
    /// The node's voting weight. Zero means unregistered, jailed or exiting,
    /// and carries neither a ballot nor a candidacy.
    pub weight_bps: u32,
}

impl Standing {
    /// Whether this node's key is the one being accused.
    fn accused_is_mine(&self, d: &DisputeRecord) -> bool {
        d.accused == self.node
    }

    /// Whether this operator filed the allegation.
    fn reported_by_me(&self, d: &DisputeRecord) -> bool {
        self.owner.as_deref() == Some(d.reporter.as_str()) || self.signer == d.reporter
    }

    /// Registered, not jailed, not exiting: entitled to a ballot and to stand.
    fn enfranchised(&self) -> bool {
        self.weight_bps > 0
    }
}

/// What happens to this operator if a duty goes undone.
///
/// Named rather than scored, because the four are not points on one scale and
/// an operator triaging a list needs to know which kind of thing they are
/// looking at. They order the list, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Consequence {
    /// Stake or a bond is lost, or a finding stands that could have been
    /// contested. A window closes and it cannot be reopened.
    Costly,
    /// A say this operator is entitled to, spent by not using it. Nothing is
    /// taken; something is decided without them.
    Forfeited,
    /// Money already owed to this operator sits where it is until somebody
    /// moves it. No deadline — being late costs nothing but the wait.
    Owed,
    /// Nothing, to this operator. The network needs it done and anybody may
    /// do it.
    Housekeeping,
}

impl Consequence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Costly => "costly",
            Self::Forfeited => "forfeited",
            Self::Owed => "owed",
            Self::Housekeeping => "housekeeping",
        }
    }
}

/// The kind of thing that needs doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DutyKind {
    /// A dispute against this node is being voted on. Nothing is filed on
    /// chain to answer it — the evidence link is off-chain and so is the
    /// answer — but the committee is deciding now, not later.
    AnswerDispute,
    /// A dispute against this node was upheld and the appeal window is open.
    Appeal,
    /// A committee seat, and a dispute this member has not voted on.
    VoteOnDispute,
    /// Voting has closed on a dispute this operator is party to and nobody has
    /// recorded the result.
    ResolveDispute,
    /// A resolved dispute whose appeal window has closed, where settling moves
    /// money towards this operator.
    Settle,
    /// An election is taking nominations and this operator could stand.
    Nominate,
    /// An election is balloting and this node has not voted.
    CastBallot,
    /// A ballot has closed and nobody has counted it.
    FinalizeElection,
    /// The committee's term is served and no election has been opened.
    OpenElection,
}

/// One thing to do, why, and by when.
#[derive(Debug, Clone, Serialize)]
pub struct Duty {
    pub kind: DutyKind,
    pub consequence: Consequence,
    /// The dispute or election this is about. Zero for [`DutyKind::OpenElection`],
    /// which is about the one that does not exist yet.
    pub subject: u64,
    /// Ledger time by which it must be done, where a window closes.
    pub deadline: Option<u64>,
    /// One line an operator can act on without reading the contract.
    pub detail: String,
    /// The command that does it.
    pub command: String,
}

impl Duty {
    /// Seconds left, negative once the window has closed.
    ///
    /// A signed number on purpose: a duty whose deadline has just passed is
    /// the one an operator most needs to see, and clamping it to zero would
    /// make "closed four seconds ago" and "closed four days ago" the same
    /// line.
    pub fn seconds_left(&self, now: u64) -> Option<i64> {
        self.deadline
            .map(|d| d as i64 - i64::try_from(now).unwrap_or(i64::MAX))
    }
}

/// Everything [`derive`] reads, as one value.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Ledger time. Never the node's own clock: every window below is compared
    /// against this by the contract.
    pub now: u64,
    pub params: SlashingParams,
    pub standing: Standing,
    /// The disputes the scan covered, newest first.
    pub disputes: Vec<DisputeRecord>,
    /// Ids of disputes whose current voting round this member has already
    /// voted in. Empty for anyone not on the committee.
    pub voted: BTreeSet<u64>,
    /// The election that has not been finalised, if there is one.
    pub election: Option<ElectionRecord>,
    /// Whether this node has already cast a ballot in that election.
    pub balloted: bool,
    /// Whether this operator already stands in it.
    pub nominated: bool,
    /// Ledger time from which another election may be opened.
    pub next_election: u64,
    /// The dispute id range the scan covered, and the total that exist.
    pub scanned: (u64, u64),
    pub total_disputes: u64,
}

impl Snapshot {
    /// Whether the scan could have missed a dispute.
    pub fn scan_is_complete(&self) -> bool {
        self.scanned.0 <= 1
    }
}

/// The list, worst consequence first and soonest deadline first within that.
pub fn derive(s: &Snapshot) -> Vec<Duty> {
    let mut duties = Vec::new();
    for d in &s.disputes {
        duties.extend(dispute_duties(s, d));
    }
    duties.extend(election_duties(s));

    // A duty with a deadline outranks one without, inside the same
    // consequence: the one that expires is the one that cannot be done later.
    duties.sort_by_key(|d| (d.consequence, d.deadline.unwrap_or(u64::MAX), d.subject));
    duties
}

fn dispute_duties(s: &Snapshot, d: &DisputeRecord) -> Vec<Duty> {
    let mine = s.standing.accused_is_mine(d);
    let reported = s.standing.reported_by_me(d);
    let mut out = Vec::new();

    match d.status {
        DisputeStatus::Voting if s.now <= d.deadline => {
            if mine {
                out.push(Duty {
                    kind: DutyKind::AnswerDispute,
                    consequence: Consequence::Costly,
                    subject: d.id,
                    deadline: Some(d.deadline),
                    detail: format!(
                        "dispute {} against this node ({} nonce {}); {} for, {} against, \
                         quorum {}. Evidence: {}",
                        d.id,
                        d.feed,
                        d.nonce,
                        d.votes_for,
                        d.votes_against,
                        s.params.quorum,
                        if d.evidence.is_empty() {
                            "(none given)"
                        } else {
                            &d.evidence
                        }
                    ),
                    // `replay`, not `dispute show`. The allegation names a
                    // nonce, which is the argument that reproduces the round
                    // from the observations behind it, so the duty can point at
                    // the thing that answers it rather than at the thing that
                    // restates it.
                    command: format!("aphelion-node replay {} {}", d.feed, d.nonce),
                });
            }
            // The contract refuses a vote on a dispute against a node the
            // member owns, so offering it here would be offering a
            // transaction fee for a guaranteed refusal.
            if s.standing.on_committee && !mine && !s.voted.contains(&d.id) {
                out.push(Duty {
                    kind: DutyKind::VoteOnDispute,
                    consequence: Consequence::Forfeited,
                    subject: d.id,
                    deadline: Some(d.deadline),
                    detail: format!(
                        "committee vote outstanding on dispute {} ({} nonce {}); \
                         {} for, {} against, quorum {}",
                        d.id, d.feed, d.nonce, d.votes_for, d.votes_against, s.params.quorum
                    ),
                    command: format!("aphelion-node dispute vote {} --uphold|--dismiss", d.id),
                });
            }
        }

        // Voting closed and nobody recorded the result. Permissionless, and
        // reported only to the two parties: the result is a function of votes
        // already cast, so there is nothing here for a bystander to want.
        DisputeStatus::Voting if mine || reported => out.push(Duty {
            kind: DutyKind::ResolveDispute,
            consequence: Consequence::Housekeeping,
            subject: d.id,
            deadline: None,
            detail: format!(
                "voting on dispute {} closed at {}; the result is not on the ledger yet",
                d.id, d.deadline
            ),
            command: format!("aphelion-node dispute resolve {}", d.id),
        }),
        DisputeStatus::Voting => {}

        DisputeStatus::Upheld | DisputeStatus::Dismissed => {
            let window_closes = d.resolved_at + s.params.appeal_period;
            let lost = (mine && d.status == DisputeStatus::Upheld)
                || (reported && d.status == DisputeStatus::Dismissed);

            if lost && d.appellant.is_none() && s.now <= window_closes {
                out.push(Duty {
                    kind: DutyKind::Appeal,
                    consequence: Consequence::Costly,
                    subject: d.id,
                    deadline: Some(window_closes),
                    detail: format!(
                        "dispute {} was {} ({} for, {} against). An appeal costs {} and is \
                         returned only if the second vote changes the outcome",
                        d.id,
                        if d.status == DisputeStatus::Upheld {
                            "upheld"
                        } else {
                            "dismissed"
                        },
                        d.votes_for,
                        d.votes_against,
                        s.params.appeal_bond
                    ),
                    command: format!("aphelion-node dispute appeal {}", d.id),
                });
            }

            // Settlement is permissionless and the money only moves when
            // somebody calls it. Reported when it moves towards this
            // operator — the accused whose dismissal frees the reporter's
            // bond to them, or the reporter whose upheld claim earns the
            // reward — and not otherwise, because nobody needs a nudge to
            // pay a fee that pays somebody else.
            let owed = (mine && d.status == DisputeStatus::Dismissed)
                || (reported && d.status == DisputeStatus::Upheld);
            if owed && s.now > window_closes {
                out.push(Duty {
                    kind: DutyKind::Settle,
                    consequence: Consequence::Owed,
                    subject: d.id,
                    deadline: None,
                    detail: format!(
                        "dispute {} was {} and its appeal window closed at {}; \
                         settling releases what is owed",
                        d.id,
                        if d.status == DisputeStatus::Upheld {
                            "upheld"
                        } else {
                            "dismissed"
                        },
                        window_closes
                    ),
                    command: format!("aphelion-node dispute settle {}", d.id),
                });
            }
        }

        DisputeStatus::Settled => {}
    }
    out
}

fn election_duties(s: &Snapshot) -> Vec<Duty> {
    let Some(e) = &s.election else {
        // No election running. One may be opened once the sitting committee
        // has served its term, and nothing opens it automatically.
        if s.now >= s.next_election {
            return vec![Duty {
                kind: DutyKind::OpenElection,
                consequence: Consequence::Housekeeping,
                subject: 0,
                deadline: None,
                detail: format!(
                    "the committee's term was served at {}; no election has been opened",
                    s.next_election
                ),
                command: "aphelion-node election open".into(),
            }];
        }
        return Vec::new();
    };

    match e.phase(s.now) {
        // Standing is a choice, not an obligation, so it is reported as an
        // opportunity with no consequence attached. Only to an operator who
        // could actually take a seat: the contract checks weight at
        // nomination and again at the count.
        ElectionPhase::Nominating if s.standing.enfranchised() && !s.nominated => vec![Duty {
            kind: DutyKind::Nominate,
            consequence: Consequence::Housekeeping,
            subject: e.id,
            deadline: Some(e.ballot_opens),
            detail: format!(
                "election {} is taking nominations for {} seats until {}",
                e.id, e.seats, e.ballot_opens
            ),
            command: "aphelion-node election nominate".into(),
        }],
        ElectionPhase::Nominating => Vec::new(),

        ElectionPhase::Balloting if s.standing.enfranchised() && !s.balloted => vec![Duty {
            kind: DutyKind::CastBallot,
            consequence: Consequence::Forfeited,
            subject: e.id,
            deadline: Some(e.closes),
            detail: format!(
                "election {} is balloting for {} seats until {}; this node's {} bps \
                 has not been cast",
                e.id, e.seats, e.closes, s.standing.weight_bps
            ),
            command: "aphelion-node election ballot <candidate>".into(),
        }],
        ElectionPhase::Balloting => Vec::new(),

        ElectionPhase::Counting => vec![Duty {
            kind: DutyKind::FinalizeElection,
            consequence: Consequence::Housekeeping,
            subject: e.id,
            deadline: None,
            detail: format!(
                "election {} closed at {} with {} ballots and is not counted; \
                 until it is, no other election can open",
                e.id, e.closes, e.ballots
            ),
            command: "aphelion-node election finalize".into(),
        }],

        // `current_election` only returns an unfinalised one, so these are
        // unreachable through `Watch`. Handled rather than ignored because a
        // caller assembling a snapshot by hand can reach them.
        ElectionPhase::Seated | ElectionPhase::Failed => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// fetching one
// ---------------------------------------------------------------------------

/// Reads a [`Snapshot`] off the chain.
///
/// Two clients, because the two questions have different answers: ledger time
/// and this node's weight come from the aggregator and registry through
/// [`ChainClient`], while everything about the committee comes from the
/// slashing contract through [`CommitteeClient`] and is signed by a different
/// account. See the note on [`crate::chain::committee`].
pub struct Watch {
    chain: std::sync::Arc<dyn ChainClient>,
    committee: std::sync::Arc<dyn CommitteeClient>,
    node: String,
    scan_depth: u64,
}

impl Watch {
    pub fn new(
        chain: std::sync::Arc<dyn ChainClient>,
        committee: std::sync::Arc<dyn CommitteeClient>,
        node_public_key_hex: impl Into<String>,
    ) -> Self {
        Self {
            chain,
            committee,
            node: node_public_key_hex.into().to_ascii_lowercase(),
            scan_depth: DEFAULT_SCAN_DEPTH,
        }
    }

    pub fn with_scan_depth(mut self, depth: u64) -> Self {
        self.scan_depth = depth.max(1);
        self
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let now = self.chain.ledger_time().await?;
        let params = self.committee.params().await?;

        let owner = self.committee.owner_of(&self.node).await?;
        let signer = self.committee.account().to_string();
        let seats = self.committee.committee().await?;
        let weight_bps = self
            .chain
            .node_info(&self.node)
            .await?
            .map(|n| n.weight_bps)
            .unwrap_or(0);
        let standing = Standing {
            node: self.node.clone(),
            owner,
            on_committee: seats.contains(&signer),
            signer,
            weight_bps,
        };

        // Newest first, and bounded. Each read is a round trip, so the scan
        // covers the window where a deadline can still be open rather than the
        // whole history; `scanned` reports what it covered.
        let total = self.committee.dispute_count().await?;
        let first = total.saturating_sub(self.scan_depth) + 1;
        let mut disputes = Vec::new();
        let mut voted = BTreeSet::new();
        for id in (first..=total).rev() {
            let Some(d) = self.committee.dispute(id).await? else {
                continue;
            };
            if standing.on_committee
                && d.status == DisputeStatus::Voting
                && self
                    .committee
                    .vote_of(id, &standing.signer)
                    .await?
                    .is_some()
            {
                voted.insert(id);
            }
            disputes.push(d);
        }

        let election_id = self.committee.current_election().await?;
        let mut election = None;
        let mut balloted = false;
        let mut nominated = false;
        if let Some(id) = election_id {
            election = self.committee.election(id).await?;
            balloted = self.committee.ballot_of(id, &self.node).await?.is_some();
            nominated = self
                .committee
                .candidates(id)
                .await?
                .iter()
                .any(|c| c.address == standing.signer);
        }

        Ok(Snapshot {
            now,
            params,
            standing,
            disputes,
            voted,
            election,
            balloted,
            nominated,
            next_election: self.committee.next_election().await?,
            scanned: (if total == 0 { 0 } else { first }, total),
            total_disputes: total,
        })
    }

    /// A snapshot and the duties derived from it.
    pub async fn duties(&self) -> Result<(Snapshot, Vec<Duty>)> {
        let snapshot = self.snapshot().await?;
        let duties = derive(&snapshot);
        Ok((snapshot, duties))
    }

    /// Re-read on an interval, for as long as the node runs.
    ///
    /// What it produces is a log line and a gauge, and that is the whole
    /// design: the windows here close in days, and an operator who is not
    /// watching a terminal needs the alert to reach them the way every other
    /// alert does. `aphelion_duties_outstanding` is the series to alert on;
    /// `deploy/prometheus/alerts.yml` ships a rule for it.
    ///
    /// A failed pass is logged and the loop continues. The slashing contract
    /// being unreachable is not a reason to stop watching it, and it is
    /// certainly not a reason to stop publishing prices.
    pub async fn run(
        self: std::sync::Arc<Self>,
        interval: std::time::Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => match self.duties().await {
                    Ok((snapshot, duties)) => report(&snapshot, &duties),
                    Err(e) => tracing::warn!(error = %e, "could not read the slashing contract"),
                },
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

/// Publish one pass: a gauge per consequence, and a log line per duty that
/// costs something.
///
/// The gauge is per consequence rather than a single total because the two
/// need different alerts. A vote not yet cast is worth a message in the
/// morning; a dispute against this node with a closing window is worth waking
/// somebody up, and a rule that could not tell them apart would be tuned for
/// whichever is more common — which is the first one.
fn report(snapshot: &Snapshot, duties: &[Duty]) {
    for consequence in [
        Consequence::Costly,
        Consequence::Forfeited,
        Consequence::Owed,
        Consequence::Housekeeping,
    ] {
        let n = duties
            .iter()
            .filter(|d| d.consequence == consequence)
            .count();
        metrics::gauge!(
            "aphelion_duties_outstanding",
            "consequence" => consequence.as_str()
        )
        .set(n as f64);
    }

    // The soonest deadline among the duties that cost stake, as seconds from
    // now. Absent rather than zero when there are none: a rule that alerted on
    // "seconds remaining is low" would fire permanently on a quiet network if
    // the quiet value were zero.
    let soonest = duties
        .iter()
        .filter(|d| d.consequence == Consequence::Costly)
        .filter_map(|d| d.seconds_left(snapshot.now))
        .min();
    if let Some(seconds) = soonest {
        metrics::gauge!("aphelion_duty_deadline_seconds").set(seconds as f64);
    }

    for d in duties {
        match d.consequence {
            Consequence::Costly => tracing::warn!(
                kind = ?d.kind,
                subject = d.subject,
                seconds_left = d.seconds_left(snapshot.now),
                command = %d.command,
                "{}",
                d.detail
            ),
            Consequence::Forfeited => tracing::info!(
                kind = ?d.kind,
                subject = d.subject,
                seconds_left = d.seconds_left(snapshot.now),
                command = %d.command,
                "{}",
                d.detail
            ),
            // Housekeeping and money owed are not urgent by construction, and
            // a node that logged them every quarter of an hour would teach its
            // operator to filter the whole target out.
            _ => tracing::debug!(kind = ?d.kind, subject = d.subject, "{}", d.detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "11111111111111111111111111111111111111111111111111111111111111aa";
    const SOMEBODY_ELSE: &str = "22222222222222222222222222222222222222222222222222222222222222bb";

    fn params() -> SlashingParams {
        SlashingParams {
            quorum: 3,
            voting_period: 86_400,
            appeal_period: 43_200,
            dispute_bond: 1_000,
            appeal_bond: 3_000,
            seats: 5,
            nomination_period: 3_600,
            election_period: 7_200,
            term_length: 100_000,
        }
    }

    fn standing() -> Standing {
        Standing {
            node: ME.into(),
            owner: Some("GOWNER".into()),
            signer: "GOWNER".into(),
            on_committee: false,
            weight_bps: 7_500,
        }
    }

    fn snapshot(now: u64) -> Snapshot {
        Snapshot {
            now,
            params: params(),
            standing: standing(),
            disputes: Vec::new(),
            voted: BTreeSet::new(),
            election: None,
            balloted: false,
            nominated: false,
            // Far enough ahead that an idle snapshot produces no duties at
            // all, so every test below is about the thing it adds.
            next_election: now + 1_000_000,
            scanned: (0, 0),
            total_disputes: 0,
        }
    }

    fn dispute(id: u64, accused: &str, status: DisputeStatus) -> DisputeRecord {
        DisputeRecord {
            id,
            accused: accused.into(),
            reporter: "GREPORTER".into(),
            feed: "BTC_USD".into(),
            nonce: 42,
            evidence: "ipfs://bafy".into(),
            bond: 1_000,
            opened_at: 1_000,
            deadline: 2_000,
            resolved_at: if status == DisputeStatus::Voting {
                0
            } else {
                2_100
            },
            vote_round: 1,
            votes_for: 0,
            votes_against: 0,
            status,
            appellant: None,
            appeal_bond: 0,
        }
    }

    fn election(id: u64, status: &str, ballot_opens: u64, closes: u64) -> ElectionRecord {
        ElectionRecord {
            id,
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

    fn kinds(duties: &[Duty]) -> Vec<DutyKind> {
        duties.iter().map(|d| d.kind).collect()
    }

    #[test]
    fn a_quiet_network_asks_nothing() {
        assert!(derive(&snapshot(5_000)).is_empty());
    }

    #[test]
    fn a_dispute_against_this_node_is_reported_while_the_committee_votes() {
        let mut s = snapshot(1_500);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Voting)];
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::AnswerDispute]);
        assert_eq!(d[0].consequence, Consequence::Costly);
        assert_eq!(d[0].deadline, Some(2_000));
        assert_eq!(d[0].seconds_left(1_500), Some(500));
        // The evidence link is the whole point of reporting it: an operator
        // who cannot see what they are accused of cannot answer it.
        assert!(d[0].detail.contains("ipfs://bafy"), "{}", d[0].detail);
    }

    #[test]
    fn a_dispute_against_somebody_else_is_not_this_operators_business() {
        let mut s = snapshot(1_500);
        s.disputes = vec![dispute(1, SOMEBODY_ELSE, DisputeStatus::Voting)];
        // Not on the committee, not the reporter: nothing to do.
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_committee_member_is_told_which_votes_are_outstanding() {
        let mut s = snapshot(1_500);
        s.standing.on_committee = true;
        s.disputes = vec![
            dispute(1, SOMEBODY_ELSE, DisputeStatus::Voting),
            dispute(2, SOMEBODY_ELSE, DisputeStatus::Voting),
        ];
        s.voted.insert(2);
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::VoteOnDispute]);
        assert_eq!(d[0].subject, 1);
        assert_eq!(d[0].consequence, Consequence::Forfeited);
    }

    #[test]
    fn a_member_is_never_offered_a_vote_on_their_own_node() {
        let mut s = snapshot(1_500);
        s.standing.on_committee = true;
        s.disputes = vec![dispute(1, ME, DisputeStatus::Voting)];
        // The contract refuses it, so offering it would be offering a fee for
        // a guaranteed refusal. Answering it is still reported.
        assert_eq!(kinds(&derive(&s)), vec![DutyKind::AnswerDispute]);
    }

    #[test]
    fn a_vote_this_operator_cannot_cast_any_more_is_not_offered() {
        let mut s = snapshot(2_001);
        s.standing.on_committee = true;
        s.disputes = vec![dispute(1, SOMEBODY_ELSE, DisputeStatus::Voting)];
        // Past the deadline. The contract refuses a late vote, and a duty an
        // operator cannot discharge is noise.
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn the_deadline_boundary_is_the_contracts_boundary() {
        let mut s = snapshot(2_000);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Voting)];
        // `vote` refuses when `now > deadline`, so the deadline second itself
        // is still open.
        assert_eq!(kinds(&derive(&s)), vec![DutyKind::AnswerDispute]);
    }

    #[test]
    fn a_dispute_nobody_resolved_is_reported_to_the_parties() {
        let mut s = snapshot(2_001);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Voting)];
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::ResolveDispute]);
        assert_eq!(d[0].consequence, Consequence::Housekeeping);
        assert_eq!(d[0].deadline, None);
    }

    #[test]
    fn an_unresolved_dispute_between_strangers_is_left_to_them() {
        let mut s = snapshot(2_001);
        s.disputes = vec![dispute(1, SOMEBODY_ELSE, DisputeStatus::Voting)];
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn an_upheld_dispute_against_this_node_offers_an_appeal_and_prices_it() {
        let mut s = snapshot(2_200);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Upheld)];
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::Appeal]);
        // resolved_at 2_100 + appeal_period 43_200.
        assert_eq!(d[0].deadline, Some(45_300));
        assert!(d[0].detail.contains("3000"), "{}", d[0].detail);
    }

    #[test]
    fn the_appeal_window_closes_and_the_duty_goes_with_it() {
        let mut s = snapshot(45_301);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Upheld)];
        // Nothing left to do: the finding stands and settling it moves stake
        // away from this operator, which is not a duty anyone owes themselves.
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_dispute_already_appealed_is_not_offered_a_second_one() {
        let mut s = snapshot(2_200);
        let mut d = dispute(1, ME, DisputeStatus::Upheld);
        d.appellant = Some("GOWNER".into());
        s.disputes = vec![d];
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_reporter_whose_claim_was_dismissed_may_appeal_it() {
        let mut s = snapshot(2_200);
        s.standing.owner = Some("GREPORTER".into());
        s.standing.signer = "GREPORTER".into();
        s.disputes = vec![dispute(1, SOMEBODY_ELSE, DisputeStatus::Dismissed)];
        assert_eq!(kinds(&derive(&s)), vec![DutyKind::Appeal]);
    }

    #[test]
    fn settlement_is_reported_only_where_the_money_comes_this_way() {
        // Dismissed against this node: the reporter's bond is owed here.
        let mut mine = snapshot(46_000);
        mine.disputes = vec![dispute(1, ME, DisputeStatus::Dismissed)];
        let d = derive(&mine);
        assert_eq!(kinds(&d), vec![DutyKind::Settle]);
        assert_eq!(d[0].consequence, Consequence::Owed);

        // Upheld against this node: settling takes stake away. Nobody needs a
        // reminder to pay a fee to be slashed.
        let mut against = snapshot(46_000);
        against.disputes = vec![dispute(1, ME, DisputeStatus::Upheld)];
        assert!(derive(&against).is_empty());
    }

    #[test]
    fn nothing_settles_while_the_appeal_window_is_open() {
        let mut s = snapshot(45_300);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Dismissed)];
        // `settle` refuses at `now <= resolved_at + appeal_period`, so the
        // boundary second is still the window.
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_settled_dispute_is_finished_with() {
        let mut s = snapshot(90_000);
        s.disputes = vec![dispute(1, ME, DisputeStatus::Settled)];
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_ballot_this_node_has_not_cast_is_a_forfeited_say() {
        let mut s = snapshot(150);
        s.election = Some(election(4, "Running", 100, 200));
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::CastBallot]);
        assert_eq!(d[0].consequence, Consequence::Forfeited);
        assert_eq!(d[0].deadline, Some(200));
        assert_eq!(d[0].subject, 4);
    }

    #[test]
    fn a_ballot_already_cast_is_not_asked_for_twice() {
        let mut s = snapshot(150);
        s.election = Some(election(4, "Running", 100, 200));
        s.balloted = true;
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_node_carrying_no_weight_is_not_asked_to_vote_or_to_stand() {
        let mut s = snapshot(150);
        s.standing.weight_bps = 0;
        s.election = Some(election(4, "Running", 100, 200));
        // Unregistered, jailed or exiting. The contract refuses both, and
        // telling a jailed operator to exercise a franchise they do not have
        // is worse than saying nothing.
        assert!(derive(&s).is_empty());

        s.now = 50;
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn nominations_are_an_opportunity_rather_than_an_obligation() {
        let mut s = snapshot(50);
        s.election = Some(election(4, "Running", 100, 200));
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::Nominate]);
        // Standing for a seat is a choice, so nothing is lost by not doing it.
        assert_eq!(d[0].consequence, Consequence::Housekeeping);
        assert_eq!(d[0].deadline, Some(100));

        s.nominated = true;
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_ballot_nobody_counted_blocks_every_later_election() {
        let mut s = snapshot(250);
        s.election = Some(election(4, "Running", 100, 200));
        let d = derive(&s);
        assert_eq!(kinds(&d), vec![DutyKind::FinalizeElection]);
        assert!(d[0].detail.contains("no other election can open"));
    }

    #[test]
    fn a_served_term_with_no_election_is_reported_as_the_gap_it_is() {
        let mut s = snapshot(5_000);
        s.next_election = 4_999;
        assert_eq!(kinds(&derive(&s)), vec![DutyKind::OpenElection]);

        // Not yet served: opening one would be refused.
        s.next_election = 5_001;
        assert!(derive(&s).is_empty());
    }

    #[test]
    fn a_running_election_is_not_also_a_reason_to_open_one() {
        let mut s = snapshot(5_000);
        s.next_election = 0;
        s.election = Some(election(4, "Running", 4_000, 9_000));
        // `open_election` refuses while one is unfinalised, so the term being
        // served is not on its own a reason to offer it.
        assert_eq!(kinds(&derive(&s)), vec![DutyKind::CastBallot]);
    }

    #[test]
    fn the_list_puts_what_costs_stake_above_what_costs_a_say() {
        let mut s = snapshot(1_500);
        s.standing.on_committee = true;
        s.election = Some(election(4, "Running", 1_000, 9_000));
        s.disputes = vec![
            dispute(1, SOMEBODY_ELSE, DisputeStatus::Voting),
            dispute(2, ME, DisputeStatus::Voting),
        ];
        let d = derive(&s);
        assert_eq!(
            kinds(&d),
            vec![
                DutyKind::AnswerDispute, // costly
                DutyKind::VoteOnDispute, // forfeited, deadline 2_000
                DutyKind::CastBallot,    // forfeited, deadline 9_000
            ]
        );
    }

    #[test]
    fn a_scan_that_could_not_reach_the_first_dispute_says_so() {
        let mut s = snapshot(1_000);
        s.total_disputes = 400;
        s.scanned = (351, 400);
        assert!(!s.scan_is_complete());

        s.scanned = (1, 400);
        assert!(s.scan_is_complete());
    }
}
