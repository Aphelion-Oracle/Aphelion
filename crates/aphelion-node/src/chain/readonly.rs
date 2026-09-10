//! A chain client that can read the chain but cannot write to it.
//!
//! Dry run needs a real view of the chain -- ledger time, this node's registry
//! record, the nonce the aggregator last accepted -- and needs to be
//! structurally unable to submit. Those two wants pull in opposite directions,
//! and substituting a mock satisfies only the second: it makes every *read*
//! fake as well, which is worse than useless to the operator dry run exists
//! for. A frozen ledger clock drifts further from the node's every second
//! until the round loop reports a clock skew that is not real and names NTP as
//! the culprit; an empty registry reports a registered node as unregistered.
//! Both are alarms about the fixture rather than about the deployment, and an
//! operator's first impression of the software should not be a fault it
//! invented.
//!
//! Wrapping the live client instead keeps the reads honest and moves the
//! guarantee to where it belongs: submission is refused here, one layer below
//! the code that decides whether to submit, so a dry run that submitted
//! anyway would have to get past two independent checks.

use std::sync::Arc;

use aphelion_core::FeedId;
use async_trait::async_trait;

use super::{ChainClient, OnChainNode, OnChainPrice, SubmitReceipt};
use crate::error::{NodeError, Result};
use crate::signer::SignedSubmission;

/// Forwards every read to `inner` and refuses every write.
pub struct ReadOnlyChain {
    inner: Arc<dyn ChainClient>,
}

impl ReadOnlyChain {
    pub fn new(inner: Arc<dyn ChainClient>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ChainClient for ReadOnlyChain {
    async fn ledger_time(&self) -> Result<u64> {
        self.inner.ledger_time().await
    }

    /// Deliberately not transient: `NodeError::Config` rather than
    /// `NodeError::Chain`, because a node told not to submit will still not be
    /// submitting on the next round, and retrying would turn one refusal into
    /// a loop of them.
    async fn submit_price(
        &self,
        _public_key_hex: &str,
        _submission: &SignedSubmission,
    ) -> Result<SubmitReceipt> {
        Err(NodeError::Config(
            "this node is running with submissions disabled (--dry-run); \
             refusing to submit"
                .into(),
        ))
    }

    async fn latest_price(&self, feed: &FeedId) -> Result<Option<OnChainPrice>> {
        self.inner.latest_price(feed).await
    }

    async fn node_info(&self, public_key_hex: &str) -> Result<Option<OnChainNode>> {
        self.inner.node_info(public_key_hex).await
    }

    async fn last_nonce(&self, public_key_hex: &str, feed: &FeedId) -> Result<u64> {
        self.inner.last_nonce(public_key_hex, feed).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{MockChain, OnChainNode};
    use aphelion_core::{Price, PriceMessage};

    fn feed() -> FeedId {
        FeedId::new("BTC_USD").unwrap()
    }

    fn chain() -> Arc<MockChain> {
        Arc::new(
            MockChain::new(1_700_000_000)
                .with_quorum(1)
                .with_registered("ab12", 10_000)
                .with_node(OnChainNode {
                    public_key_hex: "ab12".into(),
                    stake: 10_000_000_000,
                    reputation: 5_000,
                    status: "active".into(),
                    weight_bps: 10_000,
                    last_submission: 0,
                }),
        )
    }

    #[tokio::test]
    async fn reads_reach_the_chain_underneath() {
        // The point of the wrapper: what the operator sees is what is really
        // there, not a fixture's idea of it.
        let inner = chain();
        let ro = ReadOnlyChain::new(Arc::clone(&inner) as Arc<dyn ChainClient>);

        assert_eq!(ro.ledger_time().await.unwrap(), 1_700_000_000);
        assert!(
            ro.node_info("ab12").await.unwrap().is_some(),
            "a registered node must not be reported as unregistered"
        );
        assert_eq!(ro.last_nonce("ab12", &feed()).await.unwrap(), 0);
        assert!(ro.latest_price(&feed()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ledger_time_tracks_the_chain_rather_than_freezing() {
        // The regression that made this type necessary. A dry run used a mock
        // seeded once from real ledger time, so its clock stood still while
        // the node's advanced, and every run started reporting a clock skew
        // that did not exist as soon as it had been up for max_clock_skew.
        let inner = chain();
        let ro = ReadOnlyChain::new(Arc::clone(&inner) as Arc<dyn ChainClient>);

        let first = ro.ledger_time().await.unwrap();
        inner.set_ledger_time(first + 120);
        assert_eq!(
            ro.ledger_time().await.unwrap(),
            first + 120,
            "the wrapper served a stale ledger time"
        );
    }

    #[tokio::test]
    async fn submission_is_refused_and_never_reaches_the_chain() {
        let inner = chain();
        let ro = ReadOnlyChain::new(Arc::clone(&inner) as Arc<dyn ChainClient>);

        let submission = SignedSubmission {
            message: PriceMessage {
                aggregator: [0u8; 32],
                feed: feed(),
                price: Price::from_raw(1),
                timestamp: 1_700_000_000,
                confidence_bps: 50,
                nonce: 1,
            },
            signature: [0u8; 64],
        };

        let err = ro.submit_price("ab12", &submission).await.unwrap_err();
        assert!(
            !err.is_transient(),
            "a refusal to submit must not be retried: {err}"
        );
        assert!(
            inner.submissions().is_empty(),
            "the submission reached the chain despite being refused"
        );
    }
}
