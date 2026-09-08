use soroban_sdk::{contracttype, Address, BytesN};

/// Reputation scale. Deliberately the same 0..=10_000 range as basis points,
/// so weight can be derived from reputation without a unit conversion.
pub const MAX_REPUTATION: u32 = 10_000;

/// Where a newly registered node starts: half weight.
///
/// A new operator has not yet demonstrated anything, and the cost of being
/// wrong about them is borne by every consumer of the feed. Starting at half
/// weight means an attacker who registers ten fresh identities buys five
/// nodes' worth of influence for ten nodes' worth of stake — the arithmetic
/// that makes a Sybil attack unattractive rather than merely detectable.
pub const STARTING_REPUTATION: u32 = 5_000;

/// At or above this, a node votes at full weight.
pub const FULL_WEIGHT_THRESHOLD: u32 = 7_000;

/// Below this, a node is jailed and votes at zero weight.
pub const JAIL_THRESHOLD: u32 = 3_000;

/// Reputation gained for a submission inside the consensus band.
pub const REPUTATION_REWARD: u32 = 50;

/// Reputation lost for a submission outside it.
pub const REPUTATION_PENALTY: u32 = 500;

/// Reputation lost for missing a round the node should have participated in.
pub const REPUTATION_MISS: u32 = 25;

#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeStatus {
    /// Registered, bonded and voting.
    Active,
    /// Reputation has fallen below the jail threshold. Still bonded, but its
    /// submissions carry no weight until reputation recovers.
    Jailed,
    /// Unbonding has been requested; the node no longer votes and its stake
    /// is released once the unbonding period elapses.
    Exiting,
}

#[contracttype]
#[derive(Clone)]
pub struct Node {
    /// Ed25519 public key. This, not an `Address`, is the node's identity:
    /// price submissions are authenticated by signature, so any account may
    /// relay them and the paying account is irrelevant to authority.
    pub pubkey: BytesN<32>,
    /// The account that bonded the stake and may withdraw it.
    pub owner: Address,
    pub stake: i128,
    pub reputation: u32,
    pub status: NodeStatus,
    pub registered_at: u64,
    pub last_submission: u64,
    pub consecutive_misses: u32,
    pub total_rewards: i128,
    pub total_slashed: i128,
    /// Ledger time after which an exiting node may withdraw. Zero when not
    /// exiting.
    pub unbonding_until: u64,
}

/// A node plus its derived voting weight, which is what callers actually want.
#[contracttype]
#[derive(Clone)]
pub struct NodeView {
    pub pubkey: BytesN<32>,
    pub owner: Address,
    pub stake: i128,
    pub reputation: u32,
    pub status: NodeStatus,
    pub weight_bps: u32,
    pub last_submission: u64,
    pub consecutive_misses: u32,
    pub total_rewards: i128,
    pub total_slashed: i128,
    pub unbonding_until: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,
    /// The aggregator contract, the only caller allowed to move reputation.
    pub aggregator: Address,
    /// The slashing contract, allowed to penalise after a dispute.
    pub slasher: Address,
    /// Token used for stake and rewards (the native XLM SAC in production).
    pub token: Address,
    pub min_stake: i128,
    /// Delay between requesting an exit and being able to withdraw.
    ///
    /// This is what gives a dispute time to be raised: a node that publishes a
    /// bad price and immediately unbonds must still be slashable for as long
    /// as the misbehaviour can be noticed.
    pub unbonding_period: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// One entry per registered node.
    Node(BytesN<32>),
    /// Index of registered public keys, so the set can be enumerated.
    NodeIndex,
    /// Undistributed rewards held by this contract.
    RewardPool,
    /// Slashed stake awaiting governance disposal.
    SlashPool,
}

/// Voting weight in basis points, derived from reputation.
///
/// A step function rather than something continuous: it is trivial to reason
/// about, cheap to compute, and gives an operator a clear target ("get above
/// 7000") instead of a curve they have to model.
pub fn weight_for(status: &NodeStatus, reputation: u32) -> u32 {
    match status {
        NodeStatus::Active if reputation >= FULL_WEIGHT_THRESHOLD => 10_000,
        NodeStatus::Active if reputation >= JAIL_THRESHOLD => 5_000,
        _ => 0,
    }
}
