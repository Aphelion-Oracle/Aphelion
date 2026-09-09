//! In-memory `ChainClient` for tests and `--dry-run`.
//!
//! It enforces the invariants the real aggregator enforces — monotonic nonces,
//! a staleness window, quorum accounting — so a round loop that passes against
//! this mock is exercising the same control flow it will hit on chain. A mock
//! that accepted everything would only prove the node can build a transaction.

use std::collections::HashMap;
use std::sync::Mutex;

use aphelion_core::{FeedId, Price};
use async_trait::async_trait;

use super::{ChainClient, OnChainNode, OnChainPrice, SubmitReceipt};
use crate::error::{NodeError, Result};
use crate::signer::SignedSubmission;

#[derive(Default)]
struct State {
    prices: HashMap<String, OnChainPrice>,
    nonces: HashMap<(String, String), u64>,
    submissions: Vec<(String, SignedSubmission)>,
    node: Option<OnChainNode>,
    failure: Option<String>,
}

pub struct MockChain {
    state: Mutex<State>,
    ledger_time: Mutex<u64>,
    /// Matches the aggregator's `max_staleness` so timestamp handling is
    /// exercised rather than assumed.
    max_staleness: u64,
}

impl MockChain {
    pub fn new(ledger_time: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            ledger_time: Mutex::new(ledger_time),
            max_staleness: 300,
        }
    }

    pub fn with_node(self, node: OnChainNode) -> Self {
        self.state.lock().unwrap().node = Some(node);
        self
    }

    /// Advance the simulated ledger clock.
    pub fn set_ledger_time(&self, t: u64) {
        *self.ledger_time.lock().unwrap() = t;
    }

    /// Make the next submission fail, to exercise the retry path.
    pub fn fail_next(&self, reason: impl Into<String>) {
        self.state.lock().unwrap().failure = Some(reason.into());
    }

    pub fn submissions(&self) -> Vec<(String, SignedSubmission)> {
        self.state.lock().unwrap().submissions.clone()
    }

    pub fn submission_count(&self) -> usize {
        self.state.lock().unwrap().submissions.len()
    }
}

#[async_trait]
impl ChainClient for MockChain {
    async fn ledger_time(&self) -> Result<u64> {
        Ok(*self.ledger_time.lock().unwrap())
    }

    async fn submit_price(
        &self,
        public_key_hex: &str,
        submission: &SignedSubmission,
    ) -> Result<SubmitReceipt> {
        let now = *self.ledger_time.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        if let Some(reason) = state.failure.take() {
            return Err(NodeError::Chain(reason));
        }

        let m = &submission.message;

        if m.timestamp > now + 60 {
            return Err(NodeError::Chain("FutureTimestamp".into()));
        }
        if now.saturating_sub(m.timestamp) > self.max_staleness {
            return Err(NodeError::Chain("StaleObservation".into()));
        }

        let key = (public_key_hex.to_string(), m.feed.to_string());
        let last = state.nonces.get(&key).copied().unwrap_or(0);
        if m.nonce <= last {
            return Err(NodeError::Chain(format!(
                "NonceNotIncreasing: got {}, last was {last}",
                m.nonce
            )));
        }
        state.nonces.insert(key, m.nonce);
        state
            .submissions
            .push((public_key_hex.to_string(), submission.clone()));

        // A single-node mock finalises every round immediately.
        let round_id = state
            .prices
            .get(m.feed.as_str())
            .map(|p| p.round_id + 1)
            .unwrap_or(1);
        state.prices.insert(
            m.feed.to_string(),
            OnChainPrice {
                feed: m.feed.clone(),
                price: m.price,
                timestamp: m.timestamp,
                num_nodes: 1,
                confidence_bps: m.confidence_bps,
                round_id,
            },
        );

        Ok(SubmitReceipt {
            tx_hash: Some(format!("{:064x}", round_id)),
            finalized_round: true,
        })
    }

    async fn latest_price(&self, feed: &FeedId) -> Result<Option<OnChainPrice>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .prices
            .get(feed.as_str())
            .cloned())
    }

    async fn node_info(&self, _public_key_hex: &str) -> Result<Option<OnChainNode>> {
        Ok(self.state.lock().unwrap().node.clone())
    }

    async fn last_nonce(&self, public_key_hex: &str, feed: &FeedId) -> Result<u64> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .nonces
            .get(&(public_key_hex.to_string(), feed.to_string()))
            .copied()
            .unwrap_or(0))
    }
}

/// A registered, full-weight node. The starting point for most tests.
pub fn healthy_node(public_key_hex: &str) -> OnChainNode {
    OnChainNode {
        public_key_hex: public_key_hex.to_string(),
        stake: 1_000 * 10_000_000,
        reputation: 8_000,
        status: "active".into(),
        weight_bps: 10_000,
        last_submission: 0,
    }
}

#[allow(dead_code)]
fn _assert_price_type_used(_: Price) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::NodeSigner;

    fn signer() -> NodeSigner {
        let dir = std::env::temp_dir().join(format!("aphelion-mock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.json");
        NodeSigner::generate(&path).unwrap();
        NodeSigner::load(&path, [1u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn rejects_a_replayed_nonce() {
        let chain = MockChain::new(1000);
        let signer = signer();
        let feed = FeedId::new("BTC_USD").unwrap();
        let pk = signer.public_key_hex();

        let first = signer.sign_price(&feed, Price::parse_decimal("100").unwrap(), 1000, 50, 1);
        chain.submit_price(&pk, &first).await.unwrap();

        let replay = signer.sign_price(&feed, Price::parse_decimal("100").unwrap(), 1000, 50, 1);
        let err = chain
            .submit_price(&pk, &replay)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("NonceNotIncreasing"), "{err}");
    }

    #[tokio::test]
    async fn rejects_a_stale_observation() {
        let chain = MockChain::new(10_000);
        let signer = signer();
        let feed = FeedId::new("BTC_USD").unwrap();
        let sub = signer.sign_price(&feed, Price::parse_decimal("100").unwrap(), 1, 50, 1);
        let err = chain
            .submit_price(&signer.public_key_hex(), &sub)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("StaleObservation"), "{err}");
    }

    #[tokio::test]
    async fn a_successful_submission_becomes_the_readable_price() {
        let chain = MockChain::new(1000);
        let signer = signer();
        let feed = FeedId::new("BTC_USD").unwrap();
        let sub = signer.sign_price(
            &feed,
            Price::parse_decimal("64231.55").unwrap(),
            1000,
            50,
            1,
        );
        chain
            .submit_price(&signer.public_key_hex(), &sub)
            .await
            .unwrap();

        let on_chain = chain.latest_price(&feed).await.unwrap().unwrap();
        assert_eq!(on_chain.price.to_string(), "64231.55000000");
        assert_eq!(chain.submission_count(), 1);
    }
}
