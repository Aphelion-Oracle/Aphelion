//! Aphelion oracle node.
//!
//! ```text
//!   exchanges ──▶ sources ──▶ collector ──▶ Postgres (raw_prices)
//!                                              │
//!                                              ▼
//!                                       round scheduler
//!                                              │
//!                          median + outlier filter + confidence
//!                                              │
//!                                        Ed25519 signature
//!                                              │
//!                                              ▼
//!                                   aggregator contract (Soroban)
//! ```
//!
//! The two halves are deliberately decoupled by the database: collection runs
//! continuously and tolerates a flaky exchange, while the round loop reads a
//! consistent snapshot on a fixed cadence. A node that loses its RPC endpoint
//! keeps collecting, and catches up when the endpoint returns.

pub mod api;
pub mod chain;
pub mod config;
pub mod db;
pub mod engine;
pub mod error;
pub mod signer;
pub mod sources;
pub mod telemetry;

pub use config::Config;
pub use error::{NodeError, Result};
