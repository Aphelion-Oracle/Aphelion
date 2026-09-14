use soroban_sdk::{contracttype, Address, BytesN, Symbol, Vec};

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
    /// How long a round may stay open waiting for quorum before the next
    /// submission abandons it and starts a fresh one. Without this, a round
    /// that never reaches quorum would wedge the feed permanently.
    pub round_timeout: u64,
    /// How long a node may be silent before `sweep_absent` may charge it a
    /// missed round.
    pub absence_threshold: u64,

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
    /// subset of nodes can source. Zero means "use the network quorum".
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
    pub submissions: Vec<Submission>,
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
    /// Monotonic round id allocator.
    RoundCounter,
    /// Ledger time of the last submission accepted from a node, on any feed.
    LastSeen(BytesN<32>),
    /// Ledger time at which a node was last charged for being absent, so one
    /// silence cannot be billed twice.
    Swept(BytesN<32>),
    /// Metered read fees collected but not yet forwarded to the reward pool.
    Fees,
}

/// Ledger units. Roughly 30 days of extension whenever a record is within 7
/// days of expiry.
pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;

// ---------------------------------------------------------------------------
// Governable parameter bounds
// ---------------------------------------------------------------------------
//
// Every parameter above is reachable by a timelocked proposal, and until these
// existed the only check on the value in that proposal was that somebody read
// it. `validate_config` rejected a quorum of zero and a negative fee — values
// that could never work — and accepted every value that merely destroys the
// guarantees the network is for: a quorum of one, a deviation band wider than
// any price could fall outside, a staleness window measured in years.
//
// The delay is not a substitute for this. It gives operators time to notice and
// unbond, which protects them individually and does nothing for a consumer
// reading the feed. A bound the contract enforces cannot be executed past.
//
// The bounds are deliberately wide. They are not an opinion about how the
// network should be tuned — that is what governance is for — they rule out the
// values at which a parameter stops meaning what its name says. The same
// pattern the governance contract already uses for `MIN_DELAY`/`MAX_DELAY`.

/// Two, not three. Three is the smallest quorum with a median that is somebody
/// else's number, and would be the better operating floor — but the floor a
/// contract enforces and the floor a network should choose are different
/// questions, and only one of them belongs here. What this rules out is one:
/// a quorum of one makes the aggregator a relay for a single key, which is the
/// exact failure the whole design exists to remove.
pub const MIN_QUORUM: u32 = 2;
/// Every submission in a round is iterated to close it, so the ceiling is a
/// bound on the work one transaction can do, not a view about network size.
pub const MAX_QUORUM: u32 = 100;

pub const MIN_WEIGHT_BPS_FLOOR: u32 = 1;
/// A hundred nodes at full weight. Above this no achievable set of submissions
/// could close a round, so the feed would be silently disabled by a number
/// that reads like a safety margin.
pub const MAX_WEIGHT_BPS_FLOOR: u32 = 1_000_000;

pub const MIN_DEVIATION_BPS: u32 = 1;
/// 100%. A band wider than this cannot be fallen out of — a price would have
/// to be more than double the median to be penalised — so the outlier penalty
/// becomes unreachable and dishonest submissions stop costing anything.
pub const MAX_DEVIATION_BPS: u32 = 10_000;

pub const MIN_STALENESS: u64 = 1;
/// A day. Past this, "the current price" is not a description of anything.
pub const MAX_STALENESS: u64 = 24 * 3600;

/// An hour of tolerance for a clock running fast is already generous; beyond
/// it the check is not a tolerance, it is an invitation to timestamp forward.
pub const MAX_FUTURE_DRIFT_LIMIT: u64 = 3600;

/// A day. A feed that may publish at most once a day is a feed nothing can
/// safely borrow against, and the heartbeat advertised to consumers would be
/// a promise the contract refuses to let nodes keep.
pub const MAX_ROUND_INTERVAL: u64 = 24 * 3600;

pub const MIN_ROUND_TIMEOUT: u64 = 1;
pub const MAX_ROUND_TIMEOUT: u64 = 24 * 3600;

pub const MIN_ABSENCE_THRESHOLD: u64 = 1;
/// Thirty days. An absence nobody may charge for a month is an absence the
/// network is carrying for a month.
pub const MAX_ABSENCE_THRESHOLD: u64 = 30 * 24 * 3600;

pub const MIN_HISTORY_LEN: u32 = 1;
/// The ring is read in full to compute a TWAP, so this bounds that read.
pub const MAX_HISTORY_LEN: u32 = 1_000;

/// The registry's whole reputation scale. A penalty of more than the maximum
/// reputation is a penalty that jails on the first mistake whatever the node's
/// standing, which is a decision for the jail threshold rather than a side
/// effect of an outlier band.
pub const MAX_OUTLIER_REP_PENALTY: u32 = 10_000;

/// The bounds, readable from the chain.
///
/// Governance proposals are a target, a function and a `Vec<Val>`, and the
/// delay is spent with a reviewer looking at that blob. Publishing the limits
/// means the check can be made before the proposal is queued rather than
/// discovered when it executes — and means the node's tooling can show an
/// operator what range a number is allowed to move in without that range being
/// duplicated off chain, where it would go stale.
#[contracttype]
#[derive(Clone)]
pub struct ParamBounds {
    pub min_quorum: u32,
    pub max_quorum: u32,
    pub min_weight_bps_floor: u32,
    pub max_weight_bps_floor: u32,
    pub min_deviation_bps: u32,
    pub max_deviation_bps: u32,
    pub min_staleness: u64,
    pub max_staleness: u64,
    pub max_future_drift_limit: u64,
    pub max_round_interval: u64,
    pub min_round_timeout: u64,
    pub max_round_timeout: u64,
    pub min_absence_threshold: u64,
    pub max_absence_threshold: u64,
    pub min_history_len: u32,
    pub max_history_len: u32,
    pub max_outlier_rep_penalty: u32,
}
