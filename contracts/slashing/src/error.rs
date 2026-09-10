use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum SlashingError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,
    /// A configuration value that could never work: a quorum of zero, a
    /// negative bond, an appeal bond cheaper than the original.
    InvalidConfig = 4,
    InvalidAmount = 5,

    /// The address is not on the committee.
    NotCommitteeMember = 10,
    /// The same address appears twice in a committee. A duplicate would count
    /// twice towards quorum and vote once, which is a committee that can never
    /// resolve anything.
    AlreadyCommitteeMember = 11,
    /// Removing this member would leave the committee unable to reach quorum.
    CommitteeTooSmall = 12,

    /// No dispute with that id.
    UnknownDispute = 20,
    /// The accused public key is not registered.
    UnknownNode = 21,
    /// The same allegation has already been filed.
    DuplicateDispute = 22,
    /// The dispute is not in a state where this action makes sense.
    WrongPhase = 23,
    /// Voting has closed.
    VotingClosed = 24,
    /// Voting is still open.
    VotingOpen = 25,
    /// This member has already voted in this round.
    AlreadyVoted = 26,
    /// A node's own operator may not vote on a dispute against it.
    ConflictOfInterest = 27,
    /// The appeal window has not closed yet.
    AppealWindowOpen = 28,
    /// The appeal window has closed.
    AppealWindowClosed = 29,
    /// A dispute may only be appealed once.
    AlreadyAppealed = 30,
    /// The dispute has already been settled.
    AlreadySettled = 31,

    /// An election is already running. There is only ever one.
    ElectionRunning = 40,
    /// No election is running, or no election has that id.
    UnknownElection = 41,
    /// The seated committee has not served its term yet.
    TermNotServed = 42,
    /// The election is not in a phase where this action makes sense:
    /// nominating after nominations closed, balloting before they did,
    /// finalising twice.
    WrongElectionPhase = 43,
    /// Standing twice in one election.
    AlreadyNominated = 44,
    /// A ballot for somebody who did not stand.
    NotCandidate = 45,
    /// This node has already cast its ballot in this election.
    AlreadyBalloted = 46,
    /// The node is not registered, is not owned by this caller, or carries no
    /// voting weight -- jailed, exiting, or never active.
    NotEligible = 47,
}
