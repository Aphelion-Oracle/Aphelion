use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// A configuration value that could never work: a delay under the floor or
    /// over the ceiling, a grace period outside the same bounds.
    InvalidConfig = 4,

    /// The address may not queue proposals.
    NotProposer = 10,
    AlreadyProposer = 11,
    /// Removing this proposer would leave nobody able to queue anything, which
    /// would freeze every parameter of the network permanently.
    NoProposersLeft = 12,
    /// Only the guardian or the proposal's own proposer may cancel it.
    NotCancellable = 13,

    /// No proposal with that id.
    UnknownProposal = 20,
    /// The proposal has already been executed or cancelled.
    WrongPhase = 21,
    /// The delay has not been served yet.
    StillWaiting = 22,
    /// The grace period has run out. Propose it again and serve the delay.
    Expired = 23,

    /// A proposal aimed at this contract names something it cannot do to
    /// itself. Checked when the proposal is queued, not when it runs.
    UnknownAction = 24,
    /// The arguments do not fit the action they were queued for.
    InvalidArguments = 25,
}
