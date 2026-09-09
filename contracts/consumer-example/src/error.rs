use soroban_sdk::contracterror;

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum VaultError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfig = 3,
    InvalidAmount = 4,

    /// The position does not exist, or has nothing in it.
    NoPosition = 10,
    /// The withdrawal or borrow would leave the position under-collateralised.
    Undercollateralized = 11,
    /// More collateral was withdrawn, or more debt repaid, than exists.
    ExceedsPosition = 12,
    /// The vault does not hold enough of the debt asset to lend.
    InsufficientLiquidity = 13,

    /// The position is healthy; there is nothing to liquidate.
    NotLiquidatable = 20,
    /// A liquidation may not clear more than the position owes.
    RepayExceedsDebt = 21,
    /// The seizure would take more collateral than the position holds.
    SeizureExceedsCollateral = 22,

    /// Valuation overflowed. For realistic prices and balances this means the
    /// inputs are not realistic.
    MathOverflow = 30,
    /// The configured decimals are outside the range this contract can scale.
    UnsupportedDecimals = 31,
}
