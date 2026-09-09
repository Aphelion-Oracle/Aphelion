#![no_std]
//! # Aphelion consumer example: a collateralised vault
//!
//! A reference for dApp authors. It is deliberately the smallest thing that
//! reads an oracle the way a lending protocol has to, and it is not a lending
//! protocol: there is no interest, no reserve factor, no per-asset risk
//! configuration, and one borrower's liquidity is every borrower's liquidity.
//! What it does show is the part that gets protocols drained.
//!
//! ## Value collateral pessimistically to lend, optimistically to seize
//!
//! The same collateral is worth two different numbers here depending on which
//! way being wrong hurts.
//!
//! When someone borrows or withdraws, the vault takes the **lower** of spot
//! and TWAP and then subtracts the network's own confidence half-width. If
//! the price turns out to have been optimistic, the vault has lent too much
//! against too little, and the shortfall is the vault's.
//!
//! When someone tries to liquidate, the vault takes the **higher** of spot and
//! TWAP and adds the confidence half-width. If the price turns out to have
//! been pessimistic, a solvent borrower has just had their collateral sold. A
//! momentary dip on one venue must not be able to do that, which is the whole
//! reason the TWAP is consulted at all.
//!
//! Both directions use the same two inputs. The asymmetry is entirely in which
//! error the vault is willing to make.
//!
//! ## Read staleness in the same call as the value
//!
//! Every read goes through `get_price_checked`, never `get_price` followed by
//! a manual age comparison. The freshness requirement belongs in the call that
//! produces the value, because a second check on a separate line is a check
//! that eventually gets forgotten on some other code path.
//!
//! ## Treat `confidence_bps` as data, not decoration
//!
//! The network widens its confidence interval when its own sources disagree.
//! A consumer that ignores it is discarding the one warning the oracle can
//! give that it is less certain than usual, and doing so precisely when
//! markets are disorderly.

mod error;
mod types;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test;

pub use error::VaultError;
pub use types::*;

use soroban_sdk::{
    contract, contractclient, contractimpl, contracttype, panic_with_error, token, Address, Env,
    Symbol,
};

/// The aggregator's read interface.
///
/// A production integration would generate this from the deployed wasm with
/// `soroban_sdk::contractimport!`. It is written out here so the example
/// builds from a clean checkout without a build artefact of another contract
/// on disk; the shape is identical either way.
#[contractclient(name = "OracleClient")]
pub trait OracleInterface {
    fn get_price_checked(env: Env, feed: Symbol, max_age: u64) -> PriceData;
    fn get_twap(env: Env, feed: Symbol, window: u64) -> i128;
}

/// Mirror of `aphelion_aggregator::PriceData`.
#[contracttype]
#[derive(Clone)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
    pub num_nodes: u32,
    pub confidence_bps: u32,
    pub deviation: i128,
    pub round_id: u64,
    pub published_at: u64,
}

/// Prices are scaled by 1e8. See `aphelion-core::price`.
const PRICE_SCALE: i128 = 100_000_000;
const BPS: i128 = 10_000;

#[contract]
pub struct Vault;

#[contractimpl]
impl Vault {
    pub fn initialize(env: Env, config: Config) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, VaultError::AlreadyInitialized);
        }
        config.admin.require_auth();
        Self::validate(&env, &config);
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Liquidity, &0i128);
    }

    pub fn set_config(env: Env, config: Config) {
        let current = Self::config(&env);
        current.admin.require_auth();
        Self::validate(&env, &config);
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn get_config(env: Env) -> Config {
        Self::config(&env)
    }

    /// Supply debt-token liquidity for borrowers to draw on.
    pub fn fund(env: Env, from: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        from.require_auth();
        let config = Self::config(&env);
        token::Client::new(&env, &config.debt_token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let liquidity = Self::liquidity(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::Liquidity, &(liquidity + amount));
    }

    pub fn liquidity(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::Liquidity)
            .unwrap_or(0)
    }

    // -- borrower actions ---------------------------------------------------

    pub fn deposit_collateral(env: Env, user: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        user.require_auth();
        let config = Self::config(&env);

        token::Client::new(&env, &config.collateral_token).transfer(
            &user,
            &env.current_contract_address(),
            &amount,
        );

        let mut position = Self::position(env.clone(), user.clone());
        position.collateral += amount;
        Self::save_position(&env, &user, &position);
    }

    /// Draw debt against deposited collateral.
    pub fn borrow(env: Env, user: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        user.require_auth();
        let config = Self::config(&env);

        let liquidity = Self::liquidity(env.clone());
        if liquidity < amount {
            panic_with_error!(&env, VaultError::InsufficientLiquidity);
        }

        let mut position = Self::position(env.clone(), user.clone());
        position.debt += amount;
        // Checked against the pessimistic valuation, and against the *new*
        // debt: a check on the old balance is a check on a position that no
        // longer exists.
        Self::require_healthy_to_borrow(&env, &config, &position);

        env.storage()
            .instance()
            .set(&DataKey::Liquidity, &(liquidity - amount));
        token::Client::new(&env, &config.debt_token).transfer(
            &env.current_contract_address(),
            &user,
            &amount,
        );
        Self::save_position(&env, &user, &position);
    }

    pub fn repay(env: Env, user: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        user.require_auth();
        let config = Self::config(&env);

        let mut position = Self::position(env.clone(), user.clone());
        if amount > position.debt {
            panic_with_error!(&env, VaultError::ExceedsPosition);
        }

        token::Client::new(&env, &config.debt_token).transfer(
            &user,
            &env.current_contract_address(),
            &amount,
        );
        position.debt -= amount;
        let liquidity = Self::liquidity(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::Liquidity, &(liquidity + amount));
        Self::save_position(&env, &user, &position);
    }

    pub fn withdraw_collateral(env: Env, user: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        user.require_auth();
        let config = Self::config(&env);

        let mut position = Self::position(env.clone(), user.clone());
        if amount > position.collateral {
            panic_with_error!(&env, VaultError::ExceedsPosition);
        }
        position.collateral -= amount;
        Self::require_healthy_to_borrow(&env, &config, &position);

        token::Client::new(&env, &config.collateral_token).transfer(
            &env.current_contract_address(),
            &user,
            &amount,
        );
        Self::save_position(&env, &user, &position);
    }

    /// Repay part of an unhealthy position's debt and seize collateral for it,
    /// plus the liquidation bonus.
    pub fn liquidate(env: Env, liquidator: Address, user: Address, repay_amount: i128) -> i128 {
        if repay_amount <= 0 {
            panic_with_error!(&env, VaultError::InvalidAmount);
        }
        liquidator.require_auth();
        let config = Self::config(&env);

        let mut position = Self::position(env.clone(), user.clone());
        if position.debt == 0 {
            panic_with_error!(&env, VaultError::NoPosition);
        }
        if repay_amount > position.debt {
            panic_with_error!(&env, VaultError::RepayExceedsDebt);
        }

        // Liquidation is judged on the valuation most favourable to the
        // borrower. A dip that only the spot price saw, or that the network is
        // itself unsure about, must not be enough to sell someone's position.
        let price = Self::favourable_price(&env, &config);
        let value = Self::collateral_in_debt_units(&env, &config, position.collateral, price);
        if !Self::below_threshold(&env, value, position.debt, config.liquidation_bps) {
            panic_with_error!(&env, VaultError::NotLiquidatable);
        }

        // Seize what was repaid, plus the bonus, valued at that same price.
        let seized_value = repay_amount
            .checked_mul(BPS + config.liquidation_bonus_bps as i128)
            .and_then(|v| v.checked_div(BPS))
            .unwrap_or_else(|| panic_with_error!(&env, VaultError::MathOverflow));
        let seized = Self::debt_units_in_collateral(&env, &config, seized_value, price);
        if seized > position.collateral {
            panic_with_error!(&env, VaultError::SeizureExceedsCollateral);
        }

        let token_client = token::Client::new(&env, &config.debt_token);
        token_client.transfer(&liquidator, &env.current_contract_address(), &repay_amount);
        token::Client::new(&env, &config.collateral_token).transfer(
            &env.current_contract_address(),
            &liquidator,
            &seized,
        );

        position.debt -= repay_amount;
        position.collateral -= seized;
        let liquidity = Self::liquidity(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::Liquidity, &(liquidity + repay_amount));
        Self::save_position(&env, &user, &position);

        seized
    }

    // -- reads --------------------------------------------------------------

    pub fn position(env: Env, user: Address) -> Position {
        env.storage()
            .persistent()
            .get(&DataKey::Position(user))
            .unwrap_or(Position {
                collateral: 0,
                debt: 0,
            })
    }

    /// Collateralisation in basis points, on the pessimistic valuation.
    /// `u32::MAX` for a position with no debt.
    pub fn health_bps(env: Env, user: Address) -> u32 {
        let config = Self::config(&env);
        let position = Self::position(env.clone(), user);
        if position.debt <= 0 {
            return u32::MAX;
        }
        let price = Self::conservative_price(&env, &config);
        let value = Self::collateral_in_debt_units(&env, &config, position.collateral, price);
        let bps = value.saturating_mul(BPS) / position.debt;
        if bps > u32::MAX as i128 {
            u32::MAX
        } else {
            bps as u32
        }
    }

    /// What one whole unit of collateral is worth to a borrower right now, in
    /// 1e8-scaled USD. Exposed because a front end that shows a different
    /// number from the one the contract enforces is a support ticket.
    pub fn conservative_unit_price(env: Env) -> i128 {
        let config = Self::config(&env);
        Self::conservative_price(&env, &config)
    }

    /// The valuation a liquidation is judged against.
    pub fn favourable_unit_price(env: Env) -> i128 {
        let config = Self::config(&env);
        Self::favourable_price(&env, &config)
    }

    // -- valuation ----------------------------------------------------------

    /// The lower of spot and TWAP, narrowed further by the network's own
    /// confidence half-width.
    fn conservative_price(env: &Env, config: &Config) -> i128 {
        let (spot, twap) = Self::spot_and_twap(env, config);
        let base = if spot.price < twap { spot.price } else { twap };
        let haircut = base
            .checked_mul(spot.confidence_bps as i128)
            .and_then(|v| v.checked_div(BPS))
            .unwrap_or_else(|| panic_with_error!(env, VaultError::MathOverflow));
        base - haircut
    }

    /// The higher of spot and TWAP, widened by the confidence half-width.
    fn favourable_price(env: &Env, config: &Config) -> i128 {
        let (spot, twap) = Self::spot_and_twap(env, config);
        let base = if spot.price > twap { spot.price } else { twap };
        let margin = base
            .checked_mul(spot.confidence_bps as i128)
            .and_then(|v| v.checked_div(BPS))
            .unwrap_or_else(|| panic_with_error!(env, VaultError::MathOverflow));
        base + margin
    }

    /// Both readings, from one oracle, in one place.
    ///
    /// `get_price_checked` reverts on a stale feed, so every path through this
    /// contract inherits the staleness requirement without restating it.
    fn spot_and_twap(env: &Env, config: &Config) -> (PriceData, i128) {
        let oracle = OracleClient::new(env, &config.oracle);
        let spot = oracle.get_price_checked(&config.feed, &config.max_price_age);
        let twap = oracle.get_twap(&config.feed, &config.twap_window);
        (spot, twap)
    }

    /// Value `collateral` (collateral-token units) in debt-token units at
    /// `price` (1e8-scaled USD per whole collateral unit).
    fn collateral_in_debt_units(
        env: &Env,
        config: &Config,
        collateral: i128,
        price: i128,
    ) -> i128 {
        let col_scale = Self::pow10(env, config.collateral_decimals);
        let debt_scale = Self::pow10(env, config.debt_decimals);
        collateral
            .checked_mul(price)
            .and_then(|v| v.checked_mul(debt_scale))
            .and_then(|v| v.checked_div(col_scale))
            .and_then(|v| v.checked_div(PRICE_SCALE))
            .unwrap_or_else(|| panic_with_error!(env, VaultError::MathOverflow))
    }

    /// The inverse of [`Self::collateral_in_debt_units`].
    fn debt_units_in_collateral(env: &Env, config: &Config, debt: i128, price: i128) -> i128 {
        if price <= 0 {
            panic_with_error!(env, VaultError::MathOverflow);
        }
        let col_scale = Self::pow10(env, config.collateral_decimals);
        let debt_scale = Self::pow10(env, config.debt_decimals);
        debt.checked_mul(col_scale)
            .and_then(|v| v.checked_mul(PRICE_SCALE))
            .and_then(|v| v.checked_div(debt_scale))
            .and_then(|v| v.checked_div(price))
            .unwrap_or_else(|| panic_with_error!(env, VaultError::MathOverflow))
    }

    fn below_threshold(env: &Env, value: i128, debt: i128, threshold_bps: u32) -> bool {
        if debt <= 0 {
            return false;
        }
        let required = debt
            .checked_mul(threshold_bps as i128)
            .and_then(|v| v.checked_div(BPS))
            .unwrap_or_else(|| panic_with_error!(env, VaultError::MathOverflow));
        value < required
    }

    fn require_healthy_to_borrow(env: &Env, config: &Config, position: &Position) {
        if position.debt <= 0 {
            return;
        }
        let price = Self::conservative_price(env, config);
        let value = Self::collateral_in_debt_units(env, config, position.collateral, price);
        if Self::below_threshold(env, value, position.debt, config.min_collateral_bps) {
            panic_with_error!(env, VaultError::Undercollateralized);
        }
    }

    // -- internals ----------------------------------------------------------

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, VaultError::NotInitialized))
    }

    fn save_position(env: &Env, user: &Address, position: &Position) {
        let key = DataKey::Position(user.clone());
        env.storage().persistent().set(&key, position);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn validate(env: &Env, config: &Config) {
        // A liquidation threshold at or above the borrowing requirement would
        // make every new position liquidatable the moment it opened.
        let sane = config.min_collateral_bps > config.liquidation_bps
            && config.liquidation_bps > BPS as u32
            && config.liquidation_bonus_bps < BPS as u32
            && config.max_price_age > 0
            && config.twap_window > 0;
        if !sane {
            panic_with_error!(env, VaultError::InvalidConfig);
        }
        if config.collateral_decimals > 18 || config.debt_decimals > 18 {
            panic_with_error!(env, VaultError::UnsupportedDecimals);
        }
    }

    fn pow10(env: &Env, decimals: u32) -> i128 {
        if decimals > 18 {
            panic_with_error!(env, VaultError::UnsupportedDecimals);
        }
        let mut out: i128 = 1;
        for _ in 0..decimals {
            out *= 10;
        }
        out
    }
}
