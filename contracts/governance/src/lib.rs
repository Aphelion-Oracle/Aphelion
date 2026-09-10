#![no_std]
//! # Aphelion governance
//!
//! The registry, the aggregator and the slashing contract each hold an `admin`
//! address, and that address can do things no consensus is asked about: change
//! the minimum stake, repoint a contract at a different aggregator, alter the
//! dispute bonds, replace the committee, hand admin to somebody else. Every one
//! of those is a single key acting instantly and, until the transaction lands,
//! invisibly.
//!
//! That is the same shape of failure the rest of the network exists to remove.
//! A price nobody can move alone is not worth much if the parameters around it
//! can be rewritten in one transaction by one person.
//!
//! This contract is the admin instead. It cannot do anything the previous admin
//! could not; what it removes is *instantly* and *invisibly*.
//!
//! ```text
//!   propose ──▶ published, eta = now + delay
//!                  │
//!                  │  delay: anyone can read the exact call that is coming,
//!                  │  and an operator who dislikes it can unbond
//!                  ▼
//!               executable ──▶ execute (permissionless)
//!                  │
//!                  └── cancel, by the guardian or the proposer
//! ```
//!
//! ## Why the guardian can only cancel
//!
//! The guardian may stop a queued proposal and may do nothing else: it cannot
//! queue one, cannot execute one, and cannot change this contract's own
//! configuration. So a stolen guardian key stalls governance until a proposal
//! moves the guardian; it cannot move stake, prices, or admin rights anywhere.
//! A key whose only power is to say no is a key worth handing to somebody other
//! than the proposer.
//!
//! ## Why execution is permissionless
//!
//! The call was fixed when it was queued and the delay is a fact about the
//! clock, so there is nothing left for the caller to decide. Restricting
//! execution to the proposer would add no safety and would let a proposer strand
//! a change everyone had already agreed to by going quiet.
//!
//! ## Why this contract governs itself
//!
//! Its own delay, guardian and proposer set are changed the same way as
//! anything else: by a proposal that names this contract as its target and
//! serves the delay first. Changing the delay therefore takes the delay. A
//! timelock whose delay could be set to zero in one transaction is a timelock
//! for exactly as long as nobody attacks it.
//!
//! Soroban refuses contract re-entry, so `execute` cannot make that call by
//! invoking this contract — it dispatches the action internally instead, from
//! [`SelfAction`]. The consequence is worth more than the mechanism: there is
//! no public `set_config` on this contract to protect, and an entry point that
//! does not exist cannot be left unguarded by a later edit.
//!
//! `MIN_DELAY` and `MAX_DELAY` bound the delay even so: a proposal cannot
//! collapse it to nothing, and cannot set it so long that governance is dead.
//!
//! ## What this costs
//!
//! An urgent parameter change now takes a day. That is a real loss and it is
//! worth being plain about it: if a feed has to stop being trusted *now*, this
//! contract is not the instrument. Consumers' own `max_age` arguments, the
//! aggregator's out-of-band rejection and an operator's freedom to stop signing
//! all act immediately and need nobody's permission — and unlike an emergency
//! admin power, none of them can be aimed at anything else.

mod error;
mod events;
mod types;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test;

pub use error::GovernanceError;
pub use types::*;

use soroban_sdk::{
    contract, contractimpl, panic_with_error, Address, Env, String, Symbol, TryFromVal, Val, Vec,
};

use events::{ConfigChanged, ProposalCancelled, ProposalExecuted, ProposalQueued, ProposerChanged};

#[contract]
pub struct Governance;

#[contractimpl]
impl Governance {
    // -- lifecycle ----------------------------------------------------------

    /// Deploy-time setup. The guardian authorises it, because the guardian is
    /// the one role this contract can never appoint for itself afterwards
    /// without already having a working proposer.
    pub fn initialize(env: Env, config: Config, proposers: Vec<Address>) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, GovernanceError::AlreadyInitialized);
        }
        config.guardian.require_auth();
        Self::validate_config(&env, &config);

        if proposers.is_empty() {
            panic_with_error!(&env, GovernanceError::NoProposersLeft);
        }
        for i in 0..proposers.len() {
            for j in (i + 1)..proposers.len() {
                if proposers.get_unchecked(i) == proposers.get_unchecked(j) {
                    panic_with_error!(&env, GovernanceError::AlreadyProposer);
                }
            }
        }

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage()
            .instance()
            .set(&DataKey::Proposers, &proposers);
        env.storage()
            .instance()
            .set(&DataKey::ProposalCounter, &0u64);
    }

    // -- proposals ----------------------------------------------------------

    /// Queue a call for later, and publish it now.
    ///
    /// The call is stored exactly as it will be made — target, function and
    /// arguments — so what the delay publishes is the change itself and not a
    /// description of it that could turn out to differ.
    pub fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        description: String,
    ) -> u64 {
        proposer.require_auth();
        let config = Self::load_config(&env);

        if !Self::proposers(env.clone()).contains(&proposer) {
            panic_with_error!(&env, GovernanceError::NotProposer);
        }

        // A proposal against this contract is decoded now rather than in three
        // days' time. Publishing a governance change, waiting out the delay and
        // only then discovering it names a function that does not exist would
        // spend the delay on nothing.
        if target == env.current_contract_address() {
            SelfAction::decode(&env, &function, &args);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCounter)
            .unwrap_or(0)
            + 1;
        env.storage().instance().set(&DataKey::ProposalCounter, &id);

        let now = env.ledger().timestamp();
        let eta = now + config.delay;
        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            target: target.clone(),
            function: function.clone(),
            args,
            description: description.clone(),
            proposed_at: now,
            eta,
            expires_at: eta + config.grace_period,
            status: ProposalStatus::Queued,
            executed_at: 0,
        };
        Self::save_proposal(&env, &proposal);

        ProposalQueued {
            id,
            target,
            proposer,
            function,
            description,
            eta,
            expires_at: proposal.expires_at,
        }
        .publish(&env);

        id
    }

    /// Make the call. Permissionless once the delay has been served.
    pub fn execute(env: Env, id: u64) {
        let mut proposal = Self::load_proposal(&env, id);
        if proposal.status != ProposalStatus::Queued {
            panic_with_error!(&env, GovernanceError::WrongPhase);
        }

        let now = env.ledger().timestamp();
        if now < proposal.eta {
            panic_with_error!(&env, GovernanceError::StillWaiting);
        }
        if now > proposal.expires_at {
            panic_with_error!(&env, GovernanceError::Expired);
        }

        // Recorded as executed before the call is made, not after. Soroban
        // refuses re-entry today, but a proposal that could re-enter `execute`
        // during its own target call would otherwise run twice off one delay.
        proposal.status = ProposalStatus::Executed;
        proposal.executed_at = now;
        Self::save_proposal(&env, &proposal);

        if proposal.target == env.current_contract_address() {
            SelfAction::decode(&env, &proposal.function, &proposal.args).apply(&env);
        } else {
            env.invoke_contract::<Val>(&proposal.target, &proposal.function, proposal.args);
        }

        ProposalExecuted {
            id,
            target: proposal.target,
            function: proposal.function,
            executed_at: now,
        }
        .publish(&env);
    }

    /// Stop a queued proposal, permanently.
    ///
    /// The guardian may cancel anything; a proposer may withdraw their own.
    /// There is no un-cancel: reviving a proposal would return a call to the
    /// executable state without it having served a fresh delay, which is the
    /// one thing this contract exists to prevent.
    pub fn cancel(env: Env, canceller: Address, id: u64) {
        canceller.require_auth();
        let config = Self::load_config(&env);
        let mut proposal = Self::load_proposal(&env, id);

        if proposal.status != ProposalStatus::Queued {
            panic_with_error!(&env, GovernanceError::WrongPhase);
        }
        // Nothing left to prevent, and cancelling it would write a decision
        // into the record that the clock had already made.
        if env.ledger().timestamp() > proposal.expires_at {
            panic_with_error!(&env, GovernanceError::Expired);
        }

        let vetoed = canceller == config.guardian;
        if !vetoed && canceller != proposal.proposer {
            panic_with_error!(&env, GovernanceError::NotCancellable);
        }

        proposal.status = ProposalStatus::Cancelled;
        Self::save_proposal(&env, &proposal);

        ProposalCancelled {
            id,
            canceller,
            vetoed,
        }
        .publish(&env);
    }

    // -- reads --------------------------------------------------------------

    pub fn get_config(env: Env) -> Config {
        Self::load_config(&env)
    }

    pub fn proposers(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Proposers)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn get_proposal(env: Env, id: u64) -> Option<Proposal> {
        env.storage().persistent().get(&DataKey::Proposal(id))
    }

    /// The stored status combined with the clock, which is what a watcher
    /// actually wants: whether this proposal can still land on anyone.
    pub fn state(env: Env, id: u64) -> ProposalState {
        let proposal = Self::load_proposal(&env, id);
        match proposal.status {
            ProposalStatus::Executed => ProposalState::Executed,
            ProposalStatus::Cancelled => ProposalState::Cancelled,
            ProposalStatus::Queued => {
                let now = env.ledger().timestamp();
                if now < proposal.eta {
                    ProposalState::Waiting
                } else if now > proposal.expires_at {
                    ProposalState::Expired
                } else {
                    ProposalState::Ready
                }
            }
        }
    }

    pub fn proposal_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::ProposalCounter)
            .unwrap_or(0)
    }

    // -- internals ----------------------------------------------------------

    fn load_config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, GovernanceError::NotInitialized))
    }

    fn load_proposal(env: &Env, id: u64) -> Proposal {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(id))
            .unwrap_or_else(|| panic_with_error!(env, GovernanceError::UnknownProposal))
    }

    fn save_proposal(env: &Env, proposal: &Proposal) {
        let key = DataKey::Proposal(proposal.id);
        env.storage().persistent().set(&key, proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn validate_config(env: &Env, config: &Config) {
        let sane = config.delay >= MIN_DELAY
            && config.delay <= MAX_DELAY
            && config.grace_period >= MIN_GRACE_PERIOD
            && config.grace_period <= MAX_GRACE_PERIOD;
        if !sane {
            panic_with_error!(env, GovernanceError::InvalidConfig);
        }
    }
}

/// The three things this contract can be asked to do to itself.
///
/// Deliberately not a `contracttype`: it is never stored and never crosses the
/// ledger boundary. A governance proposal carries the same `(function, args)`
/// shape as any other proposal, and this is where that shape is checked against
/// what the contract can actually do — once when the proposal is queued, so a
/// mistake costs a transaction rather than a delay, and again when it runs.
pub enum SelfAction {
    SetConfig(Config),
    AddProposer(Address),
    RemoveProposer(Address),
}

impl SelfAction {
    fn decode(env: &Env, function: &Symbol, args: &Vec<Val>) -> Self {
        if *function == Symbol::new(env, "set_config") {
            let config: Config = Self::only_arg(env, args);
            // Refused here as well as at initialisation: a delay outside the
            // bounds is no more acceptable for arriving by proposal.
            Governance::validate_config(env, &config);
            SelfAction::SetConfig(config)
        } else if *function == Symbol::new(env, "add_proposer") {
            SelfAction::AddProposer(Self::only_arg(env, args))
        } else if *function == Symbol::new(env, "remove_proposer") {
            SelfAction::RemoveProposer(Self::only_arg(env, args))
        } else {
            panic_with_error!(env, GovernanceError::UnknownAction)
        }
    }

    fn apply(self, env: &Env) {
        match self {
            SelfAction::SetConfig(config) => {
                env.storage().instance().set(&DataKey::Config, &config);
                ConfigChanged {
                    guardian: config.guardian,
                    delay: config.delay,
                    grace_period: config.grace_period,
                }
                .publish(env);
            }

            SelfAction::AddProposer(proposer) => {
                let mut proposers = Governance::proposers(env.clone());
                if proposers.contains(&proposer) {
                    panic_with_error!(env, GovernanceError::AlreadyProposer);
                }
                proposers.push_back(proposer.clone());
                env.storage()
                    .instance()
                    .set(&DataKey::Proposers, &proposers);

                ProposerChanged {
                    proposer,
                    added: true,
                    count: proposers.len(),
                }
                .publish(env);
            }

            // Refuses to remove the last proposer. Every route into this
            // contract's own configuration runs through a proposal, so a
            // proposer set emptied by accident cannot be refilled: the
            // parameters of the whole network would be frozen at whatever they
            // happened to be, permanently.
            SelfAction::RemoveProposer(proposer) => {
                let proposers = Governance::proposers(env.clone());
                if !proposers.contains(&proposer) {
                    panic_with_error!(env, GovernanceError::NotProposer);
                }
                if proposers.len() == 1 {
                    panic_with_error!(env, GovernanceError::NoProposersLeft);
                }

                let mut remaining = Vec::new(env);
                for p in proposers.iter() {
                    if p != proposer {
                        remaining.push_back(p);
                    }
                }
                env.storage()
                    .instance()
                    .set(&DataKey::Proposers, &remaining);

                ProposerChanged {
                    proposer,
                    added: false,
                    count: remaining.len(),
                }
                .publish(env);
            }
        }
    }

    fn only_arg<T: TryFromVal<Env, Val>>(env: &Env, args: &Vec<Val>) -> T {
        if args.len() != 1 {
            panic_with_error!(env, GovernanceError::InvalidArguments);
        }
        T::try_from_val(env, &args.get_unchecked(0))
            .unwrap_or_else(|_| panic_with_error!(env, GovernanceError::InvalidArguments))
    }
}
