//! Several nodes, one network.
//!
//! Every other test in this crate exercises one node in isolation. These run a
//! set of independently keyed signers against a single shared chain and assert
//! the properties that only exist when there is more than one of them: that a
//! minority cannot move the price, that weight rather than headcount decides a
//! contested round, and that one node's key is useless for submitting as
//! another.
//!
//! The chain here is [`MockChain`], which computes its median with the same
//! `aphelion_core::math::weighted_median` the contract mirrors. A simulation
//! whose consensus rule differed from the real one would tell an operator
//! nothing about what happens on chain.
//!
//! What this deliberately does not cover, and the multi-node harness on the
//! roadmap will: several node *processes*, each with its own database and RPC
//! connection, racing each other for real.

use std::sync::Arc;

use aphelion_core::{FeedId, Price};
use aphelion_node::chain::{ChainClient, MockChain};
use aphelion_node::signer::NodeSigner;

const LEDGER_TIME: u64 = 1_735_689_600;
/// $64_231.55.
const TRUE_PRICE: &str = "64231.55";
/// A price no order book has ever shown.
const LIE: &str = "300.00";

fn feed() -> FeedId {
    FeedId::new("BTC_USD").unwrap()
}

fn price(s: &str) -> Price {
    Price::parse_decimal(s).unwrap()
}

/// A set of independently keyed nodes.
///
/// Each gets its own key file, because the point of the exercise is that these
/// are separate identities: sharing one signer would quietly test a single node
/// submitting several times.
struct Network {
    signers: Vec<Arc<NodeSigner>>,
    dir: std::path::PathBuf,
}

impl Drop for Network {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Network {
    fn of(n: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("aphelion-multi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let signers = (0..n)
            .map(|i| {
                let path = dir.join(format!("node-{i}.json"));
                NodeSigner::generate(&path).expect("keygen");
                // Every node in one network signs against the same aggregator,
                // which is what makes their signatures comparable at all.
                Arc::new(NodeSigner::load(&path, [0x11; 32]).expect("load"))
            })
            .collect();
        Self { signers, dir }
    }

    fn key(&self, i: usize) -> String {
        self.signers[i].public_key_hex()
    }

    /// Wire every node into a chain at the given weight.
    fn chain(&self, quorum: usize, weights: &[u32]) -> MockChain {
        let mut chain = MockChain::new(LEDGER_TIME).with_quorum(quorum);
        for (i, weight) in weights.iter().enumerate() {
            chain = chain.with_registered(&self.key(i), *weight);
        }
        chain
    }

    async fn submit(
        &self,
        chain: &MockChain,
        node: usize,
        value: &str,
        nonce: u64,
    ) -> Result<bool, String> {
        let submission =
            self.signers[node].sign_price(&feed(), price(value), LEDGER_TIME, 25, nonce);
        chain
            .submit_price(&self.key(node), &submission)
            .await
            .map(|r| r.finalized_round)
            .map_err(|e| e.to_string())
    }
}

#[tokio::test]
async fn a_round_does_not_publish_until_quorum_is_reached() {
    let network = Network::of(3);
    let chain = network.chain(3, &[10_000; 3]);

    assert!(!network.submit(&chain, 0, TRUE_PRICE, 1).await.unwrap());
    assert!(chain.latest_price(&feed()).await.unwrap().is_none());
    assert_eq!(chain.pending(&feed()), 1);

    assert!(!network.submit(&chain, 1, TRUE_PRICE, 1).await.unwrap());
    assert!(network.submit(&chain, 2, TRUE_PRICE, 1).await.unwrap());

    let published = chain
        .latest_price(&feed())
        .await
        .unwrap()
        .expect("published");
    assert_eq!(published.price, price(TRUE_PRICE));
    assert_eq!(published.num_nodes, 3);
    assert_eq!(chain.pending(&feed()), 0, "the round is closed and cleared");
}

#[tokio::test]
async fn a_minority_cannot_move_the_published_price() {
    let network = Network::of(5);
    let chain = network.chain(5, &[10_000; 5]);

    for node in 0..3 {
        network.submit(&chain, node, TRUE_PRICE, 1).await.unwrap();
    }
    for node in 3..5 {
        network.submit(&chain, node, LIE, 1).await.unwrap();
    }

    let published = chain
        .latest_price(&feed())
        .await
        .unwrap()
        .expect("published");
    assert_eq!(published.price, price(TRUE_PRICE));
    assert_eq!(
        published.num_nodes, 3,
        "the two liars voted, and appear in no published statistic"
    );
}

#[tokio::test]
async fn weight_decides_a_contested_round_rather_than_headcount() {
    let network = Network::of(5);
    // Two nodes that earned full weight against three that are still at half.
    let chain = network.chain(5, &[10_000, 10_000, 5_000, 5_000, 5_000]);

    for node in 0..2 {
        network.submit(&chain, node, TRUE_PRICE, 1).await.unwrap();
    }
    for node in 2..5 {
        network.submit(&chain, node, "64500.00", 1).await.unwrap();
    }

    // 20_000 bps against 15_000: the proven pair carries it, even outnumbered.
    let published = chain
        .latest_price(&feed())
        .await
        .unwrap()
        .expect("published");
    assert_eq!(published.price, price(TRUE_PRICE));
}

#[tokio::test]
async fn a_node_cannot_submit_twice_in_one_round() {
    let network = Network::of(3);
    let chain = network.chain(3, &[10_000; 3]);

    network.submit(&chain, 0, TRUE_PRICE, 1).await.unwrap();
    let err = network
        .submit(&chain, 0, TRUE_PRICE, 2)
        .await
        .expect_err("a second vote in the same round");
    assert!(err.contains("DuplicateSubmission"), "{err}");
}

#[tokio::test]
async fn nonces_are_tracked_per_node_and_not_shared() {
    let network = Network::of(3);
    let chain = network.chain(3, &[10_000; 3]);

    network.submit(&chain, 0, TRUE_PRICE, 7).await.unwrap();
    // Node 1 has spent no nonces of its own, so a low one is still fine. A
    // shared counter would reject this, and would also let one node exhaust
    // the whole network's nonce space.
    network.submit(&chain, 1, TRUE_PRICE, 1).await.unwrap();

    assert_eq!(chain.last_nonce(&network.key(0), &feed()).await.unwrap(), 7);
    assert_eq!(chain.last_nonce(&network.key(1), &feed()).await.unwrap(), 1);
    assert_eq!(chain.last_nonce(&network.key(2), &feed()).await.unwrap(), 0);
}

#[tokio::test]
async fn an_unregistered_node_carries_no_authority() {
    let network = Network::of(3);
    // Only two of the three are registered.
    let chain = MockChain::new(LEDGER_TIME)
        .with_quorum(2)
        .with_registered(&network.key(0), 10_000)
        .with_registered(&network.key(1), 10_000);

    network.submit(&chain, 0, TRUE_PRICE, 1).await.unwrap();
    let err = network
        .submit(&chain, 2, TRUE_PRICE, 1)
        .await
        .expect_err("an unregistered key");
    assert!(err.contains("NotAuthorizedNode"), "{err}");
}

#[tokio::test]
async fn a_jailed_node_carries_no_authority_either() {
    let network = Network::of(3);
    let chain = network.chain(2, &[10_000, 10_000, 0]);

    let err = network
        .submit(&chain, 2, TRUE_PRICE, 1)
        .await
        .expect_err("a jailed node");
    assert!(err.contains("NotAuthorizedNode"), "{err}");
}

#[tokio::test]
async fn a_round_where_nobody_agrees_publishes_nothing() {
    let network = Network::of(2);
    let chain = network.chain(2, &[10_000; 2]);

    // Two quotes straddling a midpoint neither reported: both are outliers
    // against the median, so there is nothing to publish.
    network.submit(&chain, 0, "100.00", 1).await.unwrap();
    network.submit(&chain, 1, "200.00", 1).await.unwrap();

    assert!(
        chain.latest_price(&feed()).await.unwrap().is_none(),
        "a network that did not agree must not publish"
    );
}

#[tokio::test]
async fn one_nodes_signature_does_not_authorise_another() {
    let network = Network::of(2);
    let chain = network.chain(1, &[10_000; 2]);

    // Node 0 signs a lie; the submission is relayed claiming to be node 1.
    // Nothing about the relaying account is checked, because nothing about it
    // matters -- the signature is the authority, and it is over node 0's key.
    let submission = network.signers[0].sign_price(&feed(), price(LIE), LEDGER_TIME, 25, 1);
    assert!(submission.verify(&network.signers[0].public_key()));
    assert!(
        !submission.verify(&network.signers[1].public_key()),
        "a signature must not verify against a key that did not produce it"
    );

    let err = chain
        .submit_price(&network.key(1), &submission)
        .await
        .expect_err("a submission relayed under another node's key");
    assert!(err.to_string().contains("BadSignature"), "{err}");
    assert!(chain.latest_price(&feed()).await.unwrap().is_none());
}

#[tokio::test]
async fn every_node_reaches_the_same_conclusion_from_the_same_inputs() {
    // The property the whole design rests on: the median a node predicts
    // locally is the median the chain computes. If these ever differ, an
    // honest node is penalised for arithmetic it had no way to see.
    use aphelion_core::{weighted_median, WeightedSample};

    let network = Network::of(5);
    let chain = network.chain(5, &[10_000, 10_000, 5_000, 5_000, 5_000]);

    let quotes = ["64231.55", "64230.00", "64240.00", "64235.00", "64229.00"];
    let weights = [10_000u32, 10_000, 5_000, 5_000, 5_000];

    for (node, quote) in quotes.iter().enumerate() {
        network.submit(&chain, node, quote, 1).await.unwrap();
    }

    let mut predicted: Vec<WeightedSample> = quotes
        .iter()
        .zip(weights)
        .map(|(q, w)| WeightedSample::new(price(q).raw(), w))
        .collect();
    let predicted = weighted_median(&mut predicted).unwrap();

    let published = chain
        .latest_price(&feed())
        .await
        .unwrap()
        .expect("published");
    assert_eq!(
        published.price.raw(),
        predicted,
        "the node's prediction and the chain's outcome must be the same number"
    );
}
