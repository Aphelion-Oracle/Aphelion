//! Contract events.
//!
//! Everything a node operator or an indexer needs to reconstruct why a round
//! ended the way it did, without replaying the contract. Penalties in
//! particular are published with the numbers that produced them: an operator
//! disputing a slash should be able to check the arithmetic from the event
//! alone.

use soroban_sdk::{contractevent, BytesN, Symbol};

/// A round closed and a new price is live.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceUpdated {
    #[topic]
    pub feed: Symbol,
    pub price: i128,
    pub round_id: u64,
    pub num_nodes: u32,
    pub confidence_bps: u32,
    pub timestamp: u64,
}

/// A submission passed verification and joined the open round.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmissionAccepted {
    #[topic]
    pub feed: Symbol,
    #[topic]
    pub pubkey: BytesN<32>,
    pub price: i128,
    pub nonce: u64,
    pub weight_bps: u32,
    pub round_id: u64,
}

/// A submission landed outside the consensus band and was penalised.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlierPenalized {
    #[topic]
    pub feed: Symbol,
    #[topic]
    pub pubkey: BytesN<32>,
    pub price: i128,
    pub median: i128,
    pub deviation_bps: u32,
    pub round_id: u64,
}

/// A round closed without any submission inside the band, so no price was
/// published. Loud on purpose: this is the network failing to agree, which is
/// a different and more serious condition than a quiet feed.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundFailed {
    #[topic]
    pub feed: Symbol,
    pub round_id: u64,
    pub median: i128,
    pub num_submissions: u32,
}

/// A round was abandoned without reaching quorum before `round_timeout`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundAbandoned {
    #[topic]
    pub feed: Symbol,
    pub round_id: u64,
    pub num_submissions: u32,
    pub opened_at: u64,
}

/// A node was charged a missed round for prolonged silence.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeAbsent {
    #[topic]
    pub pubkey: BytesN<32>,
    pub last_seen: u64,
    pub silent_for: u64,
}

/// Collected read fees were pushed into the registry's reward pool.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeesForwarded {
    pub amount: i128,
}

/// A feed was added or reconfigured.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedConfigured {
    #[topic]
    pub feed: Symbol,
    pub enabled: bool,
    pub heartbeat: u64,
    pub min_nodes: u32,
}
