use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{symbol_short, Address, BytesN, Env, String, Symbol, Vec};

use crate::{
    Candidate, Config, Dispute, DisputeStatus, ElectionPhase, ElectionStatus, Slashing,
    SlashingClient,
};
use aphelion_registry::{Registry, RegistryClient};

const BASE_TIME: u64 = 1_735_689_600;
const MIN_STAKE: i128 = 10_000_000_000; // 1000 XLM in stroops
const JAIL_PERIOD: u64 = 24 * 3600;
const UNBONDING: u64 = 7 * 24 * 3600;

const DISPUTE_BOND: i128 = 1_000_000_000; // 100 XLM
const APPEAL_BOND: i128 = 3_000_000_000; // 300 XLM
const SLASH_AMOUNT: i128 = 5_000_000_000; // 500 XLM
const REPORTER_REWARD: i128 = 500_000_000; // 50 XLM
const VOTING_PERIOD: u64 = 3 * 24 * 3600;
const APPEAL_PERIOD: u64 = 2 * 24 * 3600;

const SEATS: u32 = 3;
const NOMINATION_PERIOD: u64 = 2 * 24 * 3600;
const ELECTION_PERIOD: u64 = 3 * 24 * 3600;
const TERM_LENGTH: u64 = 90 * 24 * 3600;

/// What a newly registered node is worth: half weight, per the registry's
/// `STARTING_REPUTATION`. Every node in these tests is a newcomer, so weight
/// asymmetry in a ballot comes from owning more nodes rather than from
/// reputation -- which is the property being tested anyway.
const NEWCOMER_WEIGHT: u64 = 5_000;

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
        &JAIL_PERIOD,
    );

    // The accused: a real registered node with real bonded stake.
    let accused_owner = Address::generate(&env);
    token_admin.mint(&accused_owner, &(MIN_STAKE * 4));
    let accused = BytesN::from_array(&env, &[7u8; 32]);
    registry.register(&accused_owner, &accused, &(MIN_STAKE * 2));

    // Five neutral members, plus the accused's own operator. Seats are only
    // filled by an election now, so a member who has to be on the committee
    // for a test has to be there from genesis.
    let committee: std::vec::Vec<Address> = (0..5).map(|_| Address::generate(&env)).collect();
    let mut committee_vec = Vec::new(&env);
    for m in &committee {
        committee_vec.push_back(m.clone());
    }
    committee_vec.push_back(accused_owner.clone());

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
            seats: SEATS,
            nomination_period: NOMINATION_PERIOD,
            election_period: ELECTION_PERIOD,
            term_length: TERM_LENGTH,
        },
        &committee_vec,
    );

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

    /// Bond another node under `owner`. Weight follows nodes, so "how much is
    /// this participant worth in an election" is "how many of these do they
    /// have".
    fn node(&self, owner: &Address, seed: u8) -> BytesN<32> {
        self.token_admin.mint(owner, &MIN_STAKE);
        let pubkey = BytesN::from_array(&self.env, &[seed; 32]);
        self.registry.register(owner, &pubkey, &MIN_STAKE);
        pubkey
    }

    /// A fresh operator with one node.
    fn operator(&self, seed: u8) -> (Address, BytesN<32>) {
        let owner = Address::generate(&self.env);
        let node = self.node(&owner, seed);
        (owner, node)
    }

    /// Put a node below the jail threshold, from the aggregator's seat: what
    /// happens to a node that submits a price outside the consensus band.
    fn jail(&self, node: &BytesN<32>) {
        self.registry.penalize(node, &2_500, &0);
        assert_eq!(self.registry.weight_of(node), 0);
    }

    /// Serve the sitting committee's term and open an election.
    fn open_election(&self) -> u64 {
        self.advance(TERM_LENGTH);
        self.slashing.open_election()
    }

    fn tally(&self, id: u64, candidate: &Address) -> u64 {
        self.slashing
            .candidates(&id)
            .iter()
            .find(|c: &Candidate| &c.address == candidate)
            .map(|c| c.weight)
            .unwrap_or(0)
    }

    fn seated(&self) -> std::vec::Vec<Address> {
        self.slashing.committee().iter().collect()
    }

    /// Run an election that seats three fresh operators, each voting for
    /// themselves, and return them in the order they stood.
    fn elect_three(&self) -> (Address, Address, Address) {
        let (alice, a) = self.operator(20);
        let (bob, b) = self.operator(21);
        let (carol, c) = self.operator(22);

        self.open_election();
        self.slashing.nominate(&alice, &a);
        self.slashing.nominate(&bob, &b);
        self.slashing.nominate(&carol, &c);

        self.advance(NOMINATION_PERIOD);
        self.slashing.cast_ballot(&alice, &a, &alice);
        self.slashing.cast_ballot(&bob, &b, &bob);
        self.slashing.cast_ballot(&carol, &c, &carol);

        self.advance(ELECTION_PERIOD);
        assert_eq!(self.slashing.finalize_election(), ElectionStatus::Seated);
        (alice, bob, carol)
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
    // The accused's operator sits on the committee from genesis, which is the
    // only way onto it short of an election -- and exactly the situation the
    // rule exists for.
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
    assert_eq!(
        h.token.balance(&h.accused_owner),
        owner_before + DISPUTE_BOND
    );
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

    assert_eq!(
        h.token.balance(&h.accused_owner),
        owner_before - APPEAL_BOND
    );
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
fn admin_can_empty_a_seat_and_cannot_fill_one() {
    let h = setup();
    assert_eq!(h.slashing.committee().len(), 6);

    h.slashing.remove_member(&h.committee[0].clone());
    assert_eq!(h.slashing.committee().len(), 5);
    assert!(!h.seated().contains(&h.committee[0]));

    // There is no `add_member` to put them back with. The seat stays empty
    // until an election fills it, which is the whole of the asymmetry: the
    // timelock can subtract from the committee and can never install one.
    h.slashing.remove_member(&h.committee[1].clone());
    assert_eq!(h.slashing.committee().len(), 4);
}

#[test]
#[should_panic(expected = "Error(Contract, #12)")] // CommitteeTooSmall
fn a_committee_cannot_shrink_below_the_quorum_it_must_reach() {
    let h = setup();
    // Six members, quorum three. Removing four leaves a committee that
    // dismisses every dispute filed against anybody -- a quiet way to switch
    // slashing off entirely.
    h.slashing.remove_member(&h.committee[0].clone());
    h.slashing.remove_member(&h.committee[1].clone());
    h.slashing.remove_member(&h.committee[2].clone());
    h.slashing.remove_member(&h.committee[3].clone());
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // AlreadyCommitteeMember
fn a_genesis_committee_with_the_same_member_twice_is_refused() {
    let h = setup();
    // [A, A, B] with a quorum of three passes a length check and can never
    // resolve anything: the duplicate counts twice towards quorum and votes
    // once. Refused at the only point it can be, since nothing can add a
    // member afterwards.
    let fresh = env_with(&h);
    let member = Address::generate(&h.env);
    let mut committee = Vec::new(&h.env);
    committee.push_back(member.clone());
    committee.push_back(member);
    committee.push_back(Address::generate(&h.env));
    fresh.initialize(&h.slashing.get_config(), &committee);
}

/// A second, uninitialised slashing contract in the same environment, for the
/// tests that are about `initialize` itself.
fn env_with<'a>(h: &Harness<'a>) -> SlashingClient<'a> {
    SlashingClient::new(&h.env, &h.env.register(Slashing, ()))
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn a_committee_that_could_not_reach_quorum_at_full_strength_is_refused() {
    let h = setup();
    let mut config = h.slashing.get_config();
    // Three seats, quorum four: every election would seat a committee that
    // dismisses every dispute filed against anybody.
    config.quorum = SEATS + 1;
    h.slashing.set_config(&config);
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

// -- elections --------------------------------------------------------------

#[test]
fn the_appointed_committee_serves_a_term_like_any_elected_one() {
    let h = setup();
    // There is no way to avoid appointing the first committee -- an election
    // needs an electorate, and at genesis there are no nodes. What it does not
    // get is a longer tenure than one it won.
    assert_eq!(h.slashing.next_election(), BASE_TIME + TERM_LENGTH);
    assert!(h.slashing.current_election().is_none());
    assert_eq!(h.slashing.election_count(), 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #42)")] // TermNotServed
fn an_election_cannot_be_opened_before_the_term_is_served() {
    let h = setup();
    h.advance(TERM_LENGTH - 1);
    h.slashing.open_election();
}

#[test]
fn an_election_walks_from_nominating_through_balloting_to_a_count() {
    let h = setup();
    let id = h.open_election();
    assert_eq!(h.slashing.current_election(), Some(id));

    assert_eq!(h.slashing.election_phase(&id), ElectionPhase::Nominating);
    h.advance(NOMINATION_PERIOD);
    assert_eq!(h.slashing.election_phase(&id), ElectionPhase::Balloting);
    h.advance(ELECTION_PERIOD);
    assert_eq!(h.slashing.election_phase(&id), ElectionPhase::Counting);

    // Nobody stood, so nobody is seated -- but the phase still walks to a
    // recorded end rather than leaving an election open forever.
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Failed);
    assert_eq!(h.slashing.election_phase(&id), ElectionPhase::Failed);
    assert!(h.slashing.current_election().is_none());
}

#[test]
fn an_election_seats_the_candidates_with_the_most_weight_behind_them() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let (carol, c) = h.operator(12);
    let (dave, d) = h.operator(13);
    // Two operators who vote without standing.
    let (erin, e) = h.operator(14);
    let (frank, f) = h.operator(15);

    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);
    h.slashing.nominate(&carol, &c);
    h.slashing.nominate(&dave, &d);

    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.slashing.cast_ballot(&erin, &e, &alice);
    h.slashing.cast_ballot(&frank, &f, &alice);
    h.slashing.cast_ballot(&bob, &b, &bob);
    h.slashing.cast_ballot(&h.accused_owner, &h.accused, &bob);
    h.slashing.cast_ballot(&carol, &c, &carol);
    h.slashing.cast_ballot(&dave, &d, &dave);

    assert_eq!(h.tally(id, &alice), NEWCOMER_WEIGHT * 3);
    assert_eq!(h.tally(id, &bob), NEWCOMER_WEIGHT * 2);
    assert_eq!(h.tally(id, &carol), NEWCOMER_WEIGHT);
    assert_eq!(h.tally(id, &dave), NEWCOMER_WEIGHT);

    h.advance(ELECTION_PERIOD);
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Seated);

    let seated = h.seated();
    assert_eq!(seated.len(), SEATS as usize);
    assert!(seated.contains(&alice));
    assert!(seated.contains(&bob));
    // Carol and Dave drew the same weight and there was one seat left. It goes
    // to whoever stood first, which is a rule nobody can compute their way
    // around after the ballot has closed.
    assert!(seated.contains(&carol));
    assert!(!seated.contains(&dave));

    let election = h.slashing.get_election(&id).unwrap();
    assert_eq!(election.seated, SEATS);
    assert_eq!(election.ballots, 7);
    assert_eq!(election.turnout, NEWCOMER_WEIGHT * 7);
}

#[test]
fn the_committee_that_votes_on_disputes_is_the_one_the_election_seated() {
    let h = setup();
    let displaced = h.committee[0].clone();
    let (alice, _, _) = h.elect_three();

    assert_eq!(h.seated().len(), SEATS as usize);
    assert!(!h.seated().contains(&displaced));

    let id = h.open();
    h.slashing.vote(&alice, &id, &true);
    assert_eq!(h.slashing.vote_of(&id, &alice), Some(true));
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")] // NotCommitteeMember
fn a_member_who_lost_their_seat_stops_voting() {
    let h = setup();
    let displaced = h.committee[0].clone();
    h.elect_three();
    let id = h.open();
    h.slashing.vote(&displaced, &id, &true);
}

#[test]
fn a_seated_committee_starts_a_fresh_term() {
    let h = setup();
    h.elect_three();
    let now = h.env.ledger().timestamp();
    assert_eq!(h.slashing.next_election(), now + TERM_LENGTH);
}

#[test]
#[should_panic(expected = "Error(Contract, #40)")] // ElectionRunning
fn only_one_election_runs_at_a_time() {
    let h = setup();
    h.open_election();
    // Even once another term's worth of time has passed: the running one has
    // to be counted before the next is opened, or a ballot could be cast into
    // whichever of two open elections suited the voter.
    h.advance(TERM_LENGTH);
    h.slashing.open_election();
}

// -- who may stand, and on what --------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #47)")] // NotEligible
fn a_candidate_must_stand_on_a_node_they_own() {
    let h = setup();
    let (_, node) = h.operator(10);
    let outsider = Address::generate(&h.env);
    h.open_election();
    // Anyone can make an address. A node carrying weight costs a bond under a
    // key with a history, and that is the whole of the entry price.
    h.slashing.nominate(&outsider, &node);
}

#[test]
#[should_panic(expected = "Error(Contract, #47)")] // NotEligible
fn a_jailed_operator_cannot_stand() {
    let h = setup();
    let (alice, a) = h.operator(10);
    h.jail(&a);
    h.open_election();
    h.slashing.nominate(&alice, &a);
}

#[test]
#[should_panic(expected = "Error(Contract, #44)")] // AlreadyNominated
fn standing_twice_in_one_election_is_refused() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let second = h.node(&alice, 11);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    // A second node buys a second *ballot*, never a second candidacy: seats
    // are held by people.
    h.slashing.nominate(&alice, &second);
}

#[test]
#[should_panic(expected = "Error(Contract, #43)")] // WrongElectionPhase
fn nominations_close_when_the_ballot_opens() {
    let h = setup();
    let (alice, a) = h.operator(10);
    h.open_election();
    h.advance(NOMINATION_PERIOD);
    h.slashing.nominate(&alice, &a);
}

// -- ballots ----------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #43)")] // WrongElectionPhase
fn a_ballot_cannot_be_cast_while_nominations_are_still_open() {
    let h = setup();
    let (alice, a) = h.operator(10);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.cast_ballot(&alice, &a, &alice);
}

#[test]
fn an_operator_votes_once_for_every_node_they_own() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let b2 = h.node(&bob, 12);
    let b3 = h.node(&bob, 13);

    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);

    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&bob, &b, &bob);
    h.slashing.cast_ballot(&bob, &b2, &bob);
    h.slashing.cast_ballot(&bob, &b3, &bob);
    h.slashing.cast_ballot(&alice, &a, &alice);

    // Weight follows nodes, not people. Three bonds is three votes -- and
    // three times the stake at risk if any of them misbehaves.
    assert_eq!(h.tally(id, &bob), NEWCOMER_WEIGHT * 3);
    assert_eq!(h.tally(id, &alice), NEWCOMER_WEIGHT);
}

#[test]
#[should_panic(expected = "Error(Contract, #46)")] // AlreadyBalloted
fn a_node_casts_one_ballot() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.slashing.cast_ballot(&alice, &a, &bob);
}

#[test]
#[should_panic(expected = "Error(Contract, #47)")] // NotEligible
fn a_node_that_is_not_yours_does_not_vote_for_you() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (_, b) = h.operator(11);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &b, &alice);
}

#[test]
#[should_panic(expected = "Error(Contract, #47)")] // NotEligible
fn a_jailed_node_carries_no_vote() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.jail(&b);
    h.slashing.cast_ballot(&bob, &b, &alice);
}

#[test]
#[should_panic(expected = "Error(Contract, #45)")] // NotCandidate
fn a_ballot_for_somebody_who_did_not_stand_is_refused() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let bystander = Address::generate(&h.env);
    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &bystander);
}

#[test]
fn how_each_node_voted_is_on_the_record() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&bob, &b, &alice);

    assert_eq!(h.slashing.ballot_of(&id, &b), Some(alice));
    assert_eq!(h.slashing.ballot_of(&id, &a), None);
}

#[test]
fn a_ballot_is_worth_what_the_node_was_worth_when_it_was_cast() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&bob, &b, &alice);
    assert_eq!(h.tally(id, &alice), NEWCOMER_WEIGHT);

    // Bob's node is jailed after his ballot is in the box. Re-reading weight
    // at the count would let reputation moving between the two silently
    // re-weight a vote already cast -- the same capture the aggregator makes
    // when a price submission arrives.
    h.jail(&b);
    assert_eq!(h.tally(id, &alice), NEWCOMER_WEIGHT);

    h.advance(ELECTION_PERIOD);
    assert_eq!(
        h.slashing.get_election(&id).unwrap().turnout,
        NEWCOMER_WEIGHT
    );
}

// -- counting ---------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #43)")] // WrongElectionPhase
fn an_election_cannot_be_counted_before_its_ballot_closes() {
    let h = setup();
    h.open_election();
    h.advance(NOMINATION_PERIOD + ELECTION_PERIOD - 1);
    h.slashing.finalize_election();
}

#[test]
#[should_panic(expected = "Error(Contract, #41)")] // UnknownElection
fn an_election_is_counted_once() {
    let h = setup();
    h.open_election();
    h.advance(NOMINATION_PERIOD + ELECTION_PERIOD);
    h.slashing.finalize_election();
    // Counting stops being possible because nothing is running any more, not
    // because a flag says so.
    h.slashing.finalize_election();
}

#[test]
fn an_election_that_cannot_fill_its_quorum_leaves_the_incumbents_in_place() {
    let h = setup();
    let incumbents = h.seated();
    let (alice, a) = h.operator(10);

    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.advance(ELECTION_PERIOD);

    // One winner against a quorum of three. Seating them would leave a
    // committee that dismisses every dispute filed against anybody, so an
    // attacker who can suppress turnout must not thereby switch slashing off.
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Failed);
    assert_eq!(h.seated(), incumbents);
    assert_eq!(h.slashing.get_election(&id).unwrap().seated, 0);
}

#[test]
fn a_failed_election_can_be_retried_at_once() {
    let h = setup();
    h.open_election();
    h.advance(NOMINATION_PERIOD + ELECTION_PERIOD);
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Failed);

    // It cost a nomination period and a ballot to fail, so there is nothing to
    // spam with -- and making the network wait out a full term for a committee
    // it never managed to elect would penalise the wrong party.
    assert_eq!(h.slashing.next_election(), h.env.ledger().timestamp());
    assert_eq!(h.slashing.open_election(), 2);
}

#[test]
fn a_candidacy_nobody_voted_for_is_not_a_mandate() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let (carol, c) = h.operator(12);

    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);
    h.slashing.nominate(&carol, &c);

    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.slashing.cast_ballot(&bob, &b, &bob);
    h.advance(ELECTION_PERIOD);

    // Three seats and exactly three candidates, but Carol drew nothing. An
    // unopposed slate does not take the committee on zero turnout, so the
    // election falls one short of its quorum and seats nobody.
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Failed);
}

#[test]
fn a_candidate_jailed_during_the_ballot_does_not_take_a_seat() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let (carol, c) = h.operator(12);
    let (dave, d) = h.operator(13);

    h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);
    h.slashing.nominate(&carol, &c);
    h.slashing.nominate(&dave, &d);

    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.slashing.cast_ballot(&h.accused_owner, &h.accused, &alice);
    h.slashing.cast_ballot(&bob, &b, &bob);
    h.slashing.cast_ballot(&carol, &c, &carol);
    h.slashing.cast_ballot(&dave, &d, &dave);
    h.advance(ELECTION_PERIOD);

    // Alice led the ballot and was jailed before it was counted. Eligibility
    // is rechecked at the count rather than trusted from the nomination: the
    // seat goes to Dave, who would otherwise have missed out on the tie-break.
    h.jail(&a);
    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Seated);

    let seated = h.seated();
    assert_eq!(seated.len(), SEATS as usize);
    assert!(!seated.contains(&alice));
    assert!(seated.contains(&bob));
    assert!(seated.contains(&carol));
    assert!(seated.contains(&dave));
}

#[test]
fn a_config_change_does_not_move_the_bar_under_a_running_election() {
    let h = setup();
    let (alice, a) = h.operator(10);
    let (bob, b) = h.operator(11);
    let (carol, c) = h.operator(12);

    let id = h.open_election();
    h.slashing.nominate(&alice, &a);
    h.slashing.nominate(&bob, &b);
    h.slashing.nominate(&carol, &c);

    // Governance widens the committee mid-ballot. The election was opened
    // against three seats and a quorum of three, and that is what it counts
    // against -- the same capture a governance proposal makes of its own eta.
    let mut config = h.slashing.get_config();
    config.seats = 5;
    config.quorum = 5;
    h.slashing.set_config(&config);

    let election = h.slashing.get_election(&id).unwrap();
    assert_eq!(election.seats, SEATS);
    assert_eq!(election.quorum, 3);

    h.advance(NOMINATION_PERIOD);
    h.slashing.cast_ballot(&alice, &a, &alice);
    h.slashing.cast_ballot(&bob, &b, &bob);
    h.slashing.cast_ballot(&carol, &c, &carol);
    h.advance(ELECTION_PERIOD);

    assert_eq!(h.slashing.finalize_election(), ElectionStatus::Seated);
    assert_eq!(h.seated().len(), 3);
}

#[test]
fn an_unknown_election_reads_as_absent() {
    let h = setup();
    assert!(h.slashing.get_election(&99).is_none());
    assert!(h.slashing.candidates(&99).is_empty());
    assert!(h.slashing.current_election().is_none());
}
