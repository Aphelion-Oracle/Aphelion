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
//!
//! ## Who the committee is
//!
//! Seats are won in an election, not handed out. Every `term_length` anyone may
//! open one; operators stand on a node they own, operators vote with the nodes
//! they own, and the ballot is weighted by exactly the weight those nodes carry
//! in the price feed itself.
//!
//! ```text
//!   open_election ──▶ nominate ──▶ cast_ballot ──▶ finalize_election
//!   (anyone, once      (an          (one ballot     (anyone, once the
//!    the term is        operator,    per node,       ballot closes; seats
//!    served)            on a node    weighted)       the top `seats`)
//!                       they own)
//! ```
//!
//! ### Why the electorate is the node set
//!
//! The committee's power is to take an operator's stake. Handing the choice of
//! who holds it to the people whose stake is at risk is the only electorate
//! that does not require trusting somebody outside the system, and weight is
//! already the network's answer to "how much should this identity count" — a
//! Sybil-resistant one, since fresh identities start at half weight and a
//! jailed node is worth nothing.
//!
//! ### Why one ballot names one candidate
//!
//! A ballot elects `seats` members but names a single candidate. Letting each
//! voter name a full slate would let anybody with a bare majority of weight
//! take *every* seat; naming one means a faction holding more than
//! `1/(seats+1)` of the weight can seat somebody no matter who else votes. The
//! committee that judges operators should not be winnable outright by whoever
//! is largest this quarter.
//!
//! ### Why the incumbents stay when an election fails
//!
//! An election that draws fewer eligible candidates than `quorum` seats nobody
//! and leaves the sitting committee where it is. The alternative — vacating the
//! seats — would mean an attacker could switch slashing off for everyone by
//! suppressing turnout, which is the same fail-open the quorum rule above
//! exists to refuse. The cost is that a committee nobody replaces holds over
//! indefinitely, and that is the honest trade: this contract would rather be
//! governed by a stale committee than by none.
//!
//! ### What governance kept
//!
//! Admin — the timelock — can still *remove* a member, and can no longer add
//! one. That asymmetry is deliberate and is the same one the timelock's own
//! guardian has: a power that can only subtract cannot install anybody, so the
//! worst a captured admin key achieves is a smaller committee, and it cannot
//! shrink one below its quorum either. Seats are only ever filled by a vote.

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
    BallotCast, CandidateNominated, CommitteeChanged, DisputeAppealed, DisputeOpened,
    DisputeResolved, DisputeSettled, ElectionClosed, ElectionOpened, VoteCast,
};

/// The slice of the registry this contract depends on.
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    fn owner_of(env: Env, pubkey: BytesN<32>) -> Option<Address>;
    /// Voting weight in basis points; zero for a node that is unknown, jailed
    /// or exiting. This is the whole of the franchise: an election asks the
    /// registry the same question the aggregator asks it every round, so
    /// nothing about who counts has to be maintained twice.
    fn weight_of(env: Env, pubkey: BytesN<32>) -> u32;
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
        // A duplicate counts twice towards the check above and votes once, so
        // a genesis committee of [A, A, B] with a quorum of three passes here
        // and can never resolve a dispute for as long as it sits.
        for i in 0..committee.len() {
            for j in (i + 1)..committee.len() {
                if committee.get_unchecked(i) == committee.get_unchecked(j) {
                    panic_with_error!(&env, SlashingError::AlreadyCommitteeMember);
                }
            }
        }

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage()
            .instance()
            .set(&DataKey::Committee, &committee);
        env.storage()
            .instance()
            .set(&DataKey::DisputeCounter, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::ElectionCounter, &0u64);
        // The appointed committee a deployment starts with serves a term like
        // any elected one. There is no way to avoid appointing the first one —
        // an election needs an electorate, and at genesis there are no nodes.
        env.storage().instance().set(
            &DataKey::NextElection,
            &(env.ledger().timestamp() + config.term_length),
        );
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

    /// Remove a committee member.
    ///
    /// There is deliberately no matching `add_member`: seats are filled by
    /// [`Self::finalize_election`] and nothing else. Admin here is the
    /// governance timelock, so a removal is already published a day in advance
    /// and vetoable — but a power to *seat* somebody would let a proposal that
    /// nobody blocked in time install a whole committee, and the point of
    /// electing one is that it cannot be installed.
    ///
    /// Refuses to shrink the committee below the quorum it has to reach. A
    /// committee that cannot reach quorum dismisses every dispute filed
    /// against anybody, which is a quiet way to disable slashing entirely. The
    /// seat stays empty until the next election fills it; nothing promotes a
    /// runner-up, because a runner-up nobody elected to that seat is an
    /// appointment wearing an election's clothes.
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

    // -- elections ----------------------------------------------------------

    /// Open an election for every seat.
    ///
    /// Permissionless once the sitting committee has served `term_length`.
    /// Nobody has to be persuaded to call it, which is the point: a committee
    /// that could postpone its own replacement by going quiet would be an
    /// appointed one with extra steps.
    pub fn open_election(env: Env) -> u64 {
        let config = Self::load_config(&env);

        if env.storage().instance().has(&DataKey::OpenElection) {
            panic_with_error!(&env, SlashingError::ElectionRunning);
        }
        let now = env.ledger().timestamp();
        let next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextElection)
            .unwrap_or(0);
        if now < next {
            panic_with_error!(&env, SlashingError::TermNotServed);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ElectionCounter)
            .unwrap_or(0)
            + 1;
        env.storage().instance().set(&DataKey::ElectionCounter, &id);
        env.storage().instance().set(&DataKey::OpenElection, &id);

        let ballot_opens = now + config.nomination_period;
        let election = Election {
            id,
            opened_at: now,
            ballot_opens,
            closes: ballot_opens + config.election_period,
            seats: config.seats,
            quorum: config.quorum,
            status: ElectionStatus::Running,
            finalized_at: 0,
            ballots: 0,
            turnout: 0,
            seated: 0,
        };
        Self::save_election(&env, &election);

        ElectionOpened {
            id,
            seats: election.seats,
            quorum: election.quorum,
            ballot_opens,
            closes: election.closes,
        }
        .publish(&env);

        id
    }

    /// Stand for a seat, on the strength of a node you own.
    ///
    /// Candidacy is not open to everybody, and the node is why. Anyone can make
    /// an address; a node carrying weight costs `min_stake` bonded under a key
    /// with a history, which is what stops a nomination period from being
    /// filled with thousands of candidacies that finalisation would then have
    /// to count.
    ///
    /// It also decides what kind of committee this is: operators judging
    /// operators. That is a real cost — a committee drawn from the accused
    /// class can go easy on itself — and it is priced against the alternative,
    /// which is a committee drawn from people with nothing at stake in the
    /// network's honesty. The per-dispute conflict-of-interest rule and the
    /// reward that is paid to the reporter rather than the committee are what
    /// keep the first cost bounded.
    pub fn nominate(env: Env, candidate: Address, node: BytesN<32>) {
        candidate.require_auth();
        let config = Self::load_config(&env);
        let election = Self::load_open_election(&env);

        if Self::phase_of(&env, &election) != ElectionPhase::Nominating {
            panic_with_error!(&env, SlashingError::WrongElectionPhase);
        }
        Self::require_eligible(&env, &config, &candidate, &node);

        let mut candidates = Self::candidates(env.clone(), election.id);
        for c in candidates.iter() {
            if c.address == candidate {
                panic_with_error!(&env, SlashingError::AlreadyNominated);
            }
        }
        candidates.push_back(Candidate {
            address: candidate.clone(),
            node: node.clone(),
            weight: 0,
        });
        Self::save_candidates(&env, election.id, &candidates);

        CandidateNominated {
            id: election.id,
            candidate,
            node,
        }
        .publish(&env);
    }

    /// Cast one node's ballot for one candidate.
    ///
    /// The weight is read from the registry now and added to the tally now, so
    /// what a ballot is worth is what the node was worth at the moment it was
    /// cast. The aggregator captures a submission's weight the same way and for
    /// the same reason: re-reading it at the count would let a node's
    /// reputation moving between the two — up or down, honestly or not — silently
    /// re-weight a vote that was already in the box.
    pub fn cast_ballot(env: Env, voter: Address, node: BytesN<32>, candidate: Address) {
        voter.require_auth();
        let config = Self::load_config(&env);
        let mut election = Self::load_open_election(&env);

        if Self::phase_of(&env, &election) != ElectionPhase::Balloting {
            panic_with_error!(&env, SlashingError::WrongElectionPhase);
        }
        let weight = Self::require_eligible(&env, &config, &voter, &node);

        let ballot_key = DataKey::Ballot(election.id, node.clone());
        if env.storage().persistent().has(&ballot_key) {
            panic_with_error!(&env, SlashingError::AlreadyBalloted);
        }

        let mut candidates = Self::candidates(env.clone(), election.id);
        let mut standing = false;
        for i in 0..candidates.len() {
            let mut c = candidates.get_unchecked(i);
            if c.address == candidate {
                c.weight += weight as u64;
                candidates.set(i, c);
                standing = true;
                break;
            }
        }
        if !standing {
            panic_with_error!(&env, SlashingError::NotCandidate);
        }
        Self::save_candidates(&env, election.id, &candidates);

        env.storage().persistent().set(&ballot_key, &candidate);
        env.storage()
            .persistent()
            .extend_ttl(&ballot_key, TTL_THRESHOLD, TTL_EXTEND);

        election.ballots += 1;
        election.turnout += weight as u64;
        Self::save_election(&env, &election);

        BallotCast {
            id: election.id,
            node,
            voter,
            candidate,
            weight,
        }
        .publish(&env);
    }

    /// Count the ballots and seat the winners. Permissionless once the ballot
    /// has closed, for the same reason resolving a dispute is: the result is a
    /// function of votes already cast, so there is nothing left for the caller
    /// to influence.
    ///
    /// The whole committee is replaced at once — an incumbent who wants to stay
    /// stands again like anybody else. Staggering the seats would keep more
    /// continuity across a term, and is worth doing later; it is not worth
    /// doing before the simple version has been run against a real network.
    ///
    /// A seat change binds votes cast after it, and does not disturb votes
    /// already cast in an open dispute. Voiding them would let an election
    /// timed against a dispute erase evidence the committee had already
    /// recorded, and since quorum counts votes cast rather than seats filled, a
    /// mid-dispute reseating can never make a dispute easier to resolve than it
    /// was.
    pub fn finalize_election(env: Env) -> ElectionStatus {
        let config = Self::load_config(&env);
        let mut election = Self::load_open_election(&env);

        if Self::phase_of(&env, &election) != ElectionPhase::Counting {
            panic_with_error!(&env, SlashingError::WrongElectionPhase);
        }

        let registry = RegistryClient::new(&env, &config.registry);
        let candidates = Self::candidates(env.clone(), election.id);

        // Eligibility is settled once, before the count, so that the registry
        // is asked about each candidate exactly once however many seats there
        // are to fill.
        let mut skip = Vec::new(&env);
        for c in candidates.iter() {
            // No weight, no seat: a candidacy nobody voted for is not a
            // mandate, and seating one would let an unopposed slate take the
            // committee on zero turnout. And eligibility is rechecked here
            // rather than trusted from the nomination, so an operator jailed
            // during the ballot does not take a seat on the strength of votes
            // cast before anyone knew.
            skip.push_back(c.weight == 0 || registry.weight_of(&c.node) == 0);
        }

        // Selection over the nomination order rather than a sort of the whole
        // tally: `seats` is small, and taking the first maximum encountered
        // makes the tie-break the order candidates stood in — earliest wins,
        // which is a rule nobody can compute their way around after the fact.
        let mut winners = Vec::new(&env);
        while winners.len() < election.seats {
            let mut best: Option<u32> = None;
            let mut best_weight = 0u64;
            for i in 0..candidates.len() {
                if skip.get_unchecked(i) {
                    continue;
                }
                let weight = candidates.get_unchecked(i).weight;
                if weight > best_weight {
                    best = Some(i);
                    best_weight = weight;
                }
            }
            match best {
                Some(i) => {
                    skip.set(i, true);
                    winners.push_back(candidates.get_unchecked(i).address);
                }
                None => break,
            }
        }

        let now = env.ledger().timestamp();
        let seated = winners.len() >= election.quorum;

        if seated {
            env.storage().instance().set(&DataKey::Committee, &winners);
        }
        // A failed election may be retried immediately. It cost a nomination
        // period and a ballot to fail, so there is no spam to guard against,
        // and making the network wait out a full term for a committee it never
        // managed to elect would be a penalty aimed at the wrong party.
        let next_election = if seated {
            now + config.term_length
        } else {
            now
        };

        election.status = if seated {
            ElectionStatus::Seated
        } else {
            ElectionStatus::Failed
        };
        election.finalized_at = now;
        election.seated = if seated { winners.len() } else { 0 };
        Self::save_election(&env, &election);

        env.storage().instance().remove(&DataKey::OpenElection);
        env.storage()
            .instance()
            .set(&DataKey::NextElection, &next_election);

        ElectionClosed {
            id: election.id,
            status: election.status,
            // The committee as it now stands, which on a failure is the
            // incumbent one. A reader wants to know who can slash them, not
            // what changed.
            committee: Self::committee(env.clone()),
            ballots: election.ballots,
            turnout: election.turnout,
            next_election,
        }
        .publish(&env);

        election.status
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

    pub fn get_election(env: Env, id: u64) -> Option<Election> {
        env.storage().persistent().get(&DataKey::Election(id))
    }

    /// The stored status combined with the clock, which is what a candidate
    /// actually wants: whether there is still time to stand, to vote, or
    /// neither.
    pub fn election_phase(env: Env, id: u64) -> ElectionPhase {
        let election = Self::load_election(&env, id);
        Self::phase_of(&env, &election)
    }

    /// Everyone standing in an election, in the order they stood, with the
    /// weight cast for each so far. Readable while the ballot is open: a
    /// running tally is what lets an operator see a candidate they dislike
    /// pulling ahead while their own vote can still answer it.
    pub fn candidates(env: Env, id: u64) -> Vec<Candidate> {
        env.storage()
            .persistent()
            .get(&DataKey::Candidates(id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Who a node voted for in an election, if it voted.
    pub fn ballot_of(env: Env, id: u64, node: BytesN<32>) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Ballot(id, node))
    }

    /// The election that has not been finalised yet, if there is one.
    pub fn current_election(env: Env) -> Option<u64> {
        env.storage().instance().get(&DataKey::OpenElection)
    }

    /// Ledger time from which the next election may be opened.
    pub fn next_election(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextElection)
            .unwrap_or(0)
    }

    pub fn election_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::ElectionCounter)
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

    fn load_election(env: &Env, id: u64) -> Election {
        env.storage()
            .persistent()
            .get(&DataKey::Election(id))
            .unwrap_or_else(|| panic_with_error!(env, SlashingError::UnknownElection))
    }

    fn load_open_election(env: &Env) -> Election {
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenElection)
            .unwrap_or_else(|| panic_with_error!(env, SlashingError::UnknownElection));
        Self::load_election(env, id)
    }

    fn save_election(env: &Env, election: &Election) {
        let key = DataKey::Election(election.id);
        env.storage().persistent().set(&key, election);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn save_candidates(env: &Env, id: u64, candidates: &Vec<Candidate>) {
        let key = DataKey::Candidates(id);
        env.storage().persistent().set(&key, candidates);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn phase_of(env: &Env, election: &Election) -> ElectionPhase {
        match election.status {
            ElectionStatus::Seated => ElectionPhase::Seated,
            ElectionStatus::Failed => ElectionPhase::Failed,
            ElectionStatus::Running => {
                let now = env.ledger().timestamp();
                if now < election.ballot_opens {
                    ElectionPhase::Nominating
                } else if now < election.closes {
                    ElectionPhase::Balloting
                } else {
                    ElectionPhase::Counting
                }
            }
        }
    }

    /// A node the caller owns which carries weight, and how much. Standing and
    /// voting ask exactly the same question, so they ask it in one place: the
    /// franchise and the eligibility to hold a seat are the same fact about
    /// the registry, and two copies of it would eventually disagree.
    fn require_eligible(env: &Env, config: &Config, who: &Address, node: &BytesN<32>) -> u32 {
        let registry = RegistryClient::new(env, &config.registry);
        if registry.owner_of(node).as_ref() != Some(who) {
            panic_with_error!(env, SlashingError::NotEligible);
        }
        let weight = registry.weight_of(node);
        if weight == 0 {
            panic_with_error!(env, SlashingError::NotEligible);
        }
        weight
    }

    fn validate_config(env: &Env, config: &Config) {
        let sane = config.quorum > 0
            && config.voting_period > 0
            && config.appeal_period > 0
            && config.dispute_bond >= 0
            && config.appeal_bond >= config.dispute_bond
            && config.slash_amount >= 0
            && config.reporter_reward >= 0
            // A full house that still could not reach quorum is not a
            // committee; it is a slashing switch left in the off position.
            && config.seats >= config.quorum
            && config.nomination_period > 0
            && config.election_period > 0
            && config.term_length > 0;
        if !sane {
            panic_with_error!(env, SlashingError::InvalidConfig);
        }
    }
}
