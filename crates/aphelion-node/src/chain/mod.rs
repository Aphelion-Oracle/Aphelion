//! Talking to the Soroban contracts.
//!
//! Everything the node needs from the chain goes through [`ChainClient`], for
//! two reasons. The obvious one is testability: [`mock::MockChain`] lets the
//! round loop be exercised end to end with no network. The less obvious one is
//! that transaction construction and signing is the part of a Stellar
//! integration most likely to need replacing — today the shipped
//! implementation drives the `stellar` CLI, which is dependable and easy to
//! audit but costs a process spawn per submission. Swapping in a native
//! XDR-building client later is a matter of adding one more implementation of
//! this trait, with no change to the engine.

pub mod cli;
pub mod mock;
pub mod rpc;

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;
use serde::Serialize;

use crate::error::Result;
use crate::signer::SignedSubmission;

pub use cli::CliChain;
pub use mock::MockChain;
pub use rpc::RpcClient;

/// A price as the aggregator currently holds it.
#[derive(Debug, Clone, Serialize)]
pub struct OnChainPrice {
    pub feed: FeedId,
    #[serde(serialize_with = "ser_price")]
    pub price: Price,
    pub timestamp: u64,
    pub num_nodes: u32,
    pub confidence_bps: u32,
    pub round_id: u64,
}

/// This node's registry record.
#[derive(Debug, Clone, Serialize)]
pub struct OnChainNode {
    pub public_key_hex: String,
    pub stake: i128,
    pub reputation: u32,
    pub status: String,
    /// Reputation-derived voting weight in basis points, as the aggregator
    /// will apply it.
    pub weight_bps: u32,
    pub last_submission: u64,
}

/// Result of landing a submission.
#[derive(Debug, Clone, Serialize)]
pub struct SubmitReceipt {
    pub tx_hash: Option<String>,
    /// Whether this submission was the one that closed the round.
    pub finalized_round: bool,
}

#[async_trait]
pub trait ChainClient: Send + Sync {
    /// Ledger close time in unix seconds.
    ///
    /// The node's own clock is never trusted for the timestamp it signs: the
    /// contract compares against ledger time, so that is what must be tracked.
    async fn ledger_time(&self) -> Result<u64>;

    /// Relay a signed submission to the aggregator on behalf of the node
    /// identified by `public_key_hex`.
    ///
    /// The public key is passed explicitly rather than being derived from the
    /// transaction source because the two are unrelated: the signature is what
    /// authorises the price, and the transaction merely pays to carry it. That
    /// separation is deliberate — it lets several operators share one funded
    /// relayer account without sharing any signing authority.
    async fn submit_price(
        &self,
        public_key_hex: &str,
        submission: &SignedSubmission,
    ) -> Result<SubmitReceipt>;

    /// Read the aggregator's current price for a feed.
    async fn latest_price(&self, feed: &FeedId) -> Result<Option<OnChainPrice>>;

    /// Read this node's registry record.
    async fn node_info(&self, public_key_hex: &str) -> Result<Option<OnChainNode>>;

    /// The highest nonce the aggregator has accepted from this node for a
    /// feed. Used at startup to resynchronise the local nonce counter after a
    /// restore from backup.
    async fn last_nonce(&self, public_key_hex: &str, feed: &FeedId) -> Result<u64>;
}

fn ser_price<S: serde::Serializer>(p: &Price, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&p.to_string())
}
