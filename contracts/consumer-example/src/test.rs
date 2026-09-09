#![cfg(test)]
//! End-to-end: a real registry, a real aggregator fed by real signed
//! submissions, and this vault reading the result.
//!
//! Deliberately not a mock oracle. The interesting failures in an oracle
//! integration are the ones where the consumer's assumptions and the
//! aggregator's behaviour drift apart -- a renamed field, a confidence value
//! that widens when nobody expected it to, a TWAP that refuses a window. A
//! mock agrees with whatever the consumer already believes, which is exactly
//! the wrong property for this test to have.

use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{Address, BytesN, Env, Symbol};

use crate::{Config, Vault, VaultClient};
use aphelion_aggregator::{Aggregator, AggregatorClient, Config as OracleConfig};
use aphelion_registry::{Registry, RegistryClient};

const BASE_TIME: u64 = 1_735_689_600;
const MIN_STAKE: i128 = 1_000_0000000;

/// $0.40 per XLM, 1e8-scaled.
const START_PRICE: i128 = 40_000_000;
/// Collateral is quoted in stroops (7 dp), debt in a 6 dp stablecoin. The two
/// differ on purpose: a vault that only works when both sides share a scale is
/// a vault that breaks on its second asset.
const COLLATERAL_DECIMALS: u32 = 7;
const DEBT_DECIMALS: u32 = 6;

/// 10_000 XLM.
const COLLATERAL: i128 = 10_000 * 10_000_000;
/// $2_000 of debt, in 6 dp units.
const BORROWED: i128 = 2_000_000_000;

const TWAP_WINDOW: u64 = 120;
const MAX_PRICE_AGE: u64 = 300;
const LIQUIDITY: i128 = 10_000_000_000;

fn feed() -> &'static str {
    "XLM_USD"
}

struct Harness<'a> {
    env: Env,
    vault: VaultClient<'a>,
    aggregator: AggregatorClient<'a>,
    collateral: TokenClient<'a>,
    debt: TokenClient<'a>,
    borrower: Address,
    liquidator: Address,
    aggregator_id: [u8; 32],
    keys: std::vec::Vec<SigningKey>,
}

/// The raw 32-byte contract id, derived here rather than borrowed from the
/// aggregator's internals: if the two ways of deriving it disagree, no
/// signature in this file verifies.
fn contract_id(env: &Env, address: &Address) -> [u8; 32] {
    let xdr = address.clone().to_xdr(env);
    let mut out = [0u8; 32];
    xdr.slice(8..40).copy_into_slice(&mut out);
    out
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(BASE_TIME);

    let admin = Address::generate(&env);
    let collateral_sac = env.register_stellar_asset_contract_v2(admin.clone());
    let debt_sac = env.register_stellar_asset_contract_v2(admin.clone());
    let collateral_address = collateral_sac.address();
    let debt_address = debt_sac.address();

    let registry_id = env.register(Registry, ());
    let aggregator_address = env.register(Aggregator, ());
    let vault_id = env.register(Vault, ());

    let registry = RegistryClient::new(&env, &registry_id);
    registry.initialize(
        &admin,
        &aggregator_address,
        &admin,
        &collateral_address,
        &MIN_STAKE,
        &(7 * 24 * 3600),
    );

    let aggregator = AggregatorClient::new(&env, &aggregator_address);
    aggregator.initialize(&OracleConfig {
        admin: admin.clone(),
        registry: registry_id,
        token: collateral_address.clone(),
        quorum: 3,
        min_weight_bps: 15_000,
        max_deviation_bps: 500,
        max_staleness: 300,
        max_future_drift: 30,
        min_round_interval: 60,
        round_timeout: 120,
        absence_threshold: 600,
        reward_per_submission: 0,
        outlier_rep_penalty: 500,
        outlier_slash: 0,
        history_len: 20,
        read_fee: 0,
    });
    aggregator.set_feed(&Symbol::new(&env, feed()), &true, &300, &0);

    let vault = VaultClient::new(&env, &vault_id);
    vault.initialize(&Config {
        admin: admin.clone(),
        oracle: aggregator_address.clone(),
        feed: Symbol::new(&env, feed()),
        collateral_token: collateral_address.clone(),
        collateral_decimals: COLLATERAL_DECIMALS,
        debt_token: debt_address.clone(),
        debt_decimals: DEBT_DECIMALS,
        max_price_age: MAX_PRICE_AGE,
        twap_window: TWAP_WINDOW,
        min_collateral_bps: 15_000,
        liquidation_bps: 12_000,
        liquidation_bonus_bps: 1_000,
    });

    let collateral_admin = StellarAssetClient::new(&env, &collateral_address);
    let debt_admin = StellarAssetClient::new(&env, &debt_address);

    let keys: std::vec::Vec<SigningKey> =
        (1u8..=4).map(|i| SigningKey::from_bytes(&[i; 32])).collect();
    for key in &keys {
        let owner = Address::generate(&env);
        collateral_admin.mint(&owner, &(MIN_STAKE * 3));
        registry.register(
            &owner,
            &BytesN::from_array(&env, &key.verifying_key().to_bytes()),
            &(MIN_STAKE * 2),
        );
    }

    let borrower = Address::generate(&env);
    collateral_admin.mint(&borrower, &(COLLATERAL * 2));
    let liquidator = Address::generate(&env);
    debt_admin.mint(&liquidator, &LIQUIDITY);
    let supplier = Address::generate(&env);
    debt_admin.mint(&supplier, &LIQUIDITY);

    let aggregator_id = contract_id(&env, &aggregator_address);
    let h = Harness {
        collateral: TokenClient::new(&env, &collateral_address),
        debt: TokenClient::new(&env, &debt_address),
        env,
        vault,
        aggregator,
        borrower,
        liquidator,
        aggregator_id,
        keys,
    };
    h.vault.fund(&supplier, &LIQUIDITY);
    h
}

impl Harness<'_> {
    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn message(
        &self,
        price: i128,
        timestamp: u64,
        conf: u32,
        nonce: u64,
    ) -> std::vec::Vec<u8> {
        let mut buf = std::vec::Vec::with_capacity(117);
        buf.extend_from_slice(b"APHELION_PRICE_V1");
        buf.extend_from_slice(&self.aggregator_id);
        let mut padded = [0u8; 32];
        padded[..feed().len()].copy_from_slice(feed().as_bytes());
        buf.extend_from_slice(&padded);
        buf.extend_from_slice(&price.to_be_bytes());
        buf.extend_from_slice(&timestamp.to_be_bytes());
        buf.extend_from_slice(&conf.to_be_bytes());
        buf.extend_from_slice(&nonce.to_be_bytes());
        buf
    }

    fn submit(&self, node: usize, price: i128, conf: u32, nonce: u64) {
        let now = self.env.ledger().timestamp();
        let key = &self.keys[node];
        let payload = self.message(price, now, conf, nonce);
        let signature = key.sign(&payload).to_bytes();
        self.aggregator.submit_price(
            &Symbol::new(&self.env, feed()),
            &BytesN::from_array(&self.env, &key.verifying_key().to_bytes()),
            &price,
            &now,
            &conf,
            &nonce,
            &BytesN::from_array(&self.env, &signature),
        );
    }

    /// One round: three nodes agreeing on `price`.
    fn publish(&self, price: i128, nonce: u64) {
        for node in 0..3 {
            self.submit(node, price, 25, nonce);
        }
    }

    /// `rounds` consecutive publications at `price`, one per minute, leaving
    /// the ledger just after the last one.
    fn publish_run(&self, price: i128, rounds: u64, first_nonce: u64) {
        for r in 0..rounds {
            self.publish(price, first_nonce + r);
            self.advance(61);
        }
    }

    /// A deposited, borrowed-against position on a settled $0.40 price.
    fn open_position(&self) -> u64 {
        self.publish_run(START_PRICE, 6, 1);
        self.vault.deposit_collateral(&self.borrower, &COLLATERAL);
        self.vault.borrow(&self.borrower, &BORROWED);
        7
    }

    fn health(&self) -> u32 {
        self.vault.health_bps(&self.borrower)
    }
}

// -- valuation --------------------------------------------------------------

#[test]
fn a_position_is_valued_from_the_live_network_price() {
    let h = setup();
    let next = h.open_position();
    let _ = next;

    // 10_000 XLM at $0.40 is $4_000, less the network's 25 bps of confidence.
    assert_eq!(h.vault.conservative_unit_price(), 39_900_000);
    assert_eq!(
        h.health(),
        19_950,
        "$3_990 of collateral against $2_000 of debt"
    );
    assert_eq!(h.debt.balance(&h.borrower), BORROWED);
    assert_eq!(h.collateral.balance(&h.vault.address), COLLATERAL);
}

#[test]
fn the_two_valuations_bracket_the_network_price() {
    let h = setup();
    h.publish_run(START_PRICE, 6, 1);

    let conservative = h.vault.conservative_unit_price();
    let favourable = h.vault.favourable_unit_price();

    assert!(conservative < START_PRICE);
    assert!(favourable > START_PRICE);
    // The same two inputs, read in whichever direction being wrong hurts.
    assert_eq!(favourable - START_PRICE, START_PRICE - conservative);
}

#[test]
fn disagreement_between_nodes_narrows_what_can_be_borrowed() {
    let h = setup();
    h.publish_run(START_PRICE, 6, 1);
    let agreed = h.vault.conservative_unit_price();

    // The same price, but the nodes now differ by 2%: the network says so, and
    // the vault lends less against it.
    h.submit(0, START_PRICE, 25, 7);
    h.submit(1, START_PRICE * 102 / 100, 25, 7);
    h.submit(2, START_PRICE * 98 / 100, 25, 7);

    let disputed = h.vault.conservative_unit_price();
    assert!(
        disputed < agreed,
        "a widened confidence interval must reach the borrowing limit: {disputed} vs {agreed}"
    );
}

// -- borrowing --------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // Undercollateralized
fn borrowing_past_the_collateral_ratio_is_refused() {
    let h = setup();
    h.publish_run(START_PRICE, 6, 1);
    h.vault.deposit_collateral(&h.borrower, &COLLATERAL);
    // $3_990 of collateral at 150% supports $2_660. This asks for $2_700.
    h.vault.borrow(&h.borrower, &2_700_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")] // InsufficientLiquidity
fn the_vault_cannot_lend_what_it_does_not_hold() {
    let h = setup();
    h.publish_run(START_PRICE, 6, 1);
    h.vault
        .deposit_collateral(&h.borrower, &(COLLATERAL * 2));
    h.vault.borrow(&h.borrower, &(LIQUIDITY + 1));
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // Undercollateralized
fn collateral_backing_a_debt_cannot_be_withdrawn() {
    let h = setup();
    h.open_position();
    h.vault
        .withdraw_collateral(&h.borrower, &(COLLATERAL / 2));
}

#[test]
fn repaying_releases_the_collateral_it_was_backing() {
    let h = setup();
    h.open_position();
    h.vault.repay(&h.borrower, &BORROWED);

    assert_eq!(h.vault.position(&h.borrower).debt, 0);
    assert_eq!(h.vault.health_bps(&h.borrower), u32::MAX);

    h.vault.withdraw_collateral(&h.borrower, &COLLATERAL);
    assert_eq!(h.vault.position(&h.borrower).collateral, 0);
    assert_eq!(h.collateral.balance(&h.borrower), COLLATERAL * 2);
}

// -- staleness --------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #31)")] // aggregator's StalePrice
fn every_operation_stops_when_the_feed_goes_stale() {
    let h = setup();
    h.open_position();

    // The network stops publishing. Nothing here proceeds on a guess: the
    // staleness check lives inside the call that produces the value.
    h.advance(MAX_PRICE_AGE + 61);
    h.vault.borrow(&h.borrower, &1);
}

#[test]
#[should_panic(expected = "Error(Contract, #32)")] // aggregator's InsufficientHistory
fn a_twap_window_the_feed_cannot_cover_stops_the_vault_too() {
    let h = setup();
    // One round of history, and a vault configured to want two minutes of it.
    h.publish(START_PRICE, 1);
    h.vault.deposit_collateral(&h.borrower, &COLLATERAL);
    h.vault.borrow(&h.borrower, &BORROWED);
}

// -- liquidation ------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #20)")] // NotLiquidatable
fn a_healthy_position_cannot_be_liquidated() {
    let h = setup();
    h.open_position();
    h.vault
        .liquidate(&h.liquidator, &h.borrower, &1_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #20)")] // NotLiquidatable
fn a_single_round_crash_does_not_liquidate_a_solvent_borrower() {
    let h = setup();
    let next = h.open_position();

    // One round at half price -- a flash crash, an illiquid venue, a squeezed
    // book. Spot says $0.20; the TWAP still remembers the last two minutes.
    h.publish(START_PRICE / 2, next);
    h.advance(10);

    assert!(
        h.vault.favourable_unit_price() > 30_000_000,
        "the TWAP has to still be carrying the pre-crash price"
    );
    h.vault
        .liquidate(&h.liquidator, &h.borrower, &1_000_000);
}

#[test]
fn a_sustained_crash_makes_a_position_liquidatable() {
    let h = setup();
    let next = h.open_position();
    assert!(h.health() > 12_000);

    // The price stays at $0.20 long enough for the TWAP to agree with it.
    h.publish_run(START_PRICE / 2, 5, next);

    assert!(
        h.health() < 12_000,
        "collateral is now worth about $2_000 against $2_000 of debt"
    );

    let repaid = 500_000_000i128; // $500
    let collateral_before = h.vault.position(&h.borrower).collateral;
    let liquidator_collateral_before = h.collateral.balance(&h.liquidator);

    let seized = h.vault.liquidate(&h.liquidator, &h.borrower, &repaid);

    let position = h.vault.position(&h.borrower);
    assert_eq!(position.debt, BORROWED - repaid);
    assert_eq!(position.collateral, collateral_before - seized);
    assert_eq!(
        h.collateral.balance(&h.liquidator),
        liquidator_collateral_before + seized
    );

    // The liquidator receives 10% more collateral than they repaid, valued at
    // the same price the vault judged the position on.
    let price = h.vault.favourable_unit_price();
    let repaid_worth = repaid * 10_000_000 * 100_000_000 / 1_000_000 / price;
    assert!(
        seized > repaid_worth,
        "the bonus is what pays for doing the work: {seized} vs {repaid_worth}"
    );
    assert!(seized < repaid_worth * 111 / 100);
}

#[test]
#[should_panic(expected = "Error(Contract, #21)")] // RepayExceedsDebt
fn a_liquidator_cannot_repay_more_than_is_owed() {
    let h = setup();
    let next = h.open_position();
    h.publish_run(START_PRICE / 2, 5, next);
    h.vault
        .liquidate(&h.liquidator, &h.borrower, &(BORROWED + 1));
}

#[test]
fn a_partial_liquidation_leaves_a_smaller_position_behind() {
    let h = setup();
    let next = h.open_position();
    h.publish_run(START_PRICE / 2, 5, next);

    h.vault.liquidate(&h.liquidator, &h.borrower, &1_500_000_000);

    let position = h.vault.position(&h.borrower);
    assert_eq!(position.debt, BORROWED - 1_500_000_000);
    assert!(
        position.collateral > 0,
        "a liquidation seizes what it paid for, not the whole position"
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #22)")] // SeizureExceedsCollateral
fn a_liquidation_larger_than_the_collateral_backing_it_is_refused() {
    let h = setup();
    let next = h.open_position();
    h.publish_run(START_PRICE / 2, 5, next);

    // $2_000 of debt against roughly $2_000 of collateral: clearing the whole
    // debt would need $2_200 of collateral once the bonus is added, and it is
    // not there. A real protocol absorbs the shortfall as bad debt and caps
    // the seizure; this example stops instead, because inventing a bad-debt
    // policy is exactly the part a reader should design for themselves.
    h.vault.liquidate(&h.liquidator, &h.borrower, &BORROWED);
}

// -- configuration ----------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #3)")] // InvalidConfig
fn a_liquidation_threshold_above_the_borrowing_limit_is_refused() {
    let h = setup();
    let mut config = h.vault.get_config();
    // Otherwise every position would be liquidatable the instant it opened.
    config.liquidation_bps = config.min_collateral_bps;
    h.vault.set_config(&config);
}

#[test]
fn a_position_that_never_borrowed_is_infinitely_healthy() {
    let h = setup();
    h.publish_run(START_PRICE, 6, 1);
    h.vault.deposit_collateral(&h.borrower, &COLLATERAL);
    assert_eq!(h.health(), u32::MAX);
    h.vault.withdraw_collateral(&h.borrower, &COLLATERAL);
}
