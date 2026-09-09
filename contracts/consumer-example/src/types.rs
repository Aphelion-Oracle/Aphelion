use soroban_sdk::{contracttype, Address, Symbol};

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,
    /// The Aphelion aggregator this vault reads.
    pub oracle: Address,
    /// The feed that prices the collateral asset, in USD.
    pub feed: Symbol,

    pub collateral_token: Address,
    pub collateral_decimals: u32,
    /// The borrowed asset. Assumed to be a USD stablecoin: this example values
    /// one debt unit at one dollar rather than reading a second feed, which is
    /// the one simplification it makes and the one an adapting reader has to
    /// undo first.
    pub debt_token: Address,
    pub debt_decimals: u32,

    /// The oldest price this vault will act on. Anything older and every
    /// operation reverts rather than proceeding on a guess.
    pub max_price_age: u64,
    /// TWAP window used alongside the spot price. Anything a borrower could
    /// profitably manipulate inside a block is checked against this.
    pub twap_window: u64,

    /// Collateralisation required to borrow or withdraw, in basis points.
    /// 15_000 means a position must be 150% covered.
    pub min_collateral_bps: u32,
    /// Below this, a position may be liquidated. Strictly under
    /// `min_collateral_bps`, so that a position is not liquidatable the moment
    /// after it is opened.
    pub liquidation_bps: u32,
    /// Extra collateral a liquidator receives, in basis points of what they
    /// repaid. This is what pays for the gas and the price risk of doing the
    /// work.
    pub liquidation_bonus_bps: u32,
}

#[contracttype]
#[derive(Clone, Default)]
pub struct Position {
    /// Collateral token units held for this borrower.
    pub collateral: i128,
    /// Debt token units owed.
    pub debt: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    Position(Address),
    /// Debt-token liquidity supplied to the vault.
    Liquidity,
}

pub const TTL_THRESHOLD: u32 = 120_960;
pub const TTL_EXTEND: u32 = 518_400;
