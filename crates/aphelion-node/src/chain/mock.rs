//! In-memory `ChainClient` for tests, `--dry-run`, and multi-node simulation.
//!
//! It enforces the invariants the real aggregator enforces — monotonic nonces,
//! a staleness window, quorum, and a reputation-weighted median across nodes —
//! so a round loop that passes against this mock is exercising the same control
//! flow it will hit on chain. A mock that accepted everything would only prove
//! the node can build a transaction.
//!
//! The median is computed with `aphelion_core::math::weighted_median`, the same
//! function the node uses to predict a round and a deliberate mirror of the
//! contract's. That is the point: a simulation whose consensus rule differs
//! from the real one tells an operator nothing about what will happen on chain.
//!
//! By default it behaves as a single-node network, which is what `--dry-run`
//! wants. [`MockChain::with_quorum`] and [`MockChain::with_registered`] turn it
//! into a network of several, for tests that need one.

use std::collections::HashMap;
use std::sync::Mutex;

use aphelion_core::{deviation_bps, weighted_median, FeedId, Price, WeightedSample};
use async_trait::async_trait;

use super::{ChainClient, OnChainNode, OnChainPrice, SubmitReceipt};
use crate::error::{NodeError, Result};
use crate::signer::SignedSubmission;

/// One node's vote in an open round.
#[derive(Clone)]
struct Vote {
    public_key_hex: String,
    price: Price,
    timestamp: u64,
    confidence_bps: u32,
    weight_bps: u32,
}

#[derive(Default)]
struct State {
    prices: HashMap<String, OnChainPrice>,
    nonces: HashMap<(String, String), u64>,
    submissions: Vec<(String, SignedSubmission)>,
    /// Open rounds, keyed by feed.
    rounds: HashMap<String, Vec<Vote>>,
    round_counter: u64,
    /// Registered nodes and their weight. Empty means "accept anyone at full
    /// weight", which is what a single-node dry run needs.
    weights: HashMap<String, u32>,
    node: Option<OnChainNode>,
    failure: Option<String>,
}

pub struct MockChain {
    state: Mutex<State>,
    ledger_time: Mutex<u64>,
    /// Matches the aggregator's `max_staleness` so timestamp handling is
    /// exercised rather than assumed.
    max_staleness: u64,
    /// Distinct nodes required to close a round.
    quorum: usize,
    /// Deviation from the round median beyond which a submission is an outlier
    /// and is excluded from the published statistics.
    max_deviation_bps: u32,
}

impl MockChain {
    pub fn new(ledger_time: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            ledger_time: Mutex::new(ledger_time),
            max_staleness: 300,
            quorum: 1,
            max_deviation_bps: 500,
        }
    }

    pub fn with_node(self, node: OnChainNode) -> Self {
        self.state.lock().unwrap().node = Some(node);
        self
    }

    /// Require `n` distinct nodes before a round publishes.
    pub fn with_quorum(mut self, n: usize) -> Self {
        self.quorum = n.max(1);
        self
    }

    /// Register a node at a given voting weight.
    ///
    /// Once any node is registered, unregistered keys are rejected — the same
    /// way the aggregator rejects a key the registry does not know.
    pub fn with_registered(self, public_key_hex: &str, weight_bps: u32) -> Self {
        self.state
            .lock()
            .unwrap()
            .weights
            .insert(public_key_hex.to_string(), weight_bps);
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

    /// How many nodes have submitted to the open round for a feed.
    pub fn pending(&self, feed: &FeedId) -> usize {
        self.state
            .lock()
            .unwrap()
            .rounds
            .get(feed.as_str())
            .map(|r| r.len())
            .unwrap_or(0)
    }

    /// Close a round: weighted median across every vote, statistics from the
    /// in-band ones only. Mirrors `Aggregator::finalize`.
    fn finalize(state: &mut State, feed: &FeedId, votes: &[Vote], max_deviation_bps: u32) {
        let mut samples: Vec<WeightedSample> = votes
            .iter()
            .map(|v| WeightedSample::new(v.price.raw(), v.weight_bps))
            .collect();
        let Some(median) = weighted_median(&mut samples) else {
            return;
        };

        let in_band: Vec<&Vote> = votes
            .iter()
            .filter(|v| deviation_bps(v.price.raw(), median) <= max_deviation_bps)
            .collect();
        if in_band.is_empty() {
            // The network did not agree. Publishing nothing is the honest
            // outcome, and the contract does the same.
            return;
        }

        state.round_counter += 1;
        state.prices.insert(
            feed.to_string(),
            OnChainPrice {
                feed: feed.clone(),
                price: Price::from_raw(median),
                // The oldest contributing observation, so a consumer's
                // freshness check cannot be satisfied by one fast node.
                timestamp: in_band.iter().map(|v| v.timestamp).min().unwrap_or(0),
                num_nodes: in_band.len() as u32,
                confidence_bps: in_band
                    .iter()
                    .map(|v| v.confidence_bps)
                    .max()
                    .unwrap_or(0)
                    .max(
                        in_band
                            .iter()
                            .map(|v| deviation_bps(v.price.raw(), median))
                            .max()
                            .unwrap_or(0),
                    ),
                round_id: state.round_counter,
            },
        );
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

        // The signature is what carries authority, so the mock checks it the
        // way the contract does. Without this, a simulation would happily
        // accept a submission relayed under someone else's key -- the exact
        // thing the signed payload exists to prevent.
        match verifying_key(public_key_hex) {
            Some(key) if submission.verify(&key) => {}
            Some(_) => return Err(NodeError::Chain("BadSignature".into())),
            None => return Err(NodeError::Chain("MalformedPublicKey".into())),
        }

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
        // Weight is read now and stored with the vote, so a reputation change
        // between here and finalisation cannot re-weight a vote already cast.
        let weight_bps = if state.weights.is_empty() {
            10_000
        } else {
            match state.weights.get(public_key_hex) {
                Some(&w) if w > 0 => w,
                _ => return Err(NodeError::Chain("NotAuthorizedNode".into())),
            }
        };

        let round = state.rounds.entry(m.feed.to_string()).or_default();
        if round.iter().any(|v| v.public_key_hex == public_key_hex) {
            return Err(NodeError::Chain("DuplicateSubmission".into()));
        }

        state.nonces.insert(key, m.nonce);
        state
            .submissions
            .push((public_key_hex.to_string(), submission.clone()));

        let round = state.rounds.entry(m.feed.to_string()).or_default();
        round.push(Vote {
            public_key_hex: public_key_hex.to_string(),
            price: m.price,
            timestamp: m.timestamp,
            confidence_bps: m.confidence_bps,
            weight_bps,
        });

        let finalized = round.len() >= self.quorum;
        if finalized {
            let votes = state.rounds.remove(m.feed.as_str()).unwrap_or_default();
            Self::finalize(&mut state, &m.feed, &votes, self.max_deviation_bps);
        }

        Ok(SubmitReceipt {
            tx_hash: Some(format!("{:064x}", state.submissions.len())),
            finalized_round: finalized,
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

/// Decode a hex public key, or `None` if it is not one.
fn verifying_key(public_key_hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(public_key_hex).ok()?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()
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
