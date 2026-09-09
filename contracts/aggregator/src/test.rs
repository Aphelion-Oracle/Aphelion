use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{symbol_short, Address, BytesN, Env, Symbol, Vec};

use crate::{Aggregator, AggregatorClient, Config, DataKey, PriceData};
use aphelion_registry::{Registry, RegistryClient, STARTING_REPUTATION};

const BASE_TIME: u64 = 1_735_689_600;
const MIN_STAKE: i128 = 10_000_000_000; // 1000 XLM in stroops
const UNBONDING: u64 = 7 * 24 * 3600;
const PRICE_SCALE: i128 = 100_000_000;

/// $64_231.55, the price the whole suite treats as the truth.
const TRUE_PRICE: i128 = 6423155 * 1_000_000;

fn feed() -> &'static str {
    "BTC_USD"
}

struct Harness<'a> {
    env: Env,
    aggregator: AggregatorClient<'a>,
    registry: RegistryClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    aggregator_id: [u8; 32],
    keys: std::vec::Vec<SigningKey>,
}

fn base_config(admin: &Address, registry: &Address, token: &Address) -> Config {
    Config {
        admin: admin.clone(),
        registry: registry.clone(),
        token: token.clone(),
        quorum: 3,
        min_weight_bps: 15_000,
        max_deviation_bps: 500,
        max_staleness: 300,
        max_future_drift: 30,
        min_round_interval: 60,
        round_timeout: 120,
        absence_threshold: 600,
        reward_per_submission: 10,
        outlier_rep_penalty: 500,
        outlier_slash: 10_000_000, // 1 XLM
        history_len: 5,
        read_fee: 5,
    }
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(BASE_TIME);

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let registry_id = env.register(Registry, ());
    let aggregator_id = env.register(Aggregator, ());

    let registry = RegistryClient::new(&env, &registry_id);
    registry.initialize(
        &admin,
        &aggregator_id,
        &admin,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
    );

    let aggregator = AggregatorClient::new(&env, &aggregator_id);
    aggregator.initialize(&base_config(&admin, &registry_id, &sac.address()));
    aggregator.set_feed(&Symbol::new(&env, feed()), &true, &300, &0);

    let id_bytes = crate::message::contract_id_bytes(&env, &aggregator_id).to_array();

    // Deterministic keys: the same node is always node 0, so a failure names a
    // node rather than a random 32-byte string.
    let keys = (1u8..=8)
        .map(|i| SigningKey::from_bytes(&[i; 32]))
        .collect::<std::vec::Vec<_>>();

    let h = Harness {
        env,
        aggregator,
        registry,
        token,
        token_admin,
        aggregator_id: id_bytes,
        keys,
    };
    for i in 0..5 {
        h.register_node(i);
    }
    h
}

impl Harness<'_> {
    fn pubkey(&self, node: usize) -> BytesN<32> {
        BytesN::from_array(&self.env, &self.keys[node].verifying_key().to_bytes())
    }

    fn register_node(&self, node: usize) -> Address {
        let owner = Address::generate(&self.env);
        self.token_admin.mint(&owner, &(MIN_STAKE * 4));
        self.registry
            .register(&owner, &self.pubkey(node), &(MIN_STAKE * 2));
        owner
    }

    /// Drive a node's reputation up to full voting weight the way the network
    /// would: one in-band round at a time.
    fn promote(&self, node: usize) {
        let pubkey = self.pubkey(node);
        while self.registry.weight_of(&pubkey) < 10_000 {
            self.registry.record_success(&pubkey, &0);
        }
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    /// The canonical 117-byte payload, built independently of the contract's
    /// own encoder so that a drift in either is a test failure rather than a
    /// silent agreement to be wrong together.
    fn message(
        &self,
        aggregator: &[u8; 32],
        feed_id: &str,
        price: i128,
        timestamp: u64,
        confidence_bps: u32,
        nonce: u64,
    ) -> std::vec::Vec<u8> {
        let mut buf = std::vec::Vec::with_capacity(117);
        buf.extend_from_slice(b"APHELION_PRICE_V1");
        buf.extend_from_slice(aggregator);
        let mut padded = [0u8; 32];
        padded[..feed_id.len()].copy_from_slice(feed_id.as_bytes());
        buf.extend_from_slice(&padded);
        buf.extend_from_slice(&price.to_be_bytes());
        buf.extend_from_slice(&timestamp.to_be_bytes());
        buf.extend_from_slice(&confidence_bps.to_be_bytes());
        buf.extend_from_slice(&nonce.to_be_bytes());
        assert_eq!(buf.len(), 117);
        buf
    }

    /// The general form: what was signed and what was sent can differ, which
    /// is the whole point of several tests below.
    #[allow(clippy::too_many_arguments)]
    fn submit_as(
        &self,
        node: usize,
        signed_aggregator: &[u8; 32],
        signed_feed: &str,
        sent_feed: &str,
        price: i128,
        timestamp: u64,
        confidence_bps: u32,
        nonce: u64,
    ) -> bool {
        let payload = self.message(
            signed_aggregator,
            signed_feed,
            price,
            timestamp,
            confidence_bps,
            nonce,
        );
        let signature = self.keys[node].sign(&payload).to_bytes();
        self.aggregator.submit_price(
            &Symbol::new(&self.env, sent_feed),
            &self.pubkey(node),
            &price,
            &timestamp,
            &confidence_bps,
            &nonce,
            &BytesN::from_array(&self.env, &signature),
        )
    }

    /// The ordinary case: an honest submission for the standard feed.
    fn submit(&self, node: usize, price: i128, nonce: u64) -> bool {
        let now = self.env.ledger().timestamp();
        self.submit_as(
            node,
            &self.aggregator_id.clone(),
            feed(),
            feed(),
            price,
            now,
            25,
            nonce,
        )
    }

    fn price(&self) -> Option<PriceData> {
        self.aggregator.get_price(&Symbol::new(&self.env, feed()))
    }
}

// -- consensus --------------------------------------------------------------

#[test]
fn a_round_closes_once_it_has_both_the_headcount_and_the_weight() {
    let h = setup();

    assert!(!h.submit(0, TRUE_PRICE, 1), "one node is not a network");
    assert!(!h.submit(1, TRUE_PRICE, 1));
    assert!(h.price().is_none(), "nothing may publish before quorum");

    assert!(
        h.submit(2, TRUE_PRICE, 1),
        "the third node closes the round"
    );

    let price = h.price().expect("published");
    assert_eq!(price.price, TRUE_PRICE);
    assert_eq!(price.num_nodes, 3);
    assert_eq!(price.round_id, 1);
}

#[test]
fn a_headcount_without_weight_does_not_close_a_round() {
    let h = setup();
    let mut config = h.aggregator.get_config();
    // Three half-weight nodes carry 15_000 bps; demand more than they have.
    config.min_weight_bps = 20_000;
    h.aggregator.set_config(&config);

    assert!(!h.submit(0, TRUE_PRICE, 1));
    assert!(!h.submit(1, TRUE_PRICE, 1));
    assert!(
        !h.submit(2, TRUE_PRICE, 1),
        "quorum alone must not be enough to speak for the network"
    );
    assert!(h.price().is_none());

    // A fourth half-weight node takes the round to 20_000 bps.
    assert!(h.submit(3, TRUE_PRICE, 1));
    assert!(h.price().is_some());
}

#[test]
fn the_published_price_is_the_median_not_the_mean() {
    let h = setup();
    // Two honest nodes two dollars apart, and one claiming BTC is worth $300.
    let low = TRUE_PRICE - 200_000_000;
    h.submit(0, low, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, 300 * PRICE_SCALE, 1);

    let published = h.price().expect("published").price;
    assert_eq!(
        published, low,
        "the median is the middle honest quote, untouched by the liar"
    );
    assert!(
        crate::math::deviation_bps(published, TRUE_PRICE) < 10,
        "the published price stays within a basis point or two of the truth"
    );

    let mean = (low + TRUE_PRICE + 300 * PRICE_SCALE) / 3;
    assert!(
        (mean - TRUE_PRICE).abs() > 20_000 * PRICE_SCALE,
        "the mean would have been dragged twenty thousand dollars away"
    );
}

#[test]
fn an_outlier_loses_reputation_and_stake_while_the_honest_are_paid() {
    let h = setup();
    let liar = h.pubkey(2);
    let honest = h.pubkey(0);
    let before_liar = h.registry.get_node(&liar).unwrap();
    let before_honest = h.registry.get_node(&honest).unwrap();

    h.registry.fund_rewards(&h.admin_funded(1_000), &1_000);

    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, 300 * PRICE_SCALE, 1);

    let after_liar = h.registry.get_node(&liar).unwrap();
    let after_honest = h.registry.get_node(&honest).unwrap();

    assert_eq!(after_liar.reputation, before_liar.reputation - 500);
    assert_eq!(after_liar.stake, before_liar.stake - 10_000_000);
    assert!(after_honest.reputation > before_honest.reputation);
    assert_eq!(after_honest.total_rewards, 10);
}

#[test]
fn an_outlier_does_not_widen_the_confidence_consumers_rely_on() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, 300 * PRICE_SCALE, 1);

    let price = h.price().expect("published");
    assert_eq!(
        price.num_nodes, 2,
        "the outlier voted, but it did not contribute to the statistics"
    );
    assert!(
        price.confidence_bps < 500,
        "a penalised submission must not widen the published interval, got {}",
        price.confidence_bps
    );
    assert_eq!(price.deviation, 0, "the two in-band nodes agreed exactly");
}

#[test]
fn confidence_widens_when_honest_nodes_disagree() {
    let h = setup();
    // A 1% spread, inside the 5% band, so nobody is penalised.
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE * 101 / 100, 1);
    h.submit(2, TRUE_PRICE * 99 / 100, 1);

    let price = h.price().expect("published");
    assert_eq!(price.num_nodes, 3);
    assert!(
        price.confidence_bps >= 100,
        "a 1% disagreement must show up as at least 100 bps, got {}",
        price.confidence_bps
    );
    assert!(price.deviation > 0);
}

#[test]
fn the_published_timestamp_is_the_oldest_contributing_observation() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let id = h.aggregator_id;

    h.submit_as(0, &id, feed(), feed(), TRUE_PRICE, now - 90, 25, 1);
    h.submit_as(1, &id, feed(), feed(), TRUE_PRICE, now - 10, 25, 1);
    h.submit_as(2, &id, feed(), feed(), TRUE_PRICE, now, 25, 1);

    let price = h.price().expect("published");
    assert_eq!(
        price.timestamp,
        now - 90,
        "one fast node must not make a stale round look fresh"
    );
}

#[test]
fn a_round_where_nobody_agrees_publishes_nothing() {
    let h = setup();
    let mut config = h.aggregator.get_config();
    config.quorum = 2;
    config.min_weight_bps = 10_000;
    h.aggregator.set_config(&config);

    // Two submissions straddling a midpoint that neither reported: both land
    // outside the 5% band around the median.
    h.submit(0, 100 * PRICE_SCALE, 1);
    h.submit(1, 200 * PRICE_SCALE, 1);

    assert!(
        h.price().is_none(),
        "a network that did not agree must not publish"
    );
    assert!(h.registry.get_node(&h.pubkey(0)).unwrap().reputation < 5_000);
    assert!(h.registry.get_node(&h.pubkey(1)).unwrap().reputation < 5_000);
}

#[test]
fn weight_is_captured_at_submission_and_not_re_read_at_finalisation() {
    let h = setup();
    let mut config = h.aggregator.get_config();
    config.quorum = 4;
    h.aggregator.set_config(&config);
    h.promote(0);

    // Node 0 votes at full weight for the true price.
    h.submit(0, TRUE_PRICE, 1);

    // Its reputation then collapses mid-round, all the way to jail.
    h.registry.penalize(&h.pubkey(0), &6_000, &0);
    assert_eq!(h.registry.weight_of(&h.pubkey(0)), 0, "now jailed");

    // One more node agrees with it; two disagree by 2%, inside the band.
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE * 102 / 100, 1);
    h.submit(3, TRUE_PRICE * 102 / 100, 1);

    let price = h.price().expect("published");
    assert_eq!(
        price.price, TRUE_PRICE,
        "the 10_000 bps cast before the fall still counts; re-reading the \
         registry at finalisation would have handed the round to the other two"
    );
}

// -- authentication ---------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // NotAuthorizedNode
fn an_unregistered_key_carries_no_authority() {
    let h = setup();
    h.submit(7, TRUE_PRICE, 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // NotAuthorizedNode
fn a_jailed_node_cannot_submit() {
    let h = setup();
    for _ in 0..100 {
        h.registry.record_miss(&h.pubkey(0));
    }
    h.submit(0, TRUE_PRICE, 1);
}

#[test]
#[should_panic] // the host traps on a failed ed25519 verification
fn a_signature_for_another_feed_does_not_verify() {
    let h = setup();
    h.aggregator
        .set_feed(&Symbol::new(&h.env, "ETH_USD"), &true, &300, &0);
    let now = h.env.ledger().timestamp();
    let id = h.aggregator_id;
    // Signed as ETH_USD, presented as BTC_USD.
    h.submit_as(0, &id, "ETH_USD", feed(), TRUE_PRICE, now, 25, 1);
}

#[test]
#[should_panic] // the host traps on a failed ed25519 verification
fn a_signature_for_another_deployment_does_not_verify() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    // The same price, signed against a different aggregator's contract id --
    // this is exactly a testnet signature replayed onto mainnet.
    h.submit_as(0, &[0x11; 32], feed(), feed(), TRUE_PRICE, now, 25, 1);
}

#[test]
#[should_panic] // the host traps on a failed ed25519 verification
fn a_tampered_price_does_not_verify() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let payload = h.message(&h.aggregator_id, feed(), TRUE_PRICE, now, 25, 1);
    let signature = h.keys[0].sign(&payload).to_bytes();
    // A relayer inflates the price by a dollar on its way to the chain.
    h.aggregator.submit_price(
        &Symbol::new(&h.env, feed()),
        &h.pubkey(0),
        &(TRUE_PRICE + PRICE_SCALE),
        &now,
        &25,
        &1,
        &BytesN::from_array(&h.env, &signature),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #22)")] // NonceNotIncreasing
fn a_nonce_cannot_be_reused() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 7);
    h.submit(0, TRUE_PRICE, 7);
}

#[test]
fn a_replayed_submission_is_rejected_even_while_still_fresh() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let payload = h.message(&h.aggregator_id, feed(), TRUE_PRICE, now, 25, 1);
    let signature = BytesN::from_array(&h.env, &h.keys[0].sign(&payload).to_bytes());
    let f = Symbol::new(&h.env, feed());

    h.aggregator
        .submit_price(&f, &h.pubkey(0), &TRUE_PRICE, &now, &25, &1, &signature);
    // Byte-identical replay, inside the staleness window: the nonce is what
    // stops it, because the signature is perfectly valid.
    let replay =
        h.aggregator
            .try_submit_price(&f, &h.pubkey(0), &TRUE_PRICE, &now, &25, &1, &signature);
    assert!(replay.is_err());
}

#[test]
#[should_panic(expected = "Error(Contract, #25)")] // DuplicateSubmission
fn a_node_votes_at_most_once_per_round() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(0, TRUE_PRICE, 2);
}

// -- freshness --------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #23)")] // StaleObservation
fn an_observation_older_than_the_staleness_window_is_rejected() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let id = h.aggregator_id;
    h.submit_as(0, &id, feed(), feed(), TRUE_PRICE, now - 301, 25, 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #24)")] // FutureObservation
fn an_observation_from_the_future_is_rejected() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let id = h.aggregator_id;
    h.submit_as(0, &id, feed(), feed(), TRUE_PRICE, now + 31, 25, 1);
}

#[test]
fn a_slightly_fast_clock_is_tolerated() {
    let h = setup();
    let now = h.env.ledger().timestamp();
    let id = h.aggregator_id;
    h.submit_as(0, &id, feed(), feed(), TRUE_PRICE, now + 29, 25, 1);
    assert_eq!(
        h.aggregator
            .pending_round(&Symbol::new(&h.env, feed()))
            .unwrap()
            .submissions
            .len(),
        1
    );
}

// -- round lifecycle --------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #26)")] // RoundTooSoon
fn a_new_round_cannot_open_before_the_minimum_interval() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);
    // The feed published a moment ago; republishing now would be paying to say
    // the same thing twice.
    h.submit(0, TRUE_PRICE, 2);
}

#[test]
fn consecutive_rounds_publish_once_the_interval_has_passed() {
    let h = setup();
    for round in 1..=3u64 {
        h.submit(0, TRUE_PRICE, round);
        h.submit(1, TRUE_PRICE, round);
        h.submit(2, TRUE_PRICE, round);
        assert_eq!(h.price().unwrap().round_id, round);
        h.advance(61);
    }
}

#[test]
fn a_round_that_never_reaches_quorum_is_abandoned_rather_than_wedging_the_feed() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);

    // Nobody else shows up for two minutes.
    h.advance(121);

    // The next submission starts a fresh round rather than joining a stale one.
    h.submit(2, TRUE_PRICE, 2);
    let pending = h
        .aggregator
        .pending_round(&Symbol::new(&h.env, feed()))
        .expect("a new round is open");
    assert_eq!(pending.round_id, 2);
    assert_eq!(pending.submissions.len(), 1);
}

// -- reads ------------------------------------------------------------------

#[test]
fn an_unknown_feed_reads_as_absent_not_as_zero() {
    let h = setup();
    assert!(h.aggregator.get_price(&symbol_short!("NOPE")).is_none());
}

#[test]
#[should_panic(expected = "Error(Contract, #31)")] // StalePrice
fn get_price_checked_refuses_a_price_past_its_age_limit() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);
    h.advance(400);
    h.aggregator
        .get_price_checked(&Symbol::new(&h.env, feed()), &300);
}

#[test]
#[should_panic(expected = "Error(Contract, #30)")] // NoPrice
fn get_price_checked_on_a_feed_that_never_published_is_not_a_zero() {
    let h = setup();
    h.aggregator.get_price_checked(&symbol_short!("NOPE"), &300);
}

#[test]
fn twap_discounts_a_momentary_spike() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());

    // The price stands at the truth for four rounds, then spikes for one.
    let mut nonce = 0u64;
    for _ in 0..4 {
        nonce += 1;
        h.submit(0, TRUE_PRICE, nonce);
        h.submit(1, TRUE_PRICE, nonce);
        h.submit(2, TRUE_PRICE, nonce);
        h.advance(300);
    }
    nonce += 1;
    let spike = TRUE_PRICE * 2;
    h.submit(0, spike, nonce);
    h.submit(1, spike, nonce);
    h.submit(2, spike, nonce);
    h.advance(10);

    let spot = h.price().unwrap().price;
    let twap = h.aggregator.get_twap(&f, &1_200);
    assert_eq!(spot, spike);
    assert!(
        twap < TRUE_PRICE * 105 / 100,
        "a ten-second spike must barely move a twenty-minute average, got {twap}"
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #32)")] // InsufficientHistory
fn twap_refuses_a_window_the_history_cannot_cover() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);
    h.advance(60);
    // One minute of history, asked for a day's average. A partial answer here
    // would be indistinguishable from a real one.
    h.aggregator.get_twap(&Symbol::new(&h.env, feed()), &86_400);
}

#[test]
fn the_history_ring_keeps_the_configured_number_of_observations() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    for round in 1..=8u64 {
        h.submit(0, TRUE_PRICE + round as i128, round);
        h.submit(1, TRUE_PRICE + round as i128, round);
        h.submit(2, TRUE_PRICE + round as i128, round);
        h.advance(61);
    }
    let history = h.aggregator.history(&f);
    assert_eq!(history.len(), 5, "history_len is 5");
    assert_eq!(
        history.get(4).unwrap().price,
        TRUE_PRICE + 8,
        "the newest observation is last"
    );
    assert_eq!(history.get(0).unwrap().price, TRUE_PRICE + 4);
}

#[test]
fn last_nonce_reports_what_a_restored_node_needs_to_skip_past() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    h.submit(0, TRUE_PRICE, 12);
    assert_eq!(h.aggregator.last_nonce(&h.pubkey(0), &f), 12);
    assert_eq!(h.aggregator.last_nonce(&h.pubkey(1), &f), 0);
}

// -- metering ---------------------------------------------------------------

#[test]
fn metered_reads_draw_down_a_prepaid_balance_and_reach_the_reward_pool() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);

    let consumer = Address::generate(&h.env);
    h.token_admin.mint(&consumer, &1_000);
    h.aggregator.deposit(&consumer, &100);
    assert_eq!(h.aggregator.balance(&consumer), 100);

    for _ in 0..4 {
        h.aggregator.get_price_metered(&consumer, &f, &300);
    }
    assert_eq!(h.aggregator.balance(&consumer), 80);
    assert_eq!(h.aggregator.collected_fees(), 20);

    let pool_before = h.registry.reward_pool();
    assert_eq!(h.aggregator.forward_fees(), 20);
    assert_eq!(h.registry.reward_pool(), pool_before + 20);
    assert_eq!(h.aggregator.collected_fees(), 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #40)")] // InsufficientBalance
fn a_metered_read_without_balance_is_refused() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);

    let consumer = Address::generate(&h.env);
    h.aggregator.get_price_metered(&consumer, &f, &300);
}

#[test]
fn an_unused_balance_can_be_withdrawn() {
    let h = setup();
    let consumer = Address::generate(&h.env);
    h.token_admin.mint(&consumer, &1_000);
    h.aggregator.deposit(&consumer, &400);
    h.aggregator.refund(&consumer, &400);
    assert_eq!(h.aggregator.balance(&consumer), 0);
    assert_eq!(h.token.balance(&consumer), 1_000);
}

// -- absence ----------------------------------------------------------------

#[test]
fn a_silent_node_is_charged_one_miss_per_silence_not_one_per_sweep() {
    let h = setup();
    h.submit(0, TRUE_PRICE, 1);
    let before = h.registry.get_node(&h.pubkey(0)).unwrap().reputation;

    let keys = Vec::from_array(&h.env, [h.pubkey(0)]);

    // Still within the threshold: nothing is owed yet.
    h.advance(300);
    assert_eq!(h.aggregator.sweep_absent(&keys), 0);

    h.advance(400);
    assert_eq!(h.aggregator.sweep_absent(&keys), 1);
    assert_eq!(
        h.registry.get_node(&h.pubkey(0)).unwrap().reputation,
        before - 25
    );

    // A second sweep in the same silence must not bill it twice.
    assert_eq!(h.aggregator.sweep_absent(&keys), 0);
    assert_eq!(
        h.registry.get_node(&h.pubkey(0)).unwrap().reputation,
        before - 25
    );
}

#[test]
fn a_node_never_seen_gets_a_baseline_rather_than_a_penalty() {
    let h = setup();
    let keys = Vec::from_array(&h.env, [h.pubkey(4)]);
    let before = h.registry.get_node(&h.pubkey(4)).unwrap().reputation;

    // A node that registered a moment ago has no participation history. That
    // is not evidence of absence.
    assert_eq!(h.aggregator.sweep_absent(&keys), 0);
    assert_eq!(
        h.registry.get_node(&h.pubkey(4)).unwrap().reputation,
        before
    );

    h.advance(700);
    assert_eq!(h.aggregator.sweep_absent(&keys), 1, "now it is");
}

#[test]
fn sweeping_an_unknown_key_is_a_no_op_rather_than_a_trap() {
    let h = setup();
    let keys = Vec::from_array(&h.env, [h.pubkey(7)]);
    h.advance(10_000);
    assert_eq!(h.aggregator.sweep_absent(&keys), 0);
}

// -- configuration ----------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // FeedDisabled
fn a_disabled_feed_accepts_nothing() {
    let h = setup();
    h.aggregator
        .set_feed(&Symbol::new(&h.env, feed()), &false, &300, &0);
    h.submit(0, TRUE_PRICE, 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")] // UnknownFeed
fn an_unconfigured_feed_accepts_nothing() {
    let h = setup();
    let id = h.aggregator_id;
    let now = h.env.ledger().timestamp();
    h.submit_as(0, &id, "DOGE_USD", "DOGE_USD", TRUE_PRICE, now, 25, 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")] // InvalidConfig
fn a_quorum_of_zero_is_refused_at_the_point_it_is_set() {
    let h = setup();
    let mut config = h.aggregator.get_config();
    config.quorum = 0;
    h.aggregator.set_config(&config);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")] // AlreadyInitialized
fn initialising_twice_is_refused() {
    let h = setup();
    let config = h.aggregator.get_config();
    h.aggregator.initialize(&config);
}

#[test]
fn a_feed_specific_minimum_overrides_the_network_quorum() {
    let h = setup();
    let f = Symbol::new(&h.env, "ETH_USD");
    h.aggregator.set_feed(&f, &true, &300, &4);

    let id = h.aggregator_id;
    let now = h.env.ledger().timestamp();
    for node in 0..3 {
        assert!(!h.submit_as(node, &id, "ETH_USD", "ETH_USD", TRUE_PRICE, now, 25, 1));
    }
    assert!(
        h.submit_as(3, &id, "ETH_USD", "ETH_USD", TRUE_PRICE, now, 25, 1),
        "the feed asked for four, not the network's three"
    );
    assert!(h.aggregator.get_price(&f).is_some());
}

#[test]
fn the_feed_index_lists_what_has_been_configured() {
    let h = setup();
    h.aggregator
        .set_feed(&Symbol::new(&h.env, "ETH_USD"), &true, &300, &0);
    let feeds = h.aggregator.feeds();
    assert_eq!(feeds.len(), 2);
    assert!(feeds.contains(Symbol::new(&h.env, feed())));
}

#[test]
fn reconfiguring_a_feed_does_not_duplicate_it_in_the_index() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    h.aggregator.set_feed(&f, &true, &600, &0);
    h.aggregator.set_feed(&f, &true, &900, &0);
    assert_eq!(h.aggregator.feeds().len(), 1);
    assert_eq!(h.aggregator.feed_config(&f).heartbeat, 900);
}

// -- storage ----------------------------------------------------------------

#[test]
fn a_closed_round_leaves_no_pending_state_behind() {
    let h = setup();
    let f = Symbol::new(&h.env, feed());
    h.submit(0, TRUE_PRICE, 1);
    h.submit(1, TRUE_PRICE, 1);
    h.submit(2, TRUE_PRICE, 1);

    assert!(h.aggregator.pending_round(&f).is_none());
    h.env.as_contract(&h.aggregator.address, || {
        assert!(!h
            .env
            .storage()
            .persistent()
            .has(&DataKey::Round(Symbol::new(&h.env, feed()))));
    });
}

impl Harness<'_> {
    /// An account holding `amount` of the token, for funding the reward pool.
    fn admin_funded(&self, amount: i128) -> Address {
        let a = Address::generate(&self.env);
        self.token_admin.mint(&a, &amount);
        a
    }
}

// -- Byzantine simulation ---------------------------------------------------

/// Multi-round scenarios, run against the real contract pair.
///
/// The tests above check one mechanism at a time. These check the properties
/// the mechanisms exist to produce: that a minority of liars cannot move a
/// price, that lying costs more than it pays, that capital alone does not buy
/// influence, and that the network sheds a bad operator and keeps working
/// rather than stalling on their absence.
mod byzantine {
    use super::*;

    /// A node that reports a price no market has ever quoted.
    const LIE: i128 = 300 * PRICE_SCALE;

    /// Everyone submits, so nothing closes early and every vote is counted.
    fn quorum_of(h: &Harness, nodes: u32, weight_bps: u32) {
        let mut config = h.aggregator.get_config();
        config.quorum = nodes;
        config.min_weight_bps = weight_bps;
        h.aggregator.set_config(&config);
    }

    #[test]
    fn a_minority_of_liars_cannot_move_the_price_and_does_not_survive_trying() {
        let h = setup();
        quorum_of(&h, 5, 25_000);

        // Three honest nodes and two lying every round. Five rounds is how
        // long it takes a liar to fall from 5_000 reputation to below the
        // 3_000 jail threshold at 500 a round.
        for round in 1..=5u64 {
            for node in 0..3 {
                h.submit(node, TRUE_PRICE, round);
            }
            for node in 3..5 {
                h.submit(node, LIE, round);
            }

            let price = h.price().expect("the round published");
            assert_eq!(
                price.price, TRUE_PRICE,
                "round {round}: two liars out of five moved the median"
            );
            assert_eq!(
                price.num_nodes, 3,
                "round {round}: the liars must not appear in the statistics"
            );
            h.advance(61);
        }

        for node in 3..5 {
            assert_eq!(
                h.registry.weight_of(&h.pubkey(node)),
                0,
                "a node that lied five times running is still voting"
            );
        }
        for node in 0..3 {
            assert!(h.registry.get_node(&h.pubkey(node)).unwrap().reputation > STARTING_REPUTATION);
        }
    }

    #[test]
    fn the_network_keeps_publishing_after_shedding_its_liars() {
        let h = setup();
        quorum_of(&h, 5, 25_000);

        for round in 1..=5u64 {
            for node in 0..3 {
                h.submit(node, TRUE_PRICE, round);
            }
            for node in 3..5 {
                h.submit(node, LIE, round);
            }
            h.advance(61);
        }

        // The two liars are jailed and can no longer submit at all. A network
        // that needed them would now be stuck; this one is not.
        quorum_of(&h, 3, 15_000);
        for round in 6..=9u64 {
            for node in 0..3 {
                h.submit(node, TRUE_PRICE, round);
            }
            assert_eq!(h.price().unwrap().price, TRUE_PRICE);
            h.advance(61);
        }
        assert_eq!(h.price().unwrap().round_id, 9);
    }

    #[test]
    fn a_swarm_of_fresh_identities_cannot_outvote_proven_nodes() {
        let h = setup();
        for node in 5..8 {
            h.register_node(node);
        }
        // Three nodes that have earned full weight, against five that bonded
        // stake this morning. Thirty thousand basis points against
        // twenty-five thousand: the arithmetic is the defence, not a policy.
        for node in 0..3 {
            h.promote(node);
        }
        quorum_of(&h, 8, 55_000);

        for node in 0..3 {
            h.submit(node, TRUE_PRICE, 1);
        }
        for node in 3..8 {
            h.submit(node, LIE, 1);
        }

        let price = h.price().expect("published");
        assert_eq!(
            price.price, TRUE_PRICE,
            "five fresh identities outvoted three proven ones"
        );

        // And the swarm paid for it: five stakes bonded, five reputations
        // spent, nothing moved.
        for node in 3..8 {
            let record = h.registry.get_node(&h.pubkey(node)).unwrap();
            assert_eq!(record.reputation, STARTING_REPUTATION - 500);
            assert!(record.total_slashed > 0);
        }
    }

    #[test]
    fn recovery_is_slower_than_defection() {
        let h = setup();
        quorum_of(&h, 3, 15_000);
        h.promote(0);

        let before = h.registry.get_node(&h.pubkey(0)).unwrap().reputation;

        // One opportunistic round after a long stretch of honest ones.
        h.submit(0, LIE, 1);
        h.submit(1, TRUE_PRICE, 1);
        h.submit(2, TRUE_PRICE, 1);
        let after = h.registry.get_node(&h.pubkey(0)).unwrap().reputation;
        assert_eq!(after, before - 500);

        // Ten honest rounds to undo one dishonest one. That ratio is what
        // makes "behave, then defect at the profitable moment" a losing
        // strategy rather than a clever one.
        let mut rounds = 0;
        let mut nonce = 1u64;
        while h.registry.get_node(&h.pubkey(0)).unwrap().reputation < before {
            h.advance(61);
            nonce += 1;
            for node in 0..3 {
                h.submit(node, TRUE_PRICE, nonce);
            }
            rounds += 1;
            assert!(
                rounds <= 20,
                "recovery should take ten rounds, not {rounds}"
            );
        }
        assert_eq!(rounds, 10);
    }

    #[test]
    fn a_liar_that_stays_inside_the_band_earns_nothing_by_it() {
        let h = setup();
        quorum_of(&h, 3, 15_000);

        // A node shading the price by 4% -- inside the 5% band, so it is not
        // penalised, and it is counted. It still does not move the median,
        // because the median does not care how far a minority is from it.
        h.submit(0, TRUE_PRICE, 1);
        h.submit(1, TRUE_PRICE, 1);
        h.submit(2, TRUE_PRICE * 104 / 100, 1);

        let price = h.price().expect("published");
        assert_eq!(price.price, TRUE_PRICE);
        assert_eq!(price.num_nodes, 3, "it was in band, so it counted");
        assert!(
            price.confidence_bps >= 400,
            "but the disagreement it introduced is published, not hidden"
        );
    }

    #[test]
    fn a_price_the_whole_network_reports_wrongly_is_published_wrongly() {
        let h = setup();
        quorum_of(&h, 3, 15_000);

        // Stated rather than defended: Aphelion is Byzantine-fault-tolerant,
        // not omniscient. If every node reads the same manipulated venue, the
        // median of their agreement is that manipulated price. The defence
        // against this is inside the node -- several venues, outlier filtering
        // -- and in the number of independent operators, not on chain.
        for node in 0..3 {
            h.submit(node, LIE, 1);
        }
        assert_eq!(h.price().unwrap().price, LIE);
        for node in 0..3 {
            assert!(h.registry.get_node(&h.pubkey(node)).unwrap().reputation > STARTING_REPUTATION);
        }
    }
}
