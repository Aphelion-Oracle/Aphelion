//! Contract events.
//!
//! A dispute is the one part of Aphelion where a human judgement moves money.
//! Every step of it is published so that the record of who voted which way,
//! and on what evidence, is on the ledger rather than in a chat log.

use soroban_sdk::{contractevent, Address, BytesN, String, Symbol, Vec};

use crate::types::{DisputeStatus, ElectionStatus};

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

/// An election is open for nominations.
///
/// Published at the open rather than at the close, because the whole point of
/// a nomination period is that an operator can find out one is running while
/// there is still time to stand in it.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElectionOpened {
    #[topic]
    pub id: u64,
    pub seats: u32,
    pub quorum: u32,
    pub ballot_opens: u64,
    pub closes: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateNominated {
    #[topic]
    pub id: u64,
    #[topic]
    pub candidate: Address,
    pub node: BytesN<32>,
}

/// One node's ballot, with the weight it carried at the moment it was cast.
///
/// The weight is in the event because it is the part nobody can reconstruct
/// afterwards: the node's reputation moves every round, and the tally is a sum
/// of what each node was worth when it voted, not what it is worth now.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BallotCast {
    #[topic]
    pub id: u64,
    #[topic]
    pub node: BytesN<32>,
    pub voter: Address,
    pub candidate: Address,
    pub weight: u32,
}

/// The result, and the committee it produced.
///
/// The full committee is published rather than a diff against the last one: a
/// seating replaces every seat at once, and a reader who missed one event
/// should not have to replay the chain to know who can slash them.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElectionClosed {
    #[topic]
    pub id: u64,
    pub status: ElectionStatus,
    pub committee: Vec<Address>,
    pub ballots: u32,
    pub turnout: u64,
    /// When another election may be opened.
    pub next_election: u64,
}
