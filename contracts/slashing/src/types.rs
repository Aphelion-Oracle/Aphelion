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

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Addresses entitled to vote.
    Committee,
    /// Monotonic dispute id allocator.
    DisputeCounter,
    Dispute(u64),
    /// One vote per member per voting round.
    Vote(u64, u32, Address),
    /// The identity of an allegation, so it cannot be filed twice.
    Filed(BytesN<32>, Symbol, u64),
}

/// Ledger units. Roughly 30 days of extension whenever a record is within 7
/// days of expiry.
pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;
