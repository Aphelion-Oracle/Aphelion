use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum RandomnessError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,
    /// A configuration value that could never work: a window of zero, a
    /// minimum of zero participants.
    InvalidConfig = 4,
    /// A parameter outside the range this contract publishes in
    /// `param_bounds`. See the aggregator's error of the same name for why the
    /// two are separate.
    ParameterOutOfRange = 5,
    NotContractAddress = 6,

    /// A round is already running, and only one runs at a time.
    RoundInProgress = 10,
    /// Not enough time has passed since the last round was opened.
    RoundTooSoon = 11,
    UnknownRound = 12,

    /// The commit window has closed, or has not opened.
    NotCommitting = 20,
    /// The reveal window has closed, or has not opened.
    NotRevealing = 21,
    /// This node has already committed to this round. One commitment per node
    /// per round: a node that could commit twice could reveal whichever of the
    /// two suited it once it had seen the others.
    AlreadyCommitted = 22,
    /// This node committed and has already revealed.
    AlreadyRevealed = 23,
    /// This node did not commit to this round, so it has nothing to reveal.
    /// Revealing without committing would be contributing entropy chosen after
    /// seeing everybody else's.
    DidNotCommit = 24,
    /// The revealed secret does not hash to the commitment.
    BadReveal = 25,
    /// The signature did not verify against the submitting key.
    BadSignature = 26,
    /// The key is not registered, is jailed, or is exiting.
    NotAuthorizedNode = 27,

    /// The reveal window has not closed, and not every committer has revealed.
    NotReadyToFinalize = 30,
    /// Already finalized, or already recorded as failed.
    AlreadyFinalized = 31,
    /// No beacon for that round: it failed, or has not finalized.
    NoOutput = 32,
    /// A bound of zero has no values in it.
    InvalidBound = 33,
}
