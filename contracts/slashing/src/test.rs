#![cfg(test)]

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{symbol_short, Address, BytesN, Env, String, Symbol, Vec};

use crate::{Config, Dispute, DisputeStatus, Slashing, SlashingClient};
use aphelion_registry::{Registry, RegistryClient};

const BASE_TIME: u64 = 1_735_689_600;
const MIN_STAKE: i128 = 1_000_0000000;
const UNBONDING: u64 = 7 * 24 * 3600;

const DISPUTE_BOND: i128 = 100_0000000;
const APPEAL_BOND: i128 = 300_0000000;
const SLASH_AMOUNT: i128 = 500_0000000;
const REPORTER_REWARD: i128 = 50_0000000;
const VOTING_PERIOD: u64 = 3 * 24 * 3600;
const APPEAL_PERIOD: u64 = 2 * 24 * 3600;

struct Harness<'a> {
    env: Env,
    slashing: SlashingClient<'a>,
    registry: RegistryClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    committee: std::vec::Vec<Address>,
    reporter: Address,
    accused: BytesN<32>,
    accused_owner: Address,
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(BASE_TIME);

    let admin = Address::generate(&env);
    let aggregator = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let registry_id = env.register(Registry, ());
    let slashing_id = env.register(Slashing, ());

    let registry = RegistryClient::new(&env, &registry_id);
    registry.initialize(
        &admin,
        &aggregator,
        &slashing_id,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
    );

    let committee: std::vec::Vec<Address> = (0..5).map(|_| Address::generate(&env)).collect();
    let mut committee_vec = Vec::new(&env);
    for m in &committee {
        committee_vec.push_back(m.clone());
    }

    let slashing = SlashingClient::new(&env, &slashing_id);
    slashing.initialize(
        &Config {
            admin: admin.clone(),
            registry: registry_id,
            token: sac.address(),
            quorum: 3,
            voting_period: VOTING_PERIOD,
            appeal_period: APPEAL_PERIOD,
            dispute_bond: DISPUTE_BOND,
            appeal_bond: APPEAL_BOND,
            rep_penalty: 2_000,
            slash_amount: SLASH_AMOUNT,
            reporter_reward: REPORTER_REWARD,
        },
        &committee_vec,
    );

    // The accused: a real registered node with real bonded stake.
    let accused_owner = Address::generate(&env);
    token_admin.mint(&accused_owner, &(MIN_STAKE * 4));
    let accused = BytesN::from_array(&env, &[7u8; 32]);
    registry.register(&accused_owner, &accused, &(MIN_STAKE * 2));

    let reporter = Address::generate(&env);
    token_admin.mint(&reporter, &(DISPUTE_BOND * 10));

    Harness {
        env,
        slashing,
        registry,
        token,
        token_admin,
        committee,
        reporter,
        accused,
        accused_owner,
    }
}

impl Harness<'_> {
    fn feed(&self) -> Symbol {
        Symbol::new(&self.env, "BTC_USD")
    }

    fn open(&self) -> u64 {
        self.slashing.open_dispute(
            &self.reporter,
            &self.accused,
            &self.feed(),
            &42,
            &String::from_str(&self.env, "ipfs://bafyevidence"),
        )
    }

    fn vote(&self, member: usize, id: u64, uphold: bool) {
        self.slashing.vote(&self.committee[member], &id, &uphold);
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn dispute(&self, id: u64) -> Dispute {
        self.slashing.get_dispute(&id).expect("dispute exists")
    }

    /// Run a dispute to a resolved (but not settled) state.
    fn resolved(&self, upholding: usize, against: usize) -> u64 {
        let id = self.open();
        for m in 0..upholding {
            self.vote(m, id, true);
        }
        for m in upholding..upholding + against {
            self.vote(m, id, false);
        }
        self.advance(VOTING_PERIOD + 1);
        self.slashing.resolve(&id);
        id
    }
}

// -- filing -----------------------------------------------------------------

#[test]
fn filing_a_dispute_locks_the_reporters_bond() {
    let h = setup();
    let before = h.token.balance(&h.reporter);

    let id = h.open();

    assert_eq!(id, 1);
    assert_eq!(h.token.balance(&h.reporter), before - DISPUTE_BOND);
    assert_eq!(h.token.balance(&h.slashing.address), DISPUTE_BOND);

    let dispute = h.dispute(id);
    assert_eq!(dispute.status, DisputeStatus::Voting);
    assert_eq!(dispute.accused, h.accused);
    assert_eq!(dispute.deadline, BASE_TIME + VOTING_PERIOD);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // UnknownNode
fn a_dispute_must_name_a_registered_node() {
    let h = setup();
    h.slashing.open_dispute(
        &h.reporter,
        &BytesN::from_array(&h.env, &[9u8; 32]),
        &h.feed(),
        &42,
        &String::from_str(&h.env, "ipfs://nothing"),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #22)")] // DuplicateDispute
fn the_same_allegation_cannot_be_filed_twice() {
    let h = setup();
    h.open();
    // Otherwise one offence could be turned into any number of seizures by
    // re-filing after each settlement.
    h.open();
}

#[test]
fn a_different_round_is_a_different_allegation() {
    let h = setup();
    let first = h.open();
    let second = h.slashing.open_dispute(
        &h.reporter,
        &h.accused,
        &h.feed(),
        &43,
        &String::from_str(&h.env, "ipfs://other"),
    );
    assert_ne!(first, second);
    assert_eq!(h.slashing.dispute_for(&h.accused, &h.feed(), &42), Some(1));
    assert_eq!(h.slashing.dispute_for(&h.accused, &h.feed(), &99), None);
}

// -- voting -----------------------------------------------------------------

#[test]
fn a_committee_majority_upholds_a_dispute() {
    let h = setup();
    let id = h.resolved(3, 1);
    let dispute = h.dispute(id);

    assert_eq!(dispute.status, DisputeStatus::Upheld);
    assert_eq!(dispute.votes_for, 3);
    assert_eq!(dispute.votes_against, 1);
}

#[test]
fn a_dispute_nobody_voted_on_is_dismissed_not_upheld() {
    let h = setup();
    let id = h.open();
    h.advance(VOTING_PERIOD + 1);

    assert_eq!(h.slashing.resolve(&id), DisputeStatus::Dismissed);
    // Silence from the committee is not evidence against an operator. A rule
    // that treated it as such would let an attacker slash a node simply by
    // making sure nobody was watching.
}

#[test]
fn a_dispute_short_of_quorum_is_dismissed_even_when_unopposed() {
    let h = setup();
    let id = h.resolved(2, 0);
    assert_eq!(h.dispute(id).status, DisputeStatus::Dismissed);
}

#[test]
fn a_tie_favours_the_accused() {
    let h = setup();
    let id = h.resolved(2, 2);
    assert_eq!(h.dispute(id).status, DisputeStatus::Dismissed);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")] // NotCommitteeMember
fn an_outsider_cannot_vote() {
    let h = setup();
    let id = h.open();
    h.slashing.vote(&Address::generate(&h.env), &id, &true);
}

#[test]
#[should_panic(expected = "Error(Contract, #26)")] // AlreadyVoted
fn a_member_votes_once_per_round() {
    let h = setup();
    let id = h.open();
    h.vote(0, id, true);
    h.vote(0, id, true);
}

#[test]
#[should_panic(expected = "Error(Contract, #27)")] // ConflictOfInterest
fn an_operator_cannot_vote_on_a_dispute_against_their_own_node() {
    let h = setup();
    h.slashing.add_member(&h.accused_owner);
    let id = h.open();
    h.slashing.vote(&h.accused_owner, &id, &false);
}

#[test]
#[should_panic(expected = "Error(Contract, #24)")] // VotingClosed
fn a_vote_after_the_deadline_is_refused() {
    let h = setup();
    let id = h.open();
    h.advance(VOTING_PERIOD + 1);
    h.vote(0, id, true);
}

#[test]
#[should_panic(expected = "Error(Contract, #25)")] // VotingOpen
fn a_dispute_cannot_be_resolved_while_voting_is_open() {
    let h = setup();
    let id = h.open();
    h.vote(0, id, true);
    h.vote(1, id, true);
    h.vote(2, id, true);
    // Even with quorum already reached: an early close would let whoever
    // watches the ledger fastest cut off the remaining members.
    h.slashing.resolve(&id);
}

#[test]
fn how_each_member_voted_is_on_the_record() {
    let h = setup();
    let id = h.open();
    h.vote(0, id, true);
    h.vote(1, id, false);

    assert_eq!(h.slashing.vote_of(&id, &h.committee[0]), Some(true));
    assert_eq!(h.slashing.vote_of(&id, &h.committee[1]), Some(false));
    assert_eq!(h.slashing.vote_of(&id, &h.committee[2]), None);
}

// -- settlement -------------------------------------------------------------

#[test]
fn an_upheld_dispute_seizes_stake_and_pays_the_reporter() {
    let h = setup();
    let reporter_before = h.token.balance(&h.reporter);
    let stake_before = h.registry.get_node(&h.accused).unwrap().stake;
    let rep_before = h.registry.get_node(&h.accused).unwrap().reputation;

    let id = h.resolved(3, 0);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    let node = h.registry.get_node(&h.accused).unwrap();
    assert_eq!(node.stake, stake_before - SLASH_AMOUNT);
    assert_eq!(node.reputation, rep_before - 2_000);
    assert_eq!(node.total_slashed, SLASH_AMOUNT);

    // Bond returned, plus the reward out of the slash pool.
    assert_eq!(
        h.token.balance(&h.reporter),
        reporter_before + REPORTER_REWARD
    );
    assert_eq!(h.registry.slash_pool(), SLASH_AMOUNT - REPORTER_REWARD);
    assert_eq!(h.dispute(id).status, DisputeStatus::Settled);
}

#[test]
fn a_dismissed_dispute_hands_the_bond_to_the_operator_who_answered_it() {
    let h = setup();
    let reporter_before = h.token.balance(&h.reporter);
    let owner_before = h.token.balance(&h.accused_owner);
    let stake_before = h.registry.get_node(&h.accused).unwrap().stake;

    let id = h.resolved(1, 3);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    assert_eq!(h.token.balance(&h.reporter), reporter_before - DISPUTE_BOND);
    assert_eq!(h.token.balance(&h.accused_owner), owner_before + DISPUTE_BOND);
    assert_eq!(
        h.registry.get_node(&h.accused).unwrap().stake,
        stake_before,
        "a dismissed allegation must not cost the node anything"
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #28)")] // AppealWindowOpen
fn stake_does_not_move_until_the_appeal_window_closes() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.slashing.settle(&id);
}

#[test]
#[should_panic(expected = "Error(Contract, #31)")] // AlreadySettled
fn a_dispute_settles_once() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);
    h.slashing.settle(&id);
}

#[test]
fn a_reward_larger_than_the_pool_is_capped_rather_than_failing_the_settlement() {
    let h = setup();
    let mut config = h.slashing.get_config();
    config.reporter_reward = SLASH_AMOUNT * 10;
    h.slashing.set_config(&config);

    let id = h.resolved(3, 0);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    // A correct dispute must not fail to settle because the seizure was
    // smaller than the advertised reward.
    assert_eq!(h.registry.slash_pool(), 0);
    assert_eq!(h.dispute(id).status, DisputeStatus::Settled);
}

// -- appeals ----------------------------------------------------------------

#[test]
fn an_appeal_sends_the_dispute_back_for_a_second_vote() {
    let h = setup();
    let id = h.resolved(3, 0);

    h.token_admin.mint(&h.accused_owner, &APPEAL_BOND);
    h.slashing.appeal(&h.accused_owner, &id);

    let dispute = h.dispute(id);
    assert_eq!(dispute.status, DisputeStatus::Voting);
    assert_eq!(dispute.vote_round, 2);
    assert_eq!(dispute.votes_for, 0, "the first round's votes are cleared");
    assert_eq!(dispute.appellant, Some(h.accused_owner.clone()));

    // The same members vote again; a first-round vote does not carry over.
    h.vote(0, id, false);
    h.vote(1, id, false);
    h.vote(2, id, false);
    h.advance(VOTING_PERIOD + 1);
    assert_eq!(h.slashing.resolve(&id), DisputeStatus::Dismissed);
}

#[test]
fn a_successful_appeal_gets_its_bond_back() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.token_admin.mint(&h.accused_owner, &APPEAL_BOND);
    let owner_before = h.token.balance(&h.accused_owner);

    h.slashing.appeal(&h.accused_owner, &id);
    h.vote(0, id, false);
    h.vote(1, id, false);
    h.vote(2, id, false);
    h.advance(VOTING_PERIOD + 1);
    h.slashing.resolve(&id);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    // Appeal bond returned, and the dispute bond forfeited to the operator who
    // was wrongly accused.
    assert_eq!(
        h.token.balance(&h.accused_owner),
        owner_before + DISPUTE_BOND
    );
    assert_eq!(h.registry.get_node(&h.accused).unwrap().total_slashed, 0);
}

#[test]
fn a_failed_appeal_forfeits_its_bond_to_the_other_side() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.token_admin.mint(&h.accused_owner, &APPEAL_BOND);
    let owner_before = h.token.balance(&h.accused_owner);
    let reporter_before = h.token.balance(&h.reporter);

    h.slashing.appeal(&h.accused_owner, &id);
    h.vote(0, id, true);
    h.vote(1, id, true);
    h.vote(2, id, true);
    h.advance(VOTING_PERIOD + 1);
    h.slashing.resolve(&id);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    assert_eq!(h.token.balance(&h.accused_owner), owner_before - APPEAL_BOND);
    assert_eq!(
        h.token.balance(&h.reporter),
        reporter_before + DISPUTE_BOND + APPEAL_BOND + REPORTER_REWARD,
        "bond back, reward paid, and the failed appeal's bond on top: the \
         reporter is compensated for being dragged through a second vote"
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #30)")] // AlreadyAppealed
fn a_dispute_may_be_appealed_only_once() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.token_admin.mint(&h.accused_owner, &(APPEAL_BOND * 2));
    h.slashing.appeal(&h.accused_owner, &id);

    h.vote(0, id, true);
    h.vote(1, id, true);
    h.vote(2, id, true);
    h.advance(VOTING_PERIOD + 1);
    h.slashing.resolve(&id);

    h.slashing.appeal(&h.accused_owner, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #29)")] // AppealWindowClosed
fn an_appeal_after_the_window_is_refused() {
    let h = setup();
    let id = h.resolved(3, 0);
    h.token_admin.mint(&h.accused_owner, &APPEAL_BOND);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.appeal(&h.accused_owner, &id);
}

// -- committee --------------------------------------------------------------

#[test]
fn the_committee_can_grow_and_shrink() {
    let h = setup();
    let newcomer = Address::generate(&h.env);
    h.slashing.add_member(&newcomer);
    assert_eq!(h.slashing.committee().len(), 6);

    h.slashing.remove_member(&newcomer);
    assert_eq!(h.slashing.committee().len(), 5);
}

#[test]
#[should_panic(expected = "Error(Contract, #12)")] // CommitteeTooSmall
fn a_committee_cannot_shrink_below_the_quorum_it_must_reach() {
    let h = setup();
    // Five members, quorum three. Removing three leaves a committee that
    // dismisses every dispute filed against anybody -- a quiet way to switch
    // slashing off entirely.
    h.slashing.remove_member(&h.committee[0].clone());
    h.slashing.remove_member(&h.committee[1].clone());
    h.slashing.remove_member(&h.committee[2].clone());
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // AlreadyCommitteeMember
fn a_member_cannot_be_added_twice() {
    let h = setup();
    h.slashing.add_member(&h.committee[0].clone());
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn an_appeal_cheaper_than_the_dispute_it_contests_is_refused() {
    let h = setup();
    let mut config = h.slashing.get_config();
    config.appeal_bond = DISPUTE_BOND - 1;
    h.slashing.set_config(&config);
}

#[test]
fn an_unknown_dispute_reads_as_absent() {
    let h = setup();
    assert!(h.slashing.get_dispute(&99).is_none());
    assert_eq!(h.slashing.dispute_count(), 0);
}

#[test]
fn a_disputed_node_that_exits_can_still_be_reached() {
    let h = setup();
    // The unbonding period is what makes this possible: the node stops voting
    // immediately but its stake stays locked long enough for a dispute over
    // its past submissions to land.
    h.registry.request_unbond(&h.accused);
    let stake_before = h.registry.get_node(&h.accused).unwrap().stake;

    let id = h.resolved(3, 0);
    h.advance(APPEAL_PERIOD + 1);
    h.slashing.settle(&id);

    assert_eq!(
        h.registry.get_node(&h.accused).unwrap().stake,
        stake_before - SLASH_AMOUNT
    );
}

#[test]
fn a_dispute_records_where_its_evidence_lives() {
    let h = setup();
    let id = h.open();
    assert_eq!(
        h.dispute(id).evidence,
        String::from_str(&h.env, "ipfs://bafyevidence")
    );
    assert_eq!(h.dispute(id).feed, symbol_short!("BTC_USD"));
}
