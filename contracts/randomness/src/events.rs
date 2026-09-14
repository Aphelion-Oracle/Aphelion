//! Contract events.
//!
//! A beacon is only as trustworthy as the record of how it was produced, so
//! every step is published: who committed, who revealed, who committed and did
//! not reveal, and what the round finally output. A consumer that wants to
//! check a number was not chosen by one party can reconstruct the whole round
//! from these.

use soroban_sdk::{contractevent, BytesN};

use crate::types::RoundStatus;

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundOpened {
    #[topic]
    pub round_id: u64,
    pub opened_at: u64,
    pub commit_deadline: u64,
    pub reveal_deadline: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Committed {
    #[topic]
    pub round_id: u64,
    #[topic]
    pub node: BytesN<32>,
    pub commitment: BytesN<32>,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revealed {
    #[topic]
    pub round_id: u64,
    #[topic]
    pub node: BytesN<32>,
}

/// A node that committed and let the reveal window close.
///
/// Published separately from the finalisation because it is the one event in
/// this contract that costs somebody money, and because it is the signal an
/// operator watching for a griefing campaign would want to alert on.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoShow {
    #[topic]
    pub round_id: u64,
    #[topic]
    pub node: BytesN<32>,
    pub rep_penalty: u32,
    pub slashed: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundFinalized {
    #[topic]
    pub round_id: u64,
    pub status: RoundStatus,
    /// All zeroes when the round failed.
    pub output: BytesN<32>,
    pub committed: u32,
    pub revealed: u32,
}
