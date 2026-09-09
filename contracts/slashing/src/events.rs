//! Contract events.
//!
//! A dispute is the one part of Aphelion where a human judgement moves money.
//! Every step of it is published so that the record of who voted which way,
//! and on what evidence, is on the ledger rather than in a chat log.

use soroban_sdk::{contractevent, Address, BytesN, String, Symbol};

use crate::types::DisputeStatus;

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeOpened {
    #[topic]
    pub id: u64,
    #[topic]
    pub accused: BytesN<32>,
    pub reporter: Address,
    pub feed: Symbol,
    pub round_id: u64,
    pub evidence: String,
    pub deadline: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoteCast {
    #[topic]
    pub id: u64,
    #[topic]
    pub member: Address,
    pub uphold: bool,
    pub vote_round: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeResolved {
    #[topic]
    pub id: u64,
    pub status: DisputeStatus,
    pub votes_for: u32,
    pub votes_against: u32,
    /// When the appeal window closes and settlement becomes possible.
    pub settles_at: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeAppealed {
    #[topic]
    pub id: u64,
    pub appellant: Address,
    pub appealed_from: DisputeStatus,
    pub deadline: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeSettled {
    #[topic]
    pub id: u64,
    #[topic]
    pub accused: BytesN<32>,
    pub upheld: bool,
    pub slashed: i128,
    pub reporter_paid: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitteeChanged {
    #[topic]
    pub member: Address,
    pub added: bool,
    pub size: u32,
}
