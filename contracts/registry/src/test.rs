use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, BytesN, Env};

use crate::{NodeStatus, Registry, RegistryClient, JAIL_THRESHOLD, STARTING_REPUTATION};

const MIN_STAKE: i128 = 10_000_000_000; // 1000 XLM in stroops
const UNBONDING: u64 = 7 * 24 * 3600;
const JAIL_PERIOD: u64 = 24 * 3600;

struct Harness<'a> {
    env: Env,
    registry: RegistryClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    admin: Address,
    aggregator: Address,
    slasher: Address,
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let aggregator = Address::generate(&env);
    let slasher = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let registry_id = env.register(Registry, ());
    let registry = RegistryClient::new(&env, &registry_id);
    registry.initialize(
        &admin,
        &aggregator,
        &slasher,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &JAIL_PERIOD,
    );

    Harness {
        env,
        registry,
        token,
        token_admin,
        admin,
        aggregator,
        slasher,
    }
}

impl Harness<'_> {
    fn funded_owner(&self, amount: i128) -> Address {
        let owner = Address::generate(&self.env);
        self.token_admin.mint(&owner, &amount);
        owner
    }

    fn pubkey(&self, byte: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[byte; 32])
    }

    fn register_node(&self, byte: u8) -> (Address, BytesN<32>) {
        let owner = self.funded_owner(MIN_STAKE * 10);
        let pubkey = self.pubkey(byte);
        self.registry.register(&owner, &pubkey, &MIN_STAKE);
        (owner, pubkey)
    }
}

#[test]
fn a_registered_node_starts_at_half_weight() {
    let h = setup();
    let (_, pubkey) = h.register_node(1);

    let node = h.registry.get_node(&pubkey).expect("registered");
    assert_eq!(node.reputation, STARTING_REPUTATION);
    assert_eq!(node.status, NodeStatus::Active);
    assert_eq!(
        node.weight_bps, 5_000,
        "a brand new operator must not carry the same weight as a proven one"
    );
}

#[test]
fn stake_actually_moves_to_the_contract() {
    let h = setup();
    let owner = h.funded_owner(MIN_STAKE * 2);
    let pubkey = h.pubkey(2);
    let before = h.token.balance(&owner);

    h.registry.register(&owner, &pubkey, &MIN_STAKE);

    assert_eq!(h.token.balance(&owner), before - MIN_STAKE);
    assert_eq!(h.token.balance(&h.registry.address), MIN_STAKE);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")] // StakeTooLow
fn rejects_a_bond_below_the_minimum() {
    let h = setup();
    let owner = h.funded_owner(MIN_STAKE);
    h.registry.register(&owner, &h.pubkey(3), &(MIN_STAKE - 1));
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")] // NodeAlreadyRegistered
fn rejects_a_duplicate_public_key() {
    let h = setup();
    let (_, pubkey) = h.register_node(4);
    let other = h.funded_owner(MIN_STAKE * 2);
    h.registry.register(&other, &pubkey, &MIN_STAKE);
}

#[test]
fn reputation_climbs_to_full_weight_after_sustained_good_behaviour() {
    let h = setup();
    let (_, pubkey) = h.register_node(5);

    // 5000 -> 7000 at +50 per successful round is 40 rounds.
    for _ in 0..40 {
        h.registry.record_success(&pubkey, &0);
    }

    let node = h.registry.get_node(&pubkey).unwrap();
    assert!(node.reputation >= 7_000);
    assert_eq!(node.weight_bps, 10_000);
}

#[test]
fn an_outlier_loses_far_more_than_a_good_round_gains() {
    let h = setup();
    let (_, pubkey) = h.register_node(6);

    h.registry.record_success(&pubkey, &0);
    let after_success = h.registry.get_node(&pubkey).unwrap().reputation;

    h.registry.penalize(&pubkey, &500, &0);
    let after_penalty = h.registry.get_node(&pubkey).unwrap().reputation;

    assert_eq!(after_success, STARTING_REPUTATION + 50);
    assert_eq!(after_penalty, after_success - 500);

    // The asymmetry is the deterrent: one bad round costs what ten good ones
    // earn, so a node cannot profitably alternate between honest rounds and
    // opportunistic ones.
    let lost = after_success - after_penalty;
    let gained_per_round = after_success - STARTING_REPUTATION;
    assert_eq!(lost / gained_per_round, 10);

    let mut rounds_to_recover = 0;
    while h.registry.get_node(&pubkey).unwrap().reputation < after_success {
        h.registry.record_success(&pubkey, &0);
        rounds_to_recover += 1;
    }
    assert_eq!(
        rounds_to_recover, 10,
        "recovery must be slower than defection"
    );
}

#[test]
fn repeated_penalties_jail_the_node_and_zero_its_weight() {
    let h = setup();
    let (_, pubkey) = h.register_node(7);

    // 5000 -> below 3000. Four penalties land exactly on the threshold, which
    // is not below it, so it takes five.
    for _ in 0..5 {
        h.registry.penalize(&pubkey, &500, &0);
    }

    let node = h.registry.get_node(&pubkey).unwrap();
    assert!(node.reputation < JAIL_THRESHOLD);
    assert_eq!(node.status, NodeStatus::Jailed);
    assert_eq!(
        node.weight_bps, 0,
        "a jailed node must not influence the median"
    );
    assert_eq!(h.registry.weight_of(&pubkey), 0);
}

#[test]
fn the_jail_boundary_is_inclusive_at_the_threshold() {
    let h = setup();
    let (_, pubkey) = h.register_node(40);

    // Exactly on the threshold: still voting, at half weight.
    for _ in 0..4 {
        h.registry.penalize(&pubkey, &500, &0);
    }
    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.reputation, JAIL_THRESHOLD);
    assert_eq!(node.status, NodeStatus::Active);
    assert_eq!(node.weight_bps, 5_000);

    // One point below it: jailed, zero weight.
    h.registry.penalize(&pubkey, &1, &0);
    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.reputation, JAIL_THRESHOLD - 1);
    assert_eq!(node.status, NodeStatus::Jailed);
    assert_eq!(node.weight_bps, 0);
}

#[test]
fn slashing_moves_stake_out_of_the_node_and_into_the_slash_pool() {
    let h = setup();
    let (_, pubkey) = h.register_node(8);
    let slash = 100_0000000i128;

    h.registry.slash(&pubkey, &500, &slash);

    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.stake, MIN_STAKE - slash);
    assert_eq!(node.total_slashed, slash);
    assert_eq!(h.registry.slash_pool(), slash);
}

#[test]
fn slashing_more_than_the_remaining_stake_takes_what_is_there() {
    let h = setup();
    let (_, pubkey) = h.register_node(9);

    h.registry.slash(&pubkey, &100, &(MIN_STAKE * 5));

    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.stake, 0);
    assert_eq!(h.registry.slash_pool(), MIN_STAKE);
}

#[test]
fn rewards_are_paid_from_the_pool_and_skipped_when_it_is_empty() {
    let h = setup();
    let (owner, pubkey) = h.register_node(10);
    let reward = 1_0000000i128;

    // Empty pool: reputation still moves, no transfer happens.
    let balance_before = h.token.balance(&owner);
    h.registry.record_success(&pubkey, &reward);
    assert_eq!(
        h.token.balance(&owner),
        balance_before,
        "an empty pool must not stall consensus, only skip payment"
    );
    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().reputation,
        STARTING_REPUTATION + 50
    );

    // Funded pool: the reward is paid.
    let funder = h.funded_owner(reward * 10);
    h.registry.fund_rewards(&funder, &(reward * 10));
    h.registry.record_success(&pubkey, &reward);

    assert_eq!(h.token.balance(&owner), balance_before + reward);
    assert_eq!(h.registry.reward_pool(), reward * 9);
}

#[test]
fn an_exiting_node_stops_voting_immediately() {
    let h = setup();
    let (_, pubkey) = h.register_node(11);

    h.registry.request_unbond(&pubkey);

    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Exiting
    );
    assert_eq!(
        h.registry.weight_of(&pubkey),
        0,
        "a node on its way out must not vote on rounds it will not be around to answer for"
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")] // StillBonded
fn cannot_withdraw_before_the_unbonding_period_elapses() {
    let h = setup();
    let (_, pubkey) = h.register_node(12);

    h.registry.request_unbond(&pubkey);
    h.env.ledger().set_timestamp(UNBONDING - 1);
    h.registry.withdraw(&pubkey);
}

#[test]
fn withdrawal_returns_the_stake_and_removes_the_node() {
    let h = setup();
    let (owner, pubkey) = h.register_node(13);
    let before = h.token.balance(&owner);

    h.registry.request_unbond(&pubkey);
    h.env.ledger().set_timestamp(UNBONDING + 1);
    let returned = h.registry.withdraw(&pubkey);

    assert_eq!(returned, MIN_STAKE);
    assert_eq!(h.token.balance(&owner), before + MIN_STAKE);
    assert!(h.registry.get_node(&pubkey).is_none());
    assert_eq!(h.registry.list_nodes().len(), 0);
}

#[test]
fn a_slashed_node_can_only_withdraw_what_is_left() {
    let h = setup();
    let (owner, pubkey) = h.register_node(14);
    let before = h.token.balance(&owner);

    h.registry.slash(&pubkey, &100, &(MIN_STAKE / 2));
    h.registry.request_unbond(&pubkey);
    h.env.ledger().set_timestamp(UNBONDING + 1);
    let returned = h.registry.withdraw(&pubkey);

    assert_eq!(returned, MIN_STAKE / 2);
    assert_eq!(h.token.balance(&owner), before + MIN_STAKE / 2);
}

#[test]
fn capital_cannot_buy_a_release_but_serving_the_term_earns_one() {
    let h = setup();
    let (_, pubkey) = h.register_node(15);

    for _ in 0..5 {
        h.registry.penalize(&pubkey, &500, &0);
    }
    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Jailed
    );

    // Reputation is still below the threshold, so capital alone changes nothing.
    h.registry.add_stake(&pubkey, &MIN_STAKE);
    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Jailed,
        "buying stake must not buy back reputation"
    );

    // Serving the term does, and it restores a newcomer's standing rather than
    // the one the node spent five penalties losing.
    let until = h.registry.get_node(&pubkey).unwrap().jailed_until;
    h.env.ledger().set_timestamp(until);
    h.registry.release(&pubkey);

    let released = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(released.status, NodeStatus::Active);
    assert_eq!(released.reputation, STARTING_REPUTATION);
    assert!(
        released.stake > MIN_STAKE,
        "the top-up stays bonded; it was never a bribe"
    );
}

#[test]
fn total_weight_ignores_jailed_and_exiting_nodes() {
    let h = setup();
    let (_, a) = h.register_node(20);
    let (_, b) = h.register_node(21);
    let (_, c) = h.register_node(22);

    assert_eq!(h.registry.total_weight(), 15_000); // 3 x 5000

    for _ in 0..5 {
        h.registry.penalize(&b, &500, &0);
    }
    h.registry.request_unbond(&c);

    assert_eq!(h.registry.total_weight(), 5_000);
    assert_eq!(h.registry.weight_of(&a), 5_000);
}

#[test]
fn an_unknown_key_has_no_weight_and_reads_as_absent() {
    let h = setup();
    let stranger = h.pubkey(99);
    assert_eq!(h.registry.weight_of(&stranger), 0);
    assert!(h.registry.get_node(&stranger).is_none());
}

#[test]
fn misses_erode_weight_without_slashing() {
    let h = setup();
    let (_, pubkey) = h.register_node(30);

    // 25 reputation per miss: 81 misses to fall below 3000.
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }

    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.status, NodeStatus::Jailed);
    assert_eq!(
        node.total_slashed, 0,
        "downtime is not theft; it must not cost stake"
    );
    assert_eq!(node.consecutive_misses, 81);
}

#[test]
fn admin_can_repoint_the_aggregator_after_deployment() {
    let h = setup();
    let new_aggregator = Address::generate(&h.env);

    h.registry.set_aggregator(&new_aggregator);

    assert_eq!(h.registry.get_config().aggregator, new_aggregator);
    assert_eq!(h.registry.get_config().admin, h.admin);
    assert_eq!(h.registry.get_config().slasher, h.slasher);
    // The old aggregator address is no longer the configured one.
    assert_ne!(h.registry.get_config().aggregator, h.aggregator);
}

#[test]
fn owner_of_names_the_account_that_bonded_the_stake() {
    let h = setup();
    let (owner, pubkey) = h.register_node(40);

    assert_eq!(h.registry.owner_of(&pubkey), Some(owner));
    assert_eq!(
        h.registry.owner_of(&h.pubkey(41)),
        None,
        "an unregistered key has no owner, and asking must not trap"
    );
}

#[test]
fn seized_stake_leaves_the_pool_only_through_the_slashing_contract() {
    let h = setup();
    let (_, pubkey) = h.register_node(42);
    let beneficiary = Address::generate(&h.env);

    h.registry.slash(&pubkey, &0, &(MIN_STAKE / 2));
    assert_eq!(h.registry.slash_pool(), MIN_STAKE / 2);

    h.registry
        .pay_from_slash_pool(&beneficiary, &(MIN_STAKE / 4));

    assert_eq!(h.token.balance(&beneficiary), MIN_STAKE / 4);
    assert_eq!(h.registry.slash_pool(), MIN_STAKE / 4);
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")] // SlashPoolExhausted
fn the_slash_pool_cannot_pay_out_more_than_it_holds() {
    let h = setup();
    let (_, pubkey) = h.register_node(43);
    h.registry.slash(&pubkey, &0, &1_000);

    h.registry
        .pay_from_slash_pool(&Address::generate(&h.env), &1_001);
}

#[test]
fn a_penalty_event_carries_the_arithmetic_that_produced_it() {
    use soroban_sdk::events::Event as _;
    use soroban_sdk::testutils::Events as _;

    let h = setup();
    let (_, pubkey) = h.register_node(44);
    h.registry.penalize(&pubkey, &500, &1_000);
    // Only the most recent invocation's events are retained, so they are read
    // before anything else is called.
    let published = h.env.events().all().filter_by_contract(&h.registry.address);

    let node = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(node.reputation, STARTING_REPUTATION - 500);
    assert_eq!(node.stake, MIN_STAKE - 1_000);

    // An operator contesting the penalty can read the resulting standing
    // straight off the event rather than having to replay the contract.
    let expected = crate::events::NodePenalized {
        pubkey,
        reason: soroban_sdk::symbol_short!("outlier"),
        reputation_delta: 500,
        seized: 1_000,
        reputation: node.reputation,
        stake: node.stake,
    }
    .to_xdr(&h.env, &h.registry.address);
    assert!(
        published.events().contains(&expected),
        "the penalty event did not carry the standing it produced"
    );
}

#[test]
fn a_jailed_node_cannot_buy_or_earn_its_way_out() {
    let h = setup();
    let (_, pubkey) = h.register_node(50);
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }
    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Jailed
    );

    // Capital does not clear jail. It cannot: jail begins exactly when
    // reputation falls below the threshold, so a condition on reputation can
    // never be met by paying.
    h.registry.add_stake(&pubkey, &(MIN_STAKE * 5));
    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Jailed
    );
    assert_eq!(h.registry.weight_of(&pubkey), 0);

    // And it cannot earn its way out either, because the aggregator refuses a
    // zero-weight submission outright -- so `record_success` is never reached
    // for a jailed node. Serving the term is the only route, which is why
    // `release` exists at all.
}

#[test]
fn a_jailed_node_is_released_once_it_has_served_its_term() {
    let h = setup();
    let (_, pubkey) = h.register_node(51);
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }

    let jailed = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(jailed.status, NodeStatus::Jailed);
    assert_eq!(
        jailed.jailed_until,
        h.env.ledger().timestamp() + JAIL_PERIOD
    );

    h.env.ledger().set_timestamp(jailed.jailed_until);
    h.registry.release(&pubkey);

    let released = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(released.status, NodeStatus::Active);
    assert_eq!(
        released.reputation, STARTING_REPUTATION,
        "released to exactly a newcomer's standing -- no better, because that \
         would make jail cheaper than being new, and no worse, because \
         re-registering is always available and would leave them here anyway"
    );
    assert_eq!(released.weight_bps, 5_000, "half weight, and earning back");
    assert_eq!(released.consecutive_misses, 0);
    assert_eq!(released.jailed_until, 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #16)")] // StillJailed
fn a_jailed_node_cannot_be_released_early() {
    let h = setup();
    let (_, pubkey) = h.register_node(52);
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }
    let until = h.registry.get_node(&pubkey).unwrap().jailed_until;
    h.env.ledger().set_timestamp(until - 1);
    h.registry.release(&pubkey);
}

#[test]
#[should_panic(expected = "Error(Contract, #15)")] // NotJailed
fn releasing_a_node_that_is_not_jailed_is_refused() {
    let h = setup();
    let (_, pubkey) = h.register_node(53);
    h.registry.release(&pubkey);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")] // StakeTooLow
fn a_node_slashed_below_the_minimum_must_top_up_before_release() {
    let h = setup();
    let (_, pubkey) = h.register_node(54);
    // Slashed to nothing and jailed by the same penalty.
    h.registry.slash(&pubkey, &3_000, &MIN_STAKE);

    let until = h.registry.get_node(&pubkey).unwrap().jailed_until;
    h.env.ledger().set_timestamp(until);
    // Coming back under-bonded would mean voting with less at risk than the
    // network requires of everyone else.
    h.registry.release(&pubkey);
}

#[test]
fn topping_up_lets_an_emptied_node_be_released() {
    let h = setup();
    let (_, pubkey) = h.register_node(55);
    h.registry.slash(&pubkey, &3_000, &MIN_STAKE);

    h.registry.add_stake(&pubkey, &MIN_STAKE);
    let until = h.registry.get_node(&pubkey).unwrap().jailed_until;
    h.env.ledger().set_timestamp(until);
    h.registry.release(&pubkey);

    assert_eq!(
        h.registry.get_node(&pubkey).unwrap().status,
        NodeStatus::Active
    );
}

#[test]
fn a_released_node_that_relapses_serves_a_fresh_term() {
    let h = setup();
    let (_, pubkey) = h.register_node(56);
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }
    let first_term = h.registry.get_node(&pubkey).unwrap().jailed_until;
    h.env.ledger().set_timestamp(first_term);
    h.registry.release(&pubkey);

    // Back at 5000, so it takes another 81 misses to fall below 3000 again.
    for _ in 0..81 {
        h.registry.record_miss(&pubkey);
    }
    let second = h.registry.get_node(&pubkey).unwrap();
    assert_eq!(second.status, NodeStatus::Jailed);
    assert_eq!(second.jailed_until, first_term + JAIL_PERIOD);
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")] // InvalidConfig
fn a_zero_unbonding_period_is_refused_at_deployment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let registry = RegistryClient::new(&env, &env.register(Registry, ()));

    // Zero would let a node publish a bad price and withdraw in the same
    // ledger, which is the one thing the delay exists to prevent.
    registry.initialize(
        &admin,
        &admin,
        &admin,
        &sac.address(),
        &MIN_STAKE,
        &0,
        &JAIL_PERIOD,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")] // InvalidConfig
fn a_jail_term_longer_than_the_unbonding_period_is_refused_at_deployment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let registry = RegistryClient::new(&env, &env.register(Registry, ()));

    // If serving the term takes longer than exiting, nobody ever serves it:
    // unbond, withdraw, register a fresh key at a newcomer's reputation, and
    // the jail term has cost nothing but the identity it was attached to.
    registry.initialize(
        &admin,
        &admin,
        &admin,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &(UNBONDING + 1),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")] // InvalidConfig
fn a_jail_term_equal_to_the_unbonding_period_is_refused_at_deployment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let registry = RegistryClient::new(&env, &env.register(Registry, ()));

    // Equal is no better than longer: it makes the two paths back the same
    // length, and one of them also refunds the stake.
    registry.initialize(
        &admin,
        &admin,
        &admin,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &UNBONDING,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #14)")] // InvalidConfig
fn a_zero_jail_term_is_refused_at_deployment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let registry = RegistryClient::new(&env, &env.register(Registry, ()));

    // A zero term makes jail a formality: a node could be jailed and released
    // inside one ledger, so falling below the threshold would cost nothing but
    // the reputation already lost, and `release` would be a free reset back to
    // a newcomer's standing.
    registry.initialize(
        &admin,
        &admin,
        &admin,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &0,
    );
}
