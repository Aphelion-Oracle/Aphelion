use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum RegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// The caller is not the admin.
    NotAdmin = 3,
    /// Only the aggregator may adjust reputation.
    NotAggregator = 4,
    /// Only the aggregator or the slashing contract may penalise.
    NotSlasher = 5,
    NodeNotFound = 6,
    NodeAlreadyRegistered = 7,
    /// The bonded amount is below `min_stake`.
    StakeTooLow = 8,
    /// Withdrawal attempted before `unbonding_until`.
    StillBonded = 9,
    /// The node is exiting and may not take on new obligations.
    NodeExiting = 10,
    /// A non-positive amount was supplied where a positive one is required.
    InvalidAmount = 11,
    /// The reward pool cannot cover the requested payout.
    RewardPoolExhausted = 12,
    /// The slash pool cannot cover the requested payout.
    SlashPoolExhausted = 13,
}
