//! # aphelion-core
//!
//! Domain types shared by every off-chain component of the Aphelion oracle
//! network (the node service, the CLI, integration tests, and any third-party
//! tooling that wants to verify what a node published).
//!
//! The single most important thing in this crate is [`message`]: the canonical
//! byte layout that a node signs and that the `aggregator` Soroban contract
//! re-derives before checking the Ed25519 signature. If those two encodings
//! ever disagree, every submission is rejected on chain. They are kept honest
//! by the shared test vectors in `tests/vectors/price_message.json`, which are
//! asserted from *both* sides (see `crates/aphelion-core/src/message.rs` and
//! `contracts/aggregator/src/test_vectors.rs`).

pub mod feed;
pub mod math;
pub mod message;
pub mod price;

pub use feed::{FeedId, FeedIdError};
pub use math::{deviation_bps, mean, stddev, weighted_median, WeightedSample};
pub use message::{PriceMessage, DOMAIN_SEPARATOR, MESSAGE_LEN};
pub use price::{Price, PriceError, PRICE_DECIMALS, PRICE_SCALE};

/// Basis points denominator. 10_000 bps == 100%.
pub const BPS_DENOMINATOR: u32 = 10_000;

/// Reputation is tracked on a 0..=10_000 scale so that it can be compared with
/// basis points without a second unit in play.
pub const MAX_REPUTATION: u32 = 10_000;
