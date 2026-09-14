use soroban_sdk::{contracttype, Address, BytesN, Vec};

/// Where a round is in its life.
///
/// `Failed` is a recorded outcome rather than an absence. A round that did not
/// reach `min_participants` produced no beacon, and a consumer must be able to
/// tell that from a round that has not finished yet — one will never have an
/// answer and the other will.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoundStatus {
    Committing,
    Revealing,
    Finalized,
    Failed,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// In a deployed network this is the timelock, like every other admin here.
    pub admin: Address,
    /// Consulted for who may take part, and for the penalty applied to a node
    /// that commits and then does not reveal.
    pub registry: Address,

    /// How long commitments are accepted after a round opens.
    pub commit_window: u64,
    /// How long reveals are accepted after the commit window closes.
    pub reveal_window: u64,
    /// Reveals needed before a beacon is published at all.
    ///
    /// This is the security parameter. The output is unpredictable as long as
    /// at least one revealed secret was chosen by somebody who did not know
    /// the others, so the floor is really "how many independent parties do we
    /// insist took part".
    pub min_participants: u32,
    /// Minimum gap between one round opening and the next.
    pub min_round_interval: u64,

    /// Reputation taken from a node that committed and did not reveal.
    pub no_show_rep_penalty: u32,
    /// Stake taken from the same node.
    ///
    /// Withholding a reveal is the one attack this construction cannot
    /// prevent outright (see the module documentation), so it is priced
    /// instead. These two are the price.
    pub no_show_slash: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct Round {
    pub id: u64,
    pub opened_at: u64,
    /// Commitments are accepted up to here.
    pub commit_deadline: u64,
    /// Reveals are accepted up to here.
    pub reveal_deadline: u64,

    /// Keys that committed, in the order they did.
    pub committed: Vec<BytesN<32>>,
    /// Keys that revealed. Always a subset of `committed`.
    pub revealed: Vec<BytesN<32>>,

    /// The XOR of every secret revealed so far.
    ///
    /// XOR rather than a running hash, and that is a security choice rather
    /// than a cheap one. A running hash makes the result depend on the order
    /// the reveals arrived in, and reveal order is something a participant
    /// chooses — so a node could submit, watch, resubmit and grind the output
    /// by timing alone. XOR is order-independent, so the only thing a
    /// participant controls is their own secret, which they were committed to
    /// before they saw anybody else's.
    pub accumulator: BytesN<32>,

    pub status: RoundStatus,
    /// The beacon. All zeroes until the round finalizes, and all zeroes
    /// forever if it failed — read it through `random`, which distinguishes
    /// the two.
    pub output: BytesN<32>,
    pub finalized_at: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Monotonic round id allocator.
    RoundCounter,
    /// The round accepting commitments or reveals, if there is one.
    CurrentRound,
    Round(u64),
    /// One node's commitment for one round.
    Commitment(u64, BytesN<32>),
    /// Set once a node has revealed, so a second reveal is refused without
    /// having to scan `revealed`.
    Revealed(u64, BytesN<32>),
}

// ---------------------------------------------------------------------------
// Governable parameter bounds
// ---------------------------------------------------------------------------
//
// Same reasoning as the aggregator's: the timelock decides when a parameter
// changes and has nothing to say about what to, so the values are bounded here.

/// A commit window shorter than this is one an operator's node can miss to a
/// slow ledger rather than to inattention.
pub const MIN_COMMIT_WINDOW: u64 = 30;
pub const MAX_COMMIT_WINDOW: u64 = 24 * 3600;

pub const MIN_REVEAL_WINDOW: u64 = 30;
pub const MAX_REVEAL_WINDOW: u64 = 24 * 3600;

/// Two. One "participant" is a beacon whose value one party chose alone, which
/// is not a beacon. As with the aggregator's quorum floor, this rules out the
/// value that defeats the construction rather than expressing a view about how
/// many participants a network should want.
pub const MIN_PARTICIPANTS_FLOOR: u32 = 2;
pub const MAX_PARTICIPANTS_FLOOR: u32 = 100;

pub const MAX_ROUND_INTERVAL: u64 = 30 * 24 * 3600;

/// The registry's whole reputation scale.
pub const MAX_NO_SHOW_REP_PENALTY: u32 = 10_000;

/// The bounds, readable from the chain. See the aggregator's `param_bounds`.
#[contracttype]
#[derive(Clone)]
pub struct ParamBounds {
    pub min_commit_window: u64,
    pub max_commit_window: u64,
    pub min_reveal_window: u64,
    pub max_reveal_window: u64,
    pub min_participants_floor: u32,
    pub max_participants_floor: u32,
    pub max_round_interval: u64,
    pub max_no_show_rep_penalty: u32,
}

/// Ledger units. Roughly 30 days of extension whenever a record is within 7
/// days of expiry — the same figures every other Aphelion contract uses.
pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;
