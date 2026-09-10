use soroban_sdk::{contracttype, Address, BytesN, String, Symbol};

#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisputeStatus {
    /// Committee members are voting.
    Voting,
    /// The committee found against the node. Stake moves once the appeal
    /// window closes.
    Upheld,
    /// The committee found for the node. The reporter's bond moves to the
    /// operator once the appeal window closes.
    Dismissed,
    /// Settled: money has moved and the dispute is closed for good.
    Settled,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// Governance. May remove a committee member and change these
    /// parameters; may *not* seat one — see `Slashing::finalize_election`.
    pub admin: Address,
    pub registry: Address,
    pub token: Address,

    /// Votes required for a dispute to resolve at all. A dispute nobody looked
    /// at fails closed — silence is not evidence of guilt.
    pub quorum: u32,
    pub voting_period: u64,
    /// How long a resolved dispute waits before money moves, so that a
    /// surprised party has a window in which to appeal.
    pub appeal_period: u64,

    /// Posted by whoever files a dispute, and forfeited if it is dismissed.
    pub dispute_bond: i128,
    /// Posted by whoever appeals. Higher than `dispute_bond`: an appeal asks
    /// the whole committee to do its work twice.
    pub appeal_bond: i128,

    /// Reputation removed from a node when a dispute is upheld.
    pub rep_penalty: u32,
    /// Stake seized when a dispute is upheld.
    pub slash_amount: i128,
    /// Paid to the reporter out of the slash pool when a dispute is upheld.
    pub reporter_reward: i128,

    // -- elections ----------------------------------------------------------
    /// How many seats an election fills. Never below `quorum`: a committee
    /// elected at full strength that still could not reach quorum would
    /// dismiss every dispute filed against anybody.
    pub seats: u32,
    /// How long candidates may stand before the ballot opens.
    pub nomination_period: u64,
    /// How long the ballot stays open after nominations close.
    pub election_period: u64,
    /// How long a seated committee serves before another election may be
    /// opened. Elections are permissionless, so this is the only thing
    /// stopping a caller from running one continuously.
    pub term_length: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Dispute {
    pub id: u64,
    /// The node being accused, by signing key.
    pub accused: BytesN<32>,
    /// Who filed, and who gets the bond back if they are right.
    pub reporter: Address,
    pub feed: Symbol,
    /// The round the allegation is about. Together with `feed` and `accused`
    /// this is the identity of the allegation, so the same claim cannot be
    /// filed twice.
    pub round_id: u64,
    /// Where the evidence lives — a URL or content hash. Deliberately not the
    /// evidence itself: ledger space is the wrong place for a data dump, and a
    /// hash is enough to prove nobody edited it afterwards.
    pub evidence: String,

    pub bond: i128,
    pub opened_at: u64,
    /// Voting closes at this time.
    pub deadline: u64,
    /// Ledger time at which the current phase resolved. Zero while voting.
    pub resolved_at: u64,

    /// Incremented by an appeal, so votes from the first round cannot be
    /// counted twice in the second.
    pub vote_round: u32,
    pub votes_for: u32,
    pub votes_against: u32,

    pub status: DisputeStatus,
    /// Who appealed, if anyone. A dispute may be appealed once.
    pub appellant: Option<Address>,
    pub appeal_bond: i128,
    /// The outcome the appellant was contesting, so settlement can tell
    /// whether the appeal actually changed anything. `Voting` while no appeal
    /// has been made, which is never a resolved outcome.
    pub appealed_from: DisputeStatus,
}

/// What the ledger records about an election.
///
/// As with a governance proposal, the phases that are facts about the clock —
/// nominating, balloting, waiting to be finalised — are not stored. Nothing
/// writes them, so a status that claimed them would put a transition in the
/// record that nobody made. [`ElectionPhase`] is what an operator reads.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElectionStatus {
    Running,
    /// Finalised: the winners took their seats.
    Seated,
    /// Finalised, and too few eligible candidates drew any weight to fill
    /// `quorum`. The incumbent committee stayed where it was.
    Failed,
}

/// What an election is doing right now: the stored status combined with the
/// clock.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElectionPhase {
    /// Candidates may stand. No ballots yet.
    Nominating,
    /// Nominations are closed and node operators are casting ballots.
    Balloting,
    /// The ballot has closed and the result is not yet on the ledger. Anyone
    /// may call `finalize_election` from here.
    Counting,
    Seated,
    Failed,
}

/// Somebody standing for a seat, and the weight cast for them so far.
#[contracttype]
#[derive(Clone)]
pub struct Candidate {
    pub address: Address,
    /// The node this candidacy rests on. Checked again at finalisation: an
    /// operator jailed during the ballot does not take a seat on the strength
    /// of votes cast before anyone knew.
    pub node: BytesN<32>,
    /// Basis points cast for them, summed as ballots arrive. `u64` because it
    /// is a sum of `u32` weights and the electorate is not bounded.
    pub weight: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Election {
    pub id: u64,
    pub opened_at: u64,
    /// Nominations close and the ballot opens here.
    pub ballot_opens: u64,
    /// The ballot closes here, and finalisation becomes possible.
    pub closes: u64,

    /// Seats and quorum as they stood when the election was opened.
    ///
    /// Captured for the same reason a governance proposal captures its own
    /// eta: a `set_config` landing mid-election must not move the bar
    /// underneath a ballot that is already being cast against it.
    pub seats: u32,
    pub quorum: u32,

    pub status: ElectionStatus,
    /// Ledger time at which it was finalised. Zero while running.
    pub finalized_at: u64,
    /// Ballots cast, and the total weight behind them. The turnout is the
    /// number that says whether a result meant anything.
    pub ballots: u32,
    pub turnout: u64,
    /// Seats actually filled. Zero unless `status` is `Seated`.
    pub seated: u32,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Addresses entitled to vote on disputes.
    Committee,
    /// Monotonic dispute id allocator.
    DisputeCounter,
    Dispute(u64),
    /// One vote per member per voting round.
    Vote(u64, u32, Address),
    /// The identity of an allegation, so it cannot be filed twice.
    Filed(BytesN<32>, Symbol, u64),

    /// Monotonic election id allocator.
    ElectionCounter,
    /// The election that has not been finalised yet, if there is one. Absent
    /// otherwise, which is what makes "is an election running" a single read.
    OpenElection,
    /// Ledger time from which another election may be opened.
    NextElection,
    Election(u64),
    /// The candidates of an election, in the order they stood. That order is
    /// the tie-break, so it has to be the stored order and not a set.
    Candidates(u64),
    /// One ballot per node per election, keyed by the node rather than its
    /// owner: weight follows nodes, so an operator running three of them
    /// votes three times.
    Ballot(u64, BytesN<32>),
}

/// Ledger units. Roughly 30 days of extension whenever a record is within 7
/// days of expiry.
pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;
