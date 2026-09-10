use soroban_sdk::{contracttype, Address, String, Symbol, Val, Vec};

/// What the ledger records about a proposal.
///
/// There is deliberately no `Expired` here. Expiry is a fact about the clock,
/// and nothing writes it: a proposal nobody executed in time was not cancelled
/// by anybody, and a status that said so would put a decision in the record
/// that no one made. [`ProposalState`] is what an operator reads.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProposalStatus {
    Queued,
    Executed,
    Cancelled,
}

/// What a proposal is doing right now, which is the stored status combined
/// with the clock.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProposalState {
    /// Queued; the delay has not been served yet.
    Waiting,
    /// Queued and executable now, by anyone.
    Ready,
    /// Queued, but the grace period ran out. Nothing will execute it, and it
    /// has to be proposed again — which serves the delay again.
    Expired,
    Executed,
    Cancelled,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// May cancel a queued proposal, and may do nothing else. Not a proposer,
    /// unless separately named as one.
    pub guardian: Address,

    /// How long a proposal waits between being queued and becoming
    /// executable. This is the whole feature: it is the window in which an
    /// operator who dislikes a change can withdraw stake before it binds them.
    pub delay: u64,

    /// How long a proposal stays executable after its delay is served. Past
    /// it, the proposal is dead. Without this a proposal queued and forgotten
    /// a year ago could still be fired at a network it no longer suits.
    pub grace_period: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    pub id: u64,
    pub proposer: Address,

    /// The contract this call lands on, and the function it calls there.
    /// Nothing here is restricted to Aphelion's own contracts: what makes a
    /// target governable is that it named this contract as its admin.
    pub target: Address,
    pub function: Symbol,
    pub args: Vec<Val>,

    /// Where the rationale lives — a URL or content hash. As with dispute
    /// evidence, deliberately not the rationale itself: ledger space is the
    /// wrong place for prose, and a hash is enough to prove nobody rewrote it
    /// after the fact.
    pub description: String,

    pub proposed_at: u64,
    /// Executable from here.
    pub eta: u64,
    /// Dead from here.
    ///
    /// Both of these are captured when the proposal is queued rather than
    /// recomputed from the live config at execution. A proposal that shortened
    /// the delay must not shorten the wait of the proposals queued alongside
    /// it, or the delay would be escapable in one step by a proposer who
    /// queued the two together.
    pub expires_at: u64,

    pub status: ProposalStatus,
    /// Ledger time at which it executed. Zero until then.
    pub executed_at: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Addresses entitled to queue proposals.
    Proposers,
    /// Monotonic proposal id allocator.
    ProposalCounter,
    Proposal(u64),
}

/// A day. An emergency that cannot survive one is not answered by a governance
/// contract anyway: it is answered by consumers' own `max_age` checks and by
/// nodes declining to sign, both of which act in seconds and need nobody's
/// permission.
pub const MIN_DELAY: u64 = 24 * 3600;

/// Thirty days. A delay long enough that no proposal can be executed before
/// the operators who dislike it have unbonded is a delay long enough; past
/// that it stops being a safeguard and becomes a way to disable governance by
/// proposing one absurd number.
pub const MAX_DELAY: u64 = 30 * 24 * 3600;

pub const MIN_GRACE_PERIOD: u64 = 24 * 3600;
pub const MAX_GRACE_PERIOD: u64 = 30 * 24 * 3600;

/// Ledger units. Roughly 30 days of extension whenever a record is within 7
/// days of expiry.
pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;
