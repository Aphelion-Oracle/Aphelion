//! Contract events.
//!
//! The delay is only worth what it publishes. A change nobody could see coming
//! has served its waiting period in private, which is the same as not having
//! served it, so every step of a proposal's life is published: what it calls,
//! on what, from when, and who stopped it.

use soroban_sdk::{contractevent, Address, String, Symbol};

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalQueued {
    #[topic]
    pub id: u64,
    #[topic]
    pub target: Address,
    pub proposer: Address,
    pub function: Symbol,
    pub description: String,
    pub eta: u64,
    pub expires_at: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalExecuted {
    #[topic]
    pub id: u64,
    #[topic]
    pub target: Address,
    pub function: Symbol,
    pub executed_at: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalCancelled {
    #[topic]
    pub id: u64,
    pub canceller: Address,
    /// True when the canceller was the guardian rather than the proposer, so
    /// a veto is distinguishable from a withdrawal in the record.
    pub vetoed: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposerChanged {
    #[topic]
    pub proposer: Address,
    pub added: bool,
    pub count: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigChanged {
    pub guardian: Address,
    pub delay: u64,
    pub grace_period: u64,
}
