use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{vec, Address, Env, IntoVal, String, Symbol, Val, Vec};

use crate::{
    Config, Governance, GovernanceClient, Proposal, ProposalState, ProposalStatus, MAX_DELAY,
    MIN_DELAY,
};
use aphelion_registry::{Registry, RegistryClient};

const BASE_TIME: u64 = 1_735_689_600;
const DELAY: u64 = 3 * 24 * 3600;
const GRACE: u64 = 7 * 24 * 3600;

const MIN_STAKE: i128 = 10_000_000_000; // 1000 XLM in stroops
const JAIL_PERIOD: u64 = 24 * 3600;
const UNBONDING: u64 = 7 * 24 * 3600;

struct Harness<'a> {
    env: Env,
    gov: GovernanceClient<'a>,
    registry: RegistryClient<'a>,
    guardian: Address,
    proposers: std::vec::Vec<Address>,
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(BASE_TIME);

    let deployer = Address::generate(&env);
    let aggregator = Address::generate(&env);
    let slasher = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(deployer.clone());
    let _ = StellarAssetClient::new(&env, &sac.address());

    let registry_id = env.register(Registry, ());
    let registry = RegistryClient::new(&env, &registry_id);
    registry.initialize(
        &deployer,
        &aggregator,
        &slasher,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &JAIL_PERIOD,
    );

    let guardian = Address::generate(&env);
    let proposers: std::vec::Vec<Address> = (0..2).map(|_| Address::generate(&env)).collect();
    let mut proposer_vec = Vec::new(&env);
    for p in &proposers {
        proposer_vec.push_back(p.clone());
    }

    let gov_id = env.register(Governance, ());
    let gov = GovernanceClient::new(&env, &gov_id);
    gov.initialize(
        &Config {
            guardian: guardian.clone(),
            delay: DELAY,
            grace_period: GRACE,
        },
        &proposer_vec,
    );

    // The point of the exercise: from here the registry has no admin key of
    // its own, only a timelock.
    registry.set_admin(&gov_id);

    Harness {
        env,
        gov,
        registry,
        guardian,
        proposers,
    }
}

impl Harness<'_> {
    fn description(&self) -> String {
        String::from_str(&self.env, "ipfs://bafyrationale")
    }

    /// Queue a raise of the registry's minimum stake — a real privileged call
    /// on a real contract that has named this timelock as its admin.
    fn propose_min_stake(&self, min_stake: i128) -> u64 {
        self.propose_as(
            0,
            &self.registry.address,
            "set_min_stake",
            vec![&self.env, min_stake.into_val(&self.env)],
        )
    }

    fn propose_as(&self, proposer: usize, target: &Address, function: &str, args: Vec<Val>) -> u64 {
        self.gov.propose(
            &self.proposers[proposer],
            target,
            &Symbol::new(&self.env, function),
            &args,
            &self.description(),
        )
    }

    /// Queue a change to the timelock's own configuration.
    fn propose_config(&self, config: &Config) -> u64 {
        self.propose_as(
            0,
            &self.gov.address,
            "set_config",
            vec![&self.env, config.into_val(&self.env)],
        )
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn proposal(&self, id: u64) -> Proposal {
        self.gov.get_proposal(&id).expect("proposal exists")
    }
}

// -- setup ------------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #1)")] // AlreadyInitialized
fn initialising_twice_is_refused() {
    let h = setup();
    h.gov.initialize(
        &Config {
            guardian: h.guardian.clone(),
            delay: DELAY,
            grace_period: GRACE,
        },
        &vec![&h.env, h.proposers[0].clone()],
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn a_delay_below_the_floor_is_refused_at_the_point_it_is_set() {
    let env = Env::default();
    env.mock_all_auths();
    let gov = GovernanceClient::new(&env, &env.register(Governance, ()));
    gov.initialize(
        &Config {
            guardian: Address::generate(&env),
            delay: MIN_DELAY - 1,
            grace_period: GRACE,
        },
        &vec![&env, Address::generate(&env)],
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn a_delay_long_enough_to_disable_governance_is_refused() {
    let env = Env::default();
    env.mock_all_auths();
    let gov = GovernanceClient::new(&env, &env.register(Governance, ()));
    gov.initialize(
        &Config {
            guardian: Address::generate(&env),
            delay: MAX_DELAY + 1,
            grace_period: GRACE,
        },
        &vec![&env, Address::generate(&env)],
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #12)")] // NoProposersLeft
fn a_timelock_with_nobody_able_to_propose_is_refused() {
    let env = Env::default();
    env.mock_all_auths();
    let gov = GovernanceClient::new(&env, &env.register(Governance, ()));
    gov.initialize(
        &Config {
            guardian: Address::generate(&env),
            delay: DELAY,
            grace_period: GRACE,
        },
        &Vec::new(&env),
    );
}

// -- queueing ---------------------------------------------------------------

#[test]
fn a_queued_proposal_publishes_the_call_and_the_clock_it_runs_on() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);

    assert_eq!(id, 1);
    assert_eq!(h.gov.proposal_count(), 1);

    let p = h.proposal(id);
    assert_eq!(p.target, h.registry.address);
    assert_eq!(p.function, Symbol::new(&h.env, "set_min_stake"));
    assert_eq!(p.args, vec![&h.env, (MIN_STAKE * 2).into_val(&h.env)]);
    assert_eq!(p.proposed_at, BASE_TIME);
    assert_eq!(p.eta, BASE_TIME + DELAY);
    assert_eq!(p.expires_at, BASE_TIME + DELAY + GRACE);
    assert_eq!(p.status, ProposalStatus::Queued);
    assert_eq!(h.gov.state(&id), ProposalState::Waiting);

    // And nothing has happened to the contract it names.
    assert_eq!(h.registry.get_config().min_stake, MIN_STAKE);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")] // NotProposer
fn an_outsider_cannot_queue_a_proposal() {
    let h = setup();
    h.gov.propose(
        &Address::generate(&h.env),
        &h.registry.address,
        &Symbol::new(&h.env, "set_min_stake"),
        &vec![&h.env, (MIN_STAKE * 2).into_val(&h.env)],
        &h.description(),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")] // NotProposer
fn the_guardian_cannot_queue_a_proposal() {
    let h = setup();
    // The whole value of the guardian key is that it can only ever say no.
    h.gov.propose(
        &h.guardian,
        &h.registry.address,
        &Symbol::new(&h.env, "set_min_stake"),
        &vec![&h.env, (MIN_STAKE * 2).into_val(&h.env)],
        &h.description(),
    );
}

// -- execution --------------------------------------------------------------

#[test]
fn an_executed_proposal_changes_the_contract_it_targets() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);

    h.advance(DELAY);
    assert_eq!(h.gov.state(&id), ProposalState::Ready);
    h.gov.execute(&id);

    assert_eq!(h.registry.get_config().min_stake, MIN_STAKE * 2);
    assert_eq!(h.gov.state(&id), ProposalState::Executed);
    assert_eq!(h.proposal(id).executed_at, BASE_TIME + DELAY);
}

#[test]
#[should_panic(expected = "Error(Contract, #22)")] // StillWaiting
fn a_proposal_cannot_execute_before_its_delay_is_served() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.advance(DELAY - 1);
    h.gov.execute(&id);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // WrongPhase
fn a_proposal_executes_once() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.advance(DELAY);
    h.gov.execute(&id);
    h.gov.execute(&id);
}

#[test]
#[should_panic(expected = "Error(Contract, #23)")] // Expired
fn a_proposal_nobody_executed_in_time_is_dead() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.advance(DELAY + GRACE + 1);
    assert_eq!(h.gov.state(&id), ProposalState::Expired);
    h.gov.execute(&id);
}

#[test]
fn an_expired_proposal_is_not_revived_by_a_longer_grace_period() {
    let h = setup();
    let victim = h.propose_min_stake(MIN_STAKE * 2);

    // Widen the grace period as far as it will go, through the timelock.
    let widened = h.propose_config(&Config {
        guardian: h.guardian.clone(),
        delay: DELAY,
        grace_period: 30 * 24 * 3600,
    });
    h.advance(DELAY);
    h.gov.execute(&widened);

    // The first proposal's window was fixed when it was queued, so it still
    // closes when it always would have. Otherwise a proposer could park a
    // stale call and re-open it later by editing the config around it.
    h.advance(GRACE + 1);
    assert_eq!(h.gov.state(&victim), ProposalState::Expired);
    assert!(h.gov.try_execute(&victim).is_err());
}

#[test]
fn a_delay_change_does_not_shorten_the_wait_of_a_proposal_already_queued() {
    let h = setup();
    let early = h.propose_min_stake(MIN_STAKE * 2);
    let shorten = h.propose_config(&Config {
        guardian: h.guardian.clone(),
        delay: MIN_DELAY,
        grace_period: GRACE,
    });

    h.advance(DELAY);
    h.gov.execute(&shorten);
    assert_eq!(h.gov.get_config().delay, MIN_DELAY);

    // `early` was queued under the old delay and has served it, so it runs;
    // what matters is the next one, which gets the new delay and not the old.
    h.gov.execute(&early);
    let later = h.propose_min_stake(MIN_STAKE * 3);
    assert_eq!(
        h.proposal(later).eta,
        h.env.ledger().timestamp() + MIN_DELAY
    );
}

#[test]
fn an_execution_that_fails_leaves_the_proposal_where_it_was() {
    let h = setup();
    // The registry refuses a non-positive minimum stake, so this is a
    // well-formed proposal that fails on arrival.
    let id = h.propose_min_stake(0);
    h.advance(DELAY);

    assert!(h.gov.try_execute(&id).is_err());

    // `execute` marks the proposal spent before making the call, but the whole
    // transaction reverts with the call, so the record still matches what
    // happened: nothing. It can be retried, or cancelled, until it expires.
    assert_eq!(h.gov.state(&id), ProposalState::Ready);
    assert_eq!(h.proposal(id).executed_at, 0);
}

// -- cancellation -----------------------------------------------------------

#[test]
fn the_guardian_can_veto_a_queued_proposal() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);

    h.gov.cancel(&h.guardian, &id);

    assert_eq!(h.gov.state(&id), ProposalState::Cancelled);
    h.advance(DELAY);
    assert!(h.gov.try_execute(&id).is_err());
    assert_eq!(h.registry.get_config().min_stake, MIN_STAKE);
}

#[test]
fn a_proposer_can_withdraw_their_own_proposal() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.gov.cancel(&h.proposers[0], &id);
    assert_eq!(h.gov.state(&id), ProposalState::Cancelled);
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")] // NotCancellable
fn one_proposer_cannot_withdraw_anothers_proposal() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.gov.cancel(&h.proposers[1], &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")] // NotCancellable
fn an_outsider_cannot_cancel() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.gov.cancel(&Address::generate(&h.env), &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // WrongPhase
fn a_cancelled_proposal_cannot_be_revived_by_cancelling_it_again() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.gov.cancel(&h.guardian, &id);
    h.gov.cancel(&h.guardian, &id);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // WrongPhase
fn an_executed_proposal_cannot_be_cancelled_after_the_fact() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);
    h.advance(DELAY);
    h.gov.execute(&id);
    h.gov.cancel(&h.guardian, &id);
}

// -- self-governance --------------------------------------------------------

#[test]
fn changing_the_delay_takes_the_delay() {
    let h = setup();
    let id = h.propose_config(&Config {
        guardian: h.guardian.clone(),
        delay: MIN_DELAY,
        grace_period: GRACE,
    });

    // There is no direct route to compare this against: the contract has no
    // public `set_config`, so a governance change has nowhere to arrive from
    // except a proposal that has served its wait.
    assert!(h.gov.try_execute(&id).is_err());
    assert_eq!(h.gov.get_config().delay, DELAY);

    h.advance(DELAY);
    h.gov.execute(&id);
    assert_eq!(h.gov.get_config().delay, MIN_DELAY);
}

#[test]
fn a_guardian_is_replaced_by_proposal_like_anything_else() {
    let h = setup();
    let successor = Address::generate(&h.env);
    let id = h.propose_config(&Config {
        guardian: successor.clone(),
        delay: DELAY,
        grace_period: GRACE,
    });

    h.advance(DELAY);
    h.gov.execute(&id);

    assert_eq!(h.gov.get_config().guardian, successor);
    // The old guardian's veto goes with the role, and not before.
    let next = h.propose_min_stake(MIN_STAKE * 2);
    assert!(h.gov.try_cancel(&h.guardian, &next).is_err());
    h.gov.cancel(&successor, &next);
    assert_eq!(h.gov.state(&next), ProposalState::Cancelled);
}

#[test]
#[should_panic(expected = "Error(Contract, #24)")] // UnknownAction
fn a_proposal_asking_this_contract_for_something_it_cannot_do_is_refused_when_it_is_queued() {
    let h = setup();
    // Not in three days' time, when the delay has been spent on a call that
    // was never going to land.
    h.propose_as(
        0,
        &h.gov.address,
        "set_delay",
        vec![&h.env, MIN_DELAY.into_val(&h.env)],
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #25)")] // InvalidArguments
fn a_governance_action_carrying_the_wrong_argument_is_refused_when_it_is_queued() {
    let h = setup();
    h.propose_as(
        0,
        &h.gov.address,
        "add_proposer",
        vec![&h.env, 42i128.into_val(&h.env)],
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn a_proposal_cannot_queue_a_delay_it_could_not_have_been_initialised_with() {
    let h = setup();
    h.propose_config(&Config {
        guardian: h.guardian.clone(),
        delay: MIN_DELAY - 1,
        grace_period: GRACE,
    });
}

#[test]
fn a_proposer_is_added_by_proposal_and_can_then_propose() {
    let h = setup();
    let newcomer = Address::generate(&h.env);
    let id = h.propose_as(
        0,
        &h.gov.address,
        "add_proposer",
        vec![&h.env, newcomer.clone().into_val(&h.env)],
    );

    h.advance(DELAY);
    h.gov.execute(&id);

    assert_eq!(h.gov.proposers().len(), 3);
    h.gov.propose(
        &newcomer,
        &h.registry.address,
        &Symbol::new(&h.env, "set_min_stake"),
        &vec![&h.env, (MIN_STAKE * 2).into_val(&h.env)],
        &h.description(),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #12)")] // NoProposersLeft
fn the_last_proposer_cannot_be_removed() {
    let h = setup();
    let first = h.propose_as(
        0,
        &h.gov.address,
        "remove_proposer",
        vec![&h.env, h.proposers[1].clone().into_val(&h.env)],
    );
    let second = h.propose_as(
        0,
        &h.gov.address,
        "remove_proposer",
        vec![&h.env, h.proposers[0].clone().into_val(&h.env)],
    );

    h.advance(DELAY);
    h.gov.execute(&first);
    assert_eq!(h.gov.proposers().len(), 1);

    // Emptying the set would freeze every parameter of the network for good:
    // refilling it is itself a proposal, and there would be nobody to make one.
    h.gov.execute(&second);
}

#[test]
fn a_proposal_can_hand_the_administered_contract_to_a_different_timelock() {
    let h = setup();
    let successor = Address::generate(&h.env);
    let id = h.propose_as(
        0,
        &h.registry.address,
        "set_admin",
        vec![&h.env, successor.clone().into_val(&h.env)],
    );

    h.advance(DELAY);
    h.gov.execute(&id);

    // A migration path that is itself delayed and published, rather than a
    // key handover nobody sees until it has happened.
    assert_eq!(h.registry.get_config().admin, successor);
}

// -- reads ------------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #20)")] // UnknownProposal
fn a_proposal_that_was_never_made_is_not_a_default_one() {
    let h = setup();
    h.gov.state(&99);
}

#[test]
fn state_walks_from_waiting_through_ready_to_expired() {
    let h = setup();
    let id = h.propose_min_stake(MIN_STAKE * 2);

    assert_eq!(h.gov.state(&id), ProposalState::Waiting);
    h.advance(DELAY);
    assert_eq!(h.gov.state(&id), ProposalState::Ready);
    h.advance(GRACE);
    assert_eq!(h.gov.state(&id), ProposalState::Ready);
    h.advance(1);
    assert_eq!(h.gov.state(&id), ProposalState::Expired);
}
