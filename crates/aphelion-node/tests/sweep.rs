//! Absence sweeps, end to end against the mock chain.
//!
//! The unit tests in `engine::upkeep` pin the decision — which keys a node
//! would offer, and the reason it excuses each of the rest. They deliberately
//! know nothing about a chain. These tests run the sweeper against
//! [`MockChain`], which mirrors `Aggregator::sweep_absent` including every case
//! where it declines to charge, so what is exercised here is the part the unit
//! tests cannot reach: that a node's plan and the aggregator's judgement agree,
//! and what happens when they do not.
//!
//! Why that distinction is worth two layers of test: the node reads the
//! registry's `last_submission` and the aggregator decides from storage of its
//! own that nothing outside the contract can read. A sweeper whose plan was
//! built from a different view of absence than the one that gets applied would
//! pass every unit test and spend an operator's money on calls that charge
//! nobody.

use std::sync::Arc;
use std::time::Duration;

use aphelion_core::{FeedId, Price};
use aphelion_node::chain::{ChainClient, MockChain, OnChainNode};
use aphelion_node::config::UpkeepConfig;
use aphelion_node::engine::{Excuse, Sweeper};
use aphelion_node::signer::NodeSigner;

const T0: u64 = 1_735_689_600;
const THRESHOLD: u64 = 3_600;
/// Just under the jail threshold plus one round's reward, so that three windows
/// of silence at 25 reputation apiece are what finally takes a node under it.
const NEAR_THE_LINE: u32 = 3_050;

fn feed() -> FeedId {
    FeedId::new("BTC_USD").unwrap()
}

/// Two independently keyed nodes and a chain that knows both.
struct Fixture {
    signers: Vec<Arc<NodeSigner>>,
    chain: Arc<MockChain>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(quorum: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("aphelion-sweep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let signers: Vec<Arc<NodeSigner>> = (0..2)
            .map(|i| {
                let path = dir.join(format!("node-{i}.json"));
                NodeSigner::generate(&path).expect("keygen");
                Arc::new(NodeSigner::load(&path, [0x11; 32]).expect("load"))
            })
            .collect();

        let mut chain = MockChain::new(T0)
            .with_quorum(quorum)
            .with_absence_threshold(THRESHOLD);
        for signer in &signers {
            chain = chain.with_registered(&signer.public_key_hex(), 10_000);
        }

        Self {
            signers,
            chain: Arc::new(chain),
            dir,
        }
    }

    fn key(&self, i: usize) -> String {
        self.signers[i].public_key_hex()
    }

    /// A sweeper belonging to node `i`, with a batch large enough not to be the
    /// thing under test.
    fn sweeper(&self, i: usize) -> Sweeper {
        Sweeper::new(
            Arc::clone(&self.chain) as Arc<dyn ChainClient>,
            self.key(i),
            UpkeepConfig {
                sweep_absent: true,
                interval: Duration::from_secs(THRESHOLD),
                max_batch: 25,
            },
        )
    }

    /// Put node `i` at a given reputation, leaving everything else alone.
    async fn set_reputation(&self, i: usize, reputation: u32) {
        let mut node = self.node(i).await;
        node.reputation = reputation;
        self.chain.set_node(node);
    }

    /// Rewrite node `i`'s registry record to say it last took part `seconds`
    /// ago, without touching what the aggregator saw.
    async fn backdate_registry(&self, i: usize, now: u64, seconds: u64) {
        let mut node = self.node(i).await;
        node.last_submission = now.saturating_sub(seconds);
        self.chain.set_node(node);
    }

    async fn node(&self, i: usize) -> OnChainNode {
        self.chain
            .node_info(&self.key(i))
            .await
            .unwrap()
            .expect("a registered node has a record")
    }

    async fn publish(&self, i: usize, at: u64, nonce: u64) {
        self.chain.set_ledger_time(at);
        let submission = self.signers[i].sign_price(
            &feed(),
            Price::parse_decimal("64231.55").unwrap(),
            at,
            25,
            nonce,
        );
        self.chain
            .submit_price(&self.key(i), &submission)
            .await
            .expect("the fixture's own submission should be accepted");
    }
}

#[tokio::test]
async fn a_healthy_network_produces_no_transaction_at_all() {
    // The common case, and the one that must not cost anything: every node has
    // published within the window, so there is nothing to charge and therefore
    // nothing to submit.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;

    assert!(f.sweeper(0).sweep_once().await.unwrap().is_none());
    assert!(
        f.chain.sweeps().is_empty(),
        "a sweep with nothing to charge still sent a transaction"
    );
}

#[tokio::test]
async fn a_node_that_goes_quiet_is_charged_a_missed_round() {
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    let before = f.node(1).await.reputation;

    // One full window later, node 1 has said nothing and node 0 has.
    let now = T0 + THRESHOLD;
    f.publish(0, now, 2).await;

    let report = f
        .sweeper(0)
        .sweep_once()
        .await
        .unwrap()
        .expect("a silent node is something to charge");

    assert_eq!(report.plan.keys(), vec![f.key(1)]);
    assert_eq!(report.charged, 1);
    assert_eq!(
        f.node(1).await.reputation,
        before - 25,
        "the missed round cost the standard 25 reputation"
    );
    assert_eq!(
        f.node(0).await.reputation,
        before + 50,
        "the node that kept publishing was rewarded, not charged"
    );
}

#[tokio::test]
async fn a_nodes_own_absence_is_never_its_own_business() {
    // Node 0 is the one that has been dead for a day. Its own sweeper must not
    // pay a fee to take reputation off itself; the symmetric incentive means
    // another operator running this same loop will do it.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;

    let now = T0 + 24 * THRESHOLD;
    f.chain.set_ledger_time(now);
    f.backdate_registry(0, now, 24 * THRESHOLD).await;
    f.backdate_registry(1, now, 60).await;

    let plan = f.sweeper(0).plan().await.unwrap();
    assert!(plan.is_empty(), "a node offered itself to sweep_absent");
    assert!(plan
        .excused
        .iter()
        .any(|(k, e)| k == &f.key(0) && *e == Excuse::Own));

    // And from the other side of the same network, it is chargeable.
    let plan = f.sweeper(1).plan().await.unwrap();
    assert_eq!(plan.keys(), vec![f.key(0)]);
}

#[tokio::test]
async fn one_silence_is_charged_once_however_many_nodes_sweep_it() {
    // The protection that matters is the aggregator's `Swept` marker, not the
    // sweeper's own memory of what it offered — otherwise a network of ten
    // operators would bill one absence ten times. Each sweep here comes from a
    // freshly built sweeper, so nothing local is doing the work.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;

    let now = T0 + THRESHOLD;
    f.chain.set_ledger_time(now);
    let before = f.node(1).await.reputation;

    let first = f.sweeper(0).sweep_once().await.unwrap().expect("charged");
    assert_eq!(first.charged, 1);

    // A second operator's node sees what we did — the registry still shows node
    // 1 as silent — so it offers the same key and pays the same fee. What it
    // does not get is a second charge.
    let second = f
        .sweeper(0)
        .sweep_once()
        .await
        .unwrap()
        .expect("the registry still reads as silent, so a key is still offered");
    assert_eq!(second.plan.keys(), vec![f.key(1)]);
    assert_eq!(
        second.charged, 0,
        "the same silence was billed twice inside one window"
    );
    assert_eq!(
        f.node(1).await.reputation,
        before - 25,
        "reputation fell twice for one absence"
    );
}

#[tokio::test]
async fn a_second_window_of_silence_is_a_second_charge() {
    // The other half of the property above: the marker stops double billing
    // within a window, and must not stop billing at all.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    let before = f.node(1).await.reputation;

    for window in 1..=2 {
        f.chain.set_ledger_time(T0 + window * THRESHOLD);
        // A fresh sweeper each window, for the reason above.
        f.sweeper(0).sweep_once().await.unwrap();
    }

    assert_eq!(f.node(1).await.reputation, before - 50);
}

#[tokio::test]
async fn sustained_absence_jails_a_node_and_its_vote_stops_counting() {
    // What the whole loop is for. A node that has stopped working does not just
    // lose reputation as a formality — it crosses the jail threshold, its
    // weight goes to zero, and the aggregator refuses its submissions. Until
    // something calls sweep_absent, none of that happens and a dead node's
    // stale vote keeps counting towards the median.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    f.set_reputation(1, NEAR_THE_LINE).await;
    assert_eq!(f.node(1).await.weight_bps, 10_000);

    // 3_050 -> 3_025 -> 3_000 -> 2_975, and 3_000 is the jail threshold.
    for window in 1..=3 {
        f.chain.set_ledger_time(T0 + window * THRESHOLD);
        f.sweeper(0).sweep_once().await.unwrap();
    }

    let jailed = f.node(1).await;
    assert_eq!(jailed.status, "jailed");
    assert_eq!(jailed.weight_bps, 0, "a jailed node must carry no weight");

    // And the refusal is the aggregator's, not a bookkeeping detail.
    let submission = f.signers[1].sign_price(
        &feed(),
        Price::parse_decimal("64231.55").unwrap(),
        T0 + 3 * THRESHOLD,
        25,
        9,
    );
    let err = f
        .chain
        .submit_price(&f.key(1), &submission)
        .await
        .expect_err("a jailed node must not be able to submit")
        .to_string();
    assert!(err.contains("NotAuthorizedNode"), "{err}");
}

#[tokio::test]
async fn a_node_already_serving_a_jail_term_is_not_ground_down_further() {
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    f.set_reputation(1, NEAR_THE_LINE).await;
    for window in 1..=3 {
        f.chain.set_ledger_time(T0 + window * THRESHOLD);
        f.sweeper(0).sweep_once().await.unwrap();
    }
    let jailed = f.node(1).await.reputation;

    // Jail is served as time. The node is silent because the aggregator
    // refuses it, so continuing to charge it for that silence would be billing
    // it for the penalty it is already serving.
    f.chain.set_ledger_time(T0 + 20 * THRESHOLD);
    let plan = f.sweeper(0).plan().await.unwrap();
    assert!(plan.is_empty());
    assert!(plan
        .excused
        .iter()
        .any(|(k, e)| k == &f.key(1) && *e == Excuse::NoWeight));
    assert_eq!(f.node(1).await.reputation, jailed);
}

#[tokio::test]
async fn a_key_the_aggregator_has_seen_more_recently_is_declined_and_reported() {
    // The divergence the module documents, pinned. The node reads the
    // registry's `last_submission`, which moves only when a round the node
    // joined actually closed; the aggregator decides from the last submission
    // it *accepted*, closed round or not. So a node whose submissions keep
    // landing in rounds that never reach quorum looks silent from outside and
    // is not silent to the contract.
    //
    // The fee is paid either way. What must not happen is the node believing it
    // charged somebody when it did not.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;

    f.chain.set_ledger_time(T0);
    f.backdate_registry(1, T0, 10 * THRESHOLD).await;
    let before = f.node(1).await.reputation;

    let report = f
        .sweeper(0)
        .sweep_once()
        .await
        .unwrap()
        .expect("the plan offered a key, so a transaction was sent");

    assert_eq!(report.plan.keys(), vec![f.key(1)]);
    assert_eq!(
        report.charged, 0,
        "the aggregator charged a node it had just accepted a submission from"
    );
    assert_eq!(f.node(1).await.reputation, before);
}

#[tokio::test]
async fn a_declined_key_is_not_offered_again_inside_the_same_window() {
    // Having paid once to learn that our view of a key was wrong, the node
    // should not pay again next tick to learn the same thing.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    f.chain.set_ledger_time(T0);
    f.backdate_registry(1, T0, 10 * THRESHOLD).await;

    let sweeper = f.sweeper(0);
    sweeper.sweep_once().await.unwrap().expect("first attempt");
    assert_eq!(f.chain.sweeps().len(), 1);

    assert!(
        sweeper.sweep_once().await.unwrap().is_none(),
        "the same key was offered twice inside one absence window"
    );
    assert_eq!(
        f.chain.sweeps().len(),
        1,
        "a second transaction was sent for a key already offered"
    );
}

#[tokio::test]
async fn a_node_that_has_never_published_is_left_alone() {
    // A freshly registered operator, minutes old, whose first round has not
    // closed yet. There is no moment to measure silence from, and the contract
    // starts its clock rather than assuming the worst.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.chain.set_ledger_time(T0 + 10 * THRESHOLD);

    let plan = f.sweeper(0).plan().await.unwrap();
    assert!(plan.is_empty());
    assert!(plan
        .excused
        .iter()
        .any(|(k, e)| k == &f.key(1) && *e == Excuse::NeverSeen));
}

#[tokio::test]
async fn a_sweep_that_fails_to_land_charges_nobody_and_is_not_retried() {
    // The transaction is the last step and the only one that costs anything. A
    // node whose endpoint disappears between planning and submitting has to
    // surface that as a failure and leave it for the next interval; retrying
    // inside one pass would turn a flaky endpoint into a stream of fees.
    let f = Fixture::new(1);
    f.publish(0, T0, 1).await;
    f.publish(1, T0, 1).await;
    f.chain.set_ledger_time(T0 + THRESHOLD);
    let before = f.node(1).await.reputation;

    f.chain
        .fail_next("error: failed to connect to RPC endpoint");
    let err = f
        .sweeper(0)
        .sweep_once()
        .await
        .expect_err("a sweep that did not land must not report success");
    assert!(
        err.is_transient(),
        "an endpoint failure should be retried later: {err}"
    );
    assert_eq!(
        f.node(1).await.reputation,
        before,
        "a transport failure cost somebody reputation"
    );
    assert_eq!(
        f.chain.sweeps().len(),
        1,
        "the failed sweep was retried in place"
    );
}
