#![no_std]
//! # Aphelion dispute resolution
//!
//! The aggregator penalises what it can prove arithmetically: a submission
//! outside the consensus band, measured against a median the contract computed
//! itself. That covers a node that lies about a price in a round it took part
//! in. It does not cover everything an operator can do wrong — colluding
//! across rounds, running someone else's key, feeding a manipulated venue on
//! purpose — because none of those are visible in one round's arithmetic.
//!
//! This contract is where those cases are decided by people, on evidence, with
//! money at stake on both sides.
//!
//! ## Why a bond on both sides
//!
//! Filing a dispute costs a bond, forfeited if the committee finds the
//! allegation baseless. Without it, a competitor can bury an operator in
//! disputes for free. Appealing costs a larger bond, forfeited if the appeal
//! does not change the outcome, because an appeal asks the whole committee to
//! do its work a second time.
//!
//! ## Why the reward is not paid by the seizure
//!
//! An upheld dispute seizes stake into the registry's slash pool, and the
//! reporter is paid *from that pool* in a separate step rather than out of the
//! seizure itself. The difference matters: if the reward were carved directly
//! out of the penalty, a committee would have a standing financial interest in
//! finding against nodes, and the size of that interest would scale with the
//! size of the penalty.
//!
//! ## Fail closed
//!
//! A dispute that does not reach voting quorum is dismissed, not upheld.
//! Silence from the committee is not evidence against an operator, and a
//! quorum rule that treated it as such would let an attacker slash a node by
//! ensuring nobody was watching.

mod error;
mod events;
mod types;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test;

pub use error::SlashingError;
pub use types::*;

use soroban_sdk::{
    contract, contractclient, contractimpl, panic_with_error, token, Address, BytesN, Env, String,
    Symbol, Vec,
};

use events::{
    CommitteeChanged, DisputeAppealed, DisputeOpened, DisputeResolved, DisputeSettled, VoteCast,
};

/// The slice of the registry this contract depends on.
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    fn owner_of(env: Env, pubkey: BytesN<32>) -> Option<Address>;
    fn slash(env: Env, pubkey: BytesN<32>, reputation_delta: u32, slash_amount: i128);
    fn pay_from_slash_pool(env: Env, to: Address, amount: i128);
    fn slash_pool(env: Env) -> i128;
}

#[contract]
pub struct Slashing;

#[contractimpl]
impl Slashing {
    // -- lifecycle ----------------------------------------------------------

    pub fn initialize(env: Env, config: Config, committee: Vec<Address>) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, SlashingError::AlreadyInitialized);
        }
        config.admin.require_auth();
        Self::validate_config(&env, &config);
        if committee.len() < config.quorum {
            panic_with_error!(&env, SlashingError::CommitteeTooSmall);
        }

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage()
            .instance()
            .set(&DataKey::Committee, &committee);
        env.storage()
            .instance()
            .set(&DataKey::DisputeCounter, &0u64);
    }

    pub fn set_config(env: Env, config: Config) {
        let current = Self::load_config(&env);
        current.admin.require_auth();
        Self::validate_config(&env, &config);
        if Self::committee(env.clone()).len() < config.quorum {
            panic_with_error!(&env, SlashingError::CommitteeTooSmall);
        }
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn get_config(env: Env) -> Config {
        Self::load_config(&env)
    }

    pub fn committee(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Committee)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn add_member(env: Env, member: Address) {
        let config = Self::load_config(&env);
        config.admin.require_auth();

        let mut committee = Self::committee(env.clone());
        if committee.contains(&member) {
            panic_with_error!(&env, SlashingError::AlreadyCommitteeMember);
        }
        committee.push_back(member.clone());
        env.storage()
            .instance()
            .set(&DataKey::Committee, &committee);

        CommitteeChanged {
            member,
            added: true,
            size: committee.len(),
        }
        .publish(&env);
    }

    /// Remove a committee member.
    ///
    /// Refuses to shrink the committee below the quorum it has to reach. A
    /// committee that cannot reach quorum dismisses every dispute filed
    /// against anybody, which is a quiet way to disable slashing entirely.
    pub fn remove_member(env: Env, member: Address) {
        let config = Self::load_config(&env);
        config.admin.require_auth();

        let committee = Self::committee(env.clone());
        if !committee.contains(&member) {
            panic_with_error!(&env, SlashingError::NotCommitteeMember);
        }
        if committee.len() - 1 < config.quorum {
            panic_with_error!(&env, SlashingError::CommitteeTooSmall);
        }

        let mut remaining = Vec::new(&env);
        for m in committee.iter() {
            if m != member {
                remaining.push_back(m);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::Committee, &remaining);

        CommitteeChanged {
            member,
            added: false,
            size: remaining.len(),
        }
        .publish(&env);
    }

    // -- disputes -----------------------------------------------------------

    /// File an allegation against a node, posting the dispute bond.
    ///
    /// The allegation is identified by `(accused, feed, round_id)`, and each
    /// one may be filed once. Otherwise the same claim could be re-filed after
    /// settlement to seize stake repeatedly for a single offence.
    pub fn open_dispute(
        env: Env,
        reporter: Address,
        accused: BytesN<32>,
        feed: Symbol,
        round_id: u64,
        evidence: String,
    ) -> u64 {
        reporter.require_auth();
        let config = Self::load_config(&env);

        if RegistryClient::new(&env, &config.registry)
            .owner_of(&accused)
            .is_none()
        {
            panic_with_error!(&env, SlashingError::UnknownNode);
        }

        let filed_key = DataKey::Filed(accused.clone(), feed.clone(), round_id);
        if env.storage().persistent().has(&filed_key) {
            panic_with_error!(&env, SlashingError::DuplicateDispute);
        }

        token::Client::new(&env, &config.token).transfer(
            &reporter,
            env.current_contract_address(),
            &config.dispute_bond,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::DisputeCounter)
            .unwrap_or(0)
            + 1;
        env.storage().instance().set(&DataKey::DisputeCounter, &id);

        let now = env.ledger().timestamp();
        let dispute = Dispute {
            id,
            accused: accused.clone(),
            reporter: reporter.clone(),
            feed: feed.clone(),
            round_id,
            evidence: evidence.clone(),
            bond: config.dispute_bond,
            opened_at: now,
            deadline: now + config.voting_period,
            resolved_at: 0,
            vote_round: 1,
            votes_for: 0,
            votes_against: 0,
            status: DisputeStatus::Voting,
            appellant: None,
            appeal_bond: 0,
            appealed_from: DisputeStatus::Voting,
        };
        Self::save_dispute(&env, &dispute);

        env.storage().persistent().set(&filed_key, &id);
        env.storage()
            .persistent()
            .extend_ttl(&filed_key, TTL_THRESHOLD, TTL_EXTEND);

        DisputeOpened {
            id,
            accused,
            reporter,
            feed,
            round_id,
            evidence,
            deadline: dispute.deadline,
        }
        .publish(&env);

        id
    }

    /// Cast a committee vote.
    ///
    /// An operator may not vote on a dispute against their own node. This is
    /// checked against the registry rather than trusted to committee etiquette,
    /// because the one case where it matters is the one where the member has
    /// every reason to ignore the etiquette.
    pub fn vote(env: Env, member: Address, dispute_id: u64, uphold: bool) {
        member.require_auth();
        let config = Self::load_config(&env);

        if !Self::committee(env.clone()).contains(&member) {
            panic_with_error!(&env, SlashingError::NotCommitteeMember);
        }

        let mut dispute = Self::load_dispute(&env, dispute_id);
        if dispute.status != DisputeStatus::Voting {
            panic_with_error!(&env, SlashingError::WrongPhase);
        }
        if env.ledger().timestamp() > dispute.deadline {
            panic_with_error!(&env, SlashingError::VotingClosed);
        }

        if RegistryClient::new(&env, &config.registry).owner_of(&dispute.accused)
            == Some(member.clone())
        {
            panic_with_error!(&env, SlashingError::ConflictOfInterest);
        }

        let vote_key = DataKey::Vote(dispute_id, dispute.vote_round, member.clone());
        if env.storage().persistent().has(&vote_key) {
            panic_with_error!(&env, SlashingError::AlreadyVoted);
        }
        env.storage().persistent().set(&vote_key, &uphold);
        env.storage()
            .persistent()
            .extend_ttl(&vote_key, TTL_THRESHOLD, TTL_EXTEND);

        if uphold {
            dispute.votes_for += 1;
        } else {
            dispute.votes_against += 1;
        }
        let vote_round = dispute.vote_round;
        Self::save_dispute(&env, &dispute);

        VoteCast {
            id: dispute_id,
            member,
            uphold,
            vote_round,
        }
        .publish(&env);
    }

    /// Close voting and record the outcome. Permissionless once the deadline
    /// has passed — the result is a function of votes already cast, so there
    /// is nothing for a caller to influence.
    pub fn resolve(env: Env, dispute_id: u64) -> DisputeStatus {
        let config = Self::load_config(&env);
        let mut dispute = Self::load_dispute(&env, dispute_id);

        if dispute.status != DisputeStatus::Voting {
            panic_with_error!(&env, SlashingError::WrongPhase);
        }
        let now = env.ledger().timestamp();
        if now <= dispute.deadline {
            panic_with_error!(&env, SlashingError::VotingOpen);
        }

        let cast = dispute.votes_for + dispute.votes_against;
        // Fails closed: too few votes, or no majority, and the node keeps its
        // stake. An attacker who can keep the committee quiet must not thereby
        // win the dispute.
        dispute.status = if cast >= config.quorum && dispute.votes_for > dispute.votes_against {
            DisputeStatus::Upheld
        } else {
            DisputeStatus::Dismissed
        };
        dispute.resolved_at = now;
        Self::save_dispute(&env, &dispute);

        DisputeResolved {
            id: dispute_id,
            status: dispute.status,
            votes_for: dispute.votes_for,
            votes_against: dispute.votes_against,
            settles_at: now + config.appeal_period,
        }
        .publish(&env);

        dispute.status
    }

    /// Contest a resolved dispute, posting the appeal bond and sending it back
    /// to the committee for a second vote.
    ///
    /// Either side may appeal, once. The bond is returned if the second vote
    /// changes the outcome and forfeited to the other side if it does not,
    /// which prices an appeal as what it is: a claim that the committee got it
    /// wrong, not a way to buy time.
    pub fn appeal(env: Env, appellant: Address, dispute_id: u64) {
        appellant.require_auth();
        let config = Self::load_config(&env);
        let mut dispute = Self::load_dispute(&env, dispute_id);

        if dispute.status != DisputeStatus::Upheld && dispute.status != DisputeStatus::Dismissed {
            panic_with_error!(&env, SlashingError::WrongPhase);
        }
        if dispute.appellant.is_some() {
            panic_with_error!(&env, SlashingError::AlreadyAppealed);
        }
        let now = env.ledger().timestamp();
        if now > dispute.resolved_at + config.appeal_period {
            panic_with_error!(&env, SlashingError::AppealWindowClosed);
        }

        token::Client::new(&env, &config.token).transfer(
            &appellant,
            env.current_contract_address(),
            &config.appeal_bond,
        );

        let appealed_from = dispute.status;
        dispute.appellant = Some(appellant.clone());
        dispute.appeal_bond = config.appeal_bond;
        dispute.appealed_from = appealed_from;
        dispute.status = DisputeStatus::Voting;
        dispute.deadline = now + config.voting_period;
        dispute.resolved_at = 0;
        dispute.vote_round += 1;
        dispute.votes_for = 0;
        dispute.votes_against = 0;
        Self::save_dispute(&env, &dispute);

        DisputeAppealed {
            id: dispute_id,
            appellant,
            appealed_from,
            deadline: dispute.deadline,
        }
        .publish(&env);
    }

    /// Move the money. Permissionless once the appeal window has closed.
    ///
    /// Upheld: the node is slashed, the reporter's bond is returned and the
    /// reporter is paid from the slash pool. Dismissed: the reporter's bond
    /// goes to the operator who had to answer the allegation.
    pub fn settle(env: Env, dispute_id: u64) {
        let config = Self::load_config(&env);
        let mut dispute = Self::load_dispute(&env, dispute_id);

        if dispute.status == DisputeStatus::Settled {
            panic_with_error!(&env, SlashingError::AlreadySettled);
        }
        if dispute.status == DisputeStatus::Voting {
            panic_with_error!(&env, SlashingError::WrongPhase);
        }
        let now = env.ledger().timestamp();
        if now <= dispute.resolved_at + config.appeal_period {
            panic_with_error!(&env, SlashingError::AppealWindowOpen);
        }

        let registry = RegistryClient::new(&env, &config.registry);
        let token = token::Client::new(&env, &config.token);
        let me = env.current_contract_address();

        let upheld = dispute.status == DisputeStatus::Upheld;
        let owner = registry
            .owner_of(&dispute.accused)
            .unwrap_or_else(|| dispute.reporter.clone());

        let mut slashed = 0i128;
        let mut reporter_paid = 0i128;

        if upheld {
            registry.slash(&dispute.accused, &config.rep_penalty, &config.slash_amount);
            slashed = config.slash_amount;

            // The reporter's own bond comes back first: it was collateral
            // against a frivolous claim, and the claim was not frivolous.
            if dispute.bond > 0 {
                token.transfer(&me, &dispute.reporter, &dispute.bond);
            }

            // The reward is capped at what the pool actually holds. A node
            // slashed down to an empty stake cannot fund a full reward, and a
            // dispute that was correct should not fail to settle because of it.
            let available = registry.slash_pool();
            let reward = if config.reporter_reward < available {
                config.reporter_reward
            } else {
                available
            };
            if reward > 0 {
                registry.pay_from_slash_pool(&dispute.reporter, &reward);
                reporter_paid = reward;
            }
        } else if dispute.bond > 0 {
            // Answering a baseless allegation costs an operator time and
            // attention; the bond is what compensates them for it.
            token.transfer(&me, &owner, &dispute.bond);
        }

        // An appeal that changed the outcome gets its bond back. One that did
        // not funds the side it dragged back through a second vote.
        if let Some(appellant) = dispute.appellant.clone() {
            if dispute.appeal_bond > 0 {
                let changed = dispute.appealed_from != dispute.status;
                let recipient = if changed {
                    appellant
                } else if upheld {
                    dispute.reporter.clone()
                } else {
                    owner.clone()
                };
                token.transfer(&me, &recipient, &dispute.appeal_bond);
            }
        }

        dispute.status = DisputeStatus::Settled;
        Self::save_dispute(&env, &dispute);

        DisputeSettled {
            id: dispute_id,
            accused: dispute.accused,
            upheld,
            slashed,
            reporter_paid,
        }
        .publish(&env);
    }

    // -- reads --------------------------------------------------------------

    pub fn get_dispute(env: Env, dispute_id: u64) -> Option<Dispute> {
        env.storage()
            .persistent()
            .get(&DataKey::Dispute(dispute_id))
    }

    /// How a member voted in the dispute's current voting round, if they have.
    pub fn vote_of(env: Env, dispute_id: u64, member: Address) -> Option<bool> {
        let dispute = Self::load_dispute(&env, dispute_id);
        env.storage()
            .persistent()
            .get(&DataKey::Vote(dispute_id, dispute.vote_round, member))
    }

    /// The dispute filed for an allegation, if one has been.
    pub fn dispute_for(env: Env, accused: BytesN<32>, feed: Symbol, round_id: u64) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::Filed(accused, feed, round_id))
    }

    pub fn dispute_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::DisputeCounter)
            .unwrap_or(0)
    }

    // -- internals ----------------------------------------------------------

    fn load_config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, SlashingError::NotInitialized))
    }

    fn load_dispute(env: &Env, id: u64) -> Dispute {
        env.storage()
            .persistent()
            .get(&DataKey::Dispute(id))
            .unwrap_or_else(|| panic_with_error!(env, SlashingError::UnknownDispute))
    }

    fn save_dispute(env: &Env, dispute: &Dispute) {
        let key = DataKey::Dispute(dispute.id);
        env.storage().persistent().set(&key, dispute);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn validate_config(env: &Env, config: &Config) {
        let sane = config.quorum > 0
            && config.voting_period > 0
            && config.appeal_period > 0
            && config.dispute_bond >= 0
            && config.appeal_bond >= config.dispute_bond
            && config.slash_amount >= 0
            && config.reporter_reward >= 0;
        if !sane {
            panic_with_error!(env, SlashingError::InvalidConfig);
        }
    }
}
