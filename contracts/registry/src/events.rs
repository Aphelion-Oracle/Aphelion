//! Contract events.
//!
//! Every change to a node's standing is published with the numbers that caused
//! it. An operator contesting a penalty should be able to reconstruct the
//! arithmetic from the event alone, without replaying the contract or trusting
//! an indexer's interpretation of it.

use soroban_sdk::{contractevent, Address, BytesN, Symbol};

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRegistered {
    #[topic]
    pub pubkey: BytesN<32>,
    pub owner: Address,
    pub stake: i128,
    pub reputation: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakeAdded {
    #[topic]
    pub pubkey: BytesN<32>,
    pub amount: i128,
    pub total_stake: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnbondingStarted {
    #[topic]
    pub pubkey: BytesN<32>,
    pub unbonding_until: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Withdrawn {
    #[topic]
    pub pubkey: BytesN<32>,
    pub owner: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardsFunded {
    pub from: Address,
    pub amount: i128,
    pub pool: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeJailed {
    #[topic]
    pub pubkey: BytesN<32>,
    pub reputation: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeUnjailed {
    #[topic]
    pub pubkey: BytesN<32>,
    pub reputation: u32,
}

/// A penalty was applied. `reason` distinguishes the aggregator's mechanical
/// outlier check from a resolved dispute, because the two carry very different
/// evidentiary weight and an operator reading their history needs to tell them
/// apart.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodePenalized {
    #[topic]
    pub pubkey: BytesN<32>,
    #[topic]
    pub reason: Symbol,
    pub reputation_delta: u32,
    pub seized: i128,
    pub reputation: u32,
    pub stake: i128,
}

/// Seized stake was paid out of the slash pool by the slashing contract.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlashPoolPaid {
    #[topic]
    pub to: Address,
    pub amount: i128,
    pub pool: i128,
}
