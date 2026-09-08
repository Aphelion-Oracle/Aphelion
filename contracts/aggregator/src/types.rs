use soroban_sdk::{contracttype, Address, BytesN, Symbol};

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,
    /// The registry consulted for every submission's voting weight.
    pub registry: Address,
    /// Token used for consumer fees and node rewards.
    pub token: Address,

    /// Minimum number of distinct nodes required to close a round.
    pub quorum: u32,
    /// Minimum total voting weight required to close a round.
    ///
    /// Separate from `quorum` on purpose: five brand-new half-weight nodes
    /// should not be able to close a round that was meant to need five proven
    /// ones. Requiring both a headcount and a weight makes "recruit many fresh
    /// identities" a strictly worse attack than "earn reputation honestly".
    pub min_weight_bps: u32,

    /// A submission further than this from the round's median is penalised.
    pub max_deviation_bps: u32,
    /// Observations older than this are rejected.
    pub max_staleness: u64,
    /// Tolerance for a node whose clock runs slightly fast.
    pub max_future_drift: u64,
    /// Minimum gap between two published rounds for one feed.
    pub min_round_interval: u64,

    /// Paid to each node whose submission landed inside the band.
    pub reward_per_submission: i128,
    /// Reputation removed from a node outside the band.
    pub outlier_rep_penalty: u32,
    /// Stake seized from a node outside the band.
    pub outlier_slash: i128,

    /// How many past observations to retain per feed for TWAP.
    pub history_len: u32,
    /// Charged to a consumer per metered read.
    pub read_fee: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct FeedConfig {
    pub feed: Symbol,
    pub enabled: bool,
    /// Longest acceptable gap between publications, advertised to consumers so
    /// they can size their own staleness checks.
    pub heartbeat: u64,
    /// Feed-specific override of the network quorum, for feeds that only a
    /// subset of nodes can source.
    pub min_nodes: u32,
}

/// One node's contribution to an open round.
#[contracttype]
#[derive(Clone)]
pub struct Submission {
    pub pubkey: BytesN<32>,
    pub price: i128,
    pub timestamp: u64,
    pub confidence_bps: u32,
    /// Weight as of the moment of submission. Captured here rather than read
    /// again at finalisation so that a reputation change mid-round cannot
    /// retroactively re-weight votes that were already cast.
    pub weight_bps: u32,
}

#[contracttype]
#[derive(Clone)]
pub struct PendingRound {
    pub round_id: u64,
    pub opened_at: u64,
    pub submissions: soroban_sdk::Vec<Submission>,
}

/// The published price for a feed.
#[contracttype]
#[derive(Clone)]
pub struct PriceData {
    /// Scaled by 1e8. See `aphelion-core::price`.
    pub price: i128,
    /// The oldest observation time among the contributing submissions --
    /// deliberately the most pessimistic, so a consumer's freshness check
    /// cannot be satisfied by one fast node in an otherwise stale round.
    pub timestamp: u64,
    pub num_nodes: u32,
    /// Half-width of the network's confidence interval, in basis points.
    pub confidence_bps: u32,
    /// Standard deviation across contributing submissions, scaled like `price`.
    pub deviation: i128,
    pub round_id: u64,
    /// Ledger time at which the round closed.
    pub published_at: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Observation {
    pub timestamp: u64,
    pub price: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Index of every configured feed.
    Feeds,
    Feed(Symbol),
    /// Current published price.
    Price(Symbol),
    /// Round currently accepting submissions.
    Round(Symbol),
    /// Ring buffer of past observations, for TWAP.
    History(Symbol),
    /// Highest nonce accepted from a node for a feed.
    Nonce(BytesN<32>, Symbol),
    /// Prepaid balance of a metered consumer.
    Balance(Address),
}
