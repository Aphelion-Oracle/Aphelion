use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum AggregatorError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,
    /// A configuration value that could never work was supplied — a negative
    /// fee, a negative reward, a heartbeat of zero. Rejected at the point it
    /// is set rather than at the round it would have broken.
    ///
    /// Values that *would* work and should still not be allowed are
    /// `ParameterOutOfRange` instead.
    InvalidConfig = 4,
    /// An address that must be a contract (the registry, this aggregator) is
    /// an account address instead.
    NotContractAddress = 5,
    /// A parameter is outside the range this contract publishes in
    /// `param_bounds`. Distinct from `InvalidConfig`: the value would work, it
    /// just stops the parameter meaning what its name says — a quorum of one,
    /// a deviation band no price can fall outside. Governance may tune these;
    /// it may not tune them past the point where they are guarantees.
    ParameterOutOfRange = 6,
    /// Two parameters that are each within range and cannot both hold — a
    /// future-drift tolerance wider than the staleness window, an absence
    /// threshold shorter than the time a round is allowed to stay open.
    InconsistentConfig = 7,

    /// No such feed, or the feed has been disabled.
    UnknownFeed = 10,
    FeedDisabled = 11,
    FeedAlreadyExists = 12,

    /// The signature did not verify against the submitted public key.
    ///
    /// The host's `ed25519_verify` traps rather than returning, so a forged
    /// signature usually surfaces as a host error rather than this code. It is
    /// kept as the documented meaning of a verification failure.
    BadSignature = 20,
    /// The submitting key is not registered, is jailed, or is exiting.
    NotAuthorizedNode = 21,
    /// The nonce did not strictly increase for this (node, feed).
    NonceNotIncreasing = 22,
    /// The observation is older than `max_staleness`.
    StaleObservation = 23,
    /// The observation is timestamped further ahead than `max_future_drift`.
    FutureObservation = 24,
    /// This node has already submitted for the open round.
    DuplicateSubmission = 25,
    /// A round was closed for this feed too recently.
    RoundTooSoon = 26,
    /// A non-positive price was submitted.
    InvalidPrice = 27,
    /// The round's arithmetic overflowed, which for realistic prices means a
    /// submission far outside any plausible range.
    MathOverflow = 28,

    /// No price has ever been published for this feed.
    NoPrice = 30,
    /// The stored price is older than the caller's freshness requirement.
    StalePrice = 31,
    /// Not enough history to compute a TWAP over the requested window.
    InsufficientHistory = 32,
    /// A TWAP window of zero seconds, or a `max_age` that cannot be met.
    InvalidWindow = 33,

    /// The consumer has no prepaid balance left.
    InsufficientBalance = 40,
    InvalidAmount = 41,
}
