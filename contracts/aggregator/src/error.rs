use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum AggregatorError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,

    /// No such feed, or the feed has been disabled.
    UnknownFeed = 10,
    FeedDisabled = 11,
    FeedAlreadyExists = 12,

    /// The signature did not verify against the submitted public key.
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

    /// No price has ever been published for this feed.
    NoPrice = 30,
    /// The stored price is older than the caller's freshness requirement.
    StalePrice = 31,
    /// Not enough history to compute a TWAP over the requested window.
    InsufficientHistory = 32,

    /// The consumer has no prepaid balance left.
    InsufficientBalance = 40,
    InvalidAmount = 41,
}
