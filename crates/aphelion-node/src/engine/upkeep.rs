//! Absence sweeps: the upkeep nobody is assigned and everybody needs.
//!
//! The aggregator charges a missed round to a node that has gone quiet, and
//! `sweep_absent` is permissionless — anyone may pay the fee to make that
//! happen. Permissionless is not the same as automatic. Until something calls
//! it, a node that stopped working keeps the weight it earned while it was
//! working, and its stale vote goes on counting towards the median for as long
//! as nobody bothers. That is the gap this module closes.
//!
//! # Why a node is the right caller
//!
//! An operator has a standing reason to want it done. Weight is relative: a
//! feed's median is taken over whoever turns up, so every basis point of
//! weight a dead node still carries is a basis point of influence the live
//! ones do not have, and every reward paid to a round it did not join is
//! smaller than it should be. Sweeping is a small fee to reduce a competitor's
//! weight to what it has actually earned lately.
//!
//! It is still a fee for a call that pays nothing back directly, which is why
//! it is **off by default**. An operator who does not want to spend on network
//! upkeep should not discover that they have been.
//!
//! # Why this node never sweeps itself
//!
//! It is excluded from every batch. Paying a fee to take reputation off your
//! own node is not a thing anyone wants, and the symmetry of the incentive is
//! what makes leaving it out honest rather than self-serving: every other
//! operator has the same reason to sweep this node that this node has to sweep
//! them. A node's own absence is somebody else's to charge, and on a network
//! with more than one operator running this loop, somebody will.
//!
//! # What this node cannot see
//!
//! The aggregator decides absence from its own storage: the later of the last
//! submission it accepted from a key and the last time that key was swept.
//! Neither is readable from outside. What is readable is the registry's
//! `last_submission`, which moves only when a round the node joined actually
//! closed.
//!
//! So this node's view is the *more pessimistic* of the two, in two ways: a
//! node whose submissions keep landing in rounds that never reach quorum looks
//! silent here and is not silent to the aggregator, and a node somebody else
//! swept a minute ago looks exactly like one nobody has touched. Both make a
//! plan that offers keys the aggregator will decline.
//!
//! That direction is the safe one — the contract re-checks every key and
//! charges nobody it should not — but it is not free, because the fee is paid
//! either way. Two things bound the waste. `sweep_absent` returns how many keys
//! it charged, so the difference between offered and charged is visible rather
//! than silent; and this module remembers what it offered, and will not offer
//! the same key again until a full absence window has passed. The memory is
//! in-process and deliberately not persisted: it is an optimisation, and a
//! restart that re-offers a key costs one fee and charges nobody.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::chain::{ChainClient, OnChainNode};
use crate::config::UpkeepConfig;
use crate::error::Result;

/// A node this sweep would charge, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub public_key_hex: String,
    /// Seconds since the last round this node is known to have taken part in.
    pub silent_for: u64,
    /// Weight the network is still giving it for that silence.
    pub weight_bps: u32,
    pub reputation: u32,
}

/// Why a key in the registry is not in the batch.
///
/// Named rather than counted, because "27 nodes, none of them sweepable" is a
/// sentence an operator has to be able to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Excuse {
    /// This node. Never its own business.
    Own,
    /// No weight: unknown, jailed or exiting. The aggregator will not charge
    /// it, and grinding down a node already serving a penalty is not upkeep.
    NoWeight,
    /// Never recorded as taking part in a closed round, so there is no moment
    /// for the silence to be measured from.
    NeverSeen,
    /// Seen recently enough that the aggregator would decline.
    Recent,
    /// Offered in a previous batch, less than one absence window ago.
    AlreadyOffered,
}

/// What a sweep would do, computed from reads alone.
#[derive(Debug, Clone, Serialize)]
pub struct SweepPlan {
    /// Registry keys examined.
    pub examined: usize,
    /// The batch, longest silence first.
    pub candidates: Vec<Candidate>,
    /// Candidates beyond `max_batch`, left for the next pass.
    pub deferred: usize,
    pub excused: Vec<(String, Excuse)>,
    pub absence_threshold: u64,
    pub ledger_time: u64,
}

impl SweepPlan {
    pub fn keys(&self) -> Vec<String> {
        self.candidates
            .iter()
            .map(|c| c.public_key_hex.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// How many keys were excused for each reason, for a one-line summary.
    pub fn excuse_counts(&self) -> Vec<(Excuse, usize)> {
        let order = [
            Excuse::Own,
            Excuse::NoWeight,
            Excuse::NeverSeen,
            Excuse::Recent,
            Excuse::AlreadyOffered,
        ];
        order
            .into_iter()
            .filter_map(|e| {
                let n = self.excused.iter().filter(|(_, x)| *x == e).count();
                (n > 0).then_some((e, n))
            })
            .collect()
    }
}

/// What a sweep did.
#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    pub plan: SweepPlan,
    /// Keys the aggregator actually charged. Never more than were offered, and
    /// less whenever its view of a key was less pessimistic than ours.
    pub charged: u32,
    pub tx_hash: Option<String>,
}

/// Decide which of `nodes` to offer to `sweep_absent`.
///
/// Pure, so the decision can be pinned by tests without a chain: everything it
/// needs is an argument, and everything it rejects is rejected for a reason it
/// names. Each check mirrors one the contract makes, so a key that survives all
/// of them is one the aggregator should charge.
pub fn plan_sweep(
    now: u64,
    absence_threshold: u64,
    nodes: &[OnChainNode],
    own_public_key_hex: &str,
    offered: &HashMap<String, u64>,
    max_batch: usize,
) -> SweepPlan {
    let mut candidates = Vec::new();
    let mut excused = Vec::new();

    for node in nodes {
        let key = node.public_key_hex.as_str();
        let excuse = if key.eq_ignore_ascii_case(own_public_key_hex) {
            Some(Excuse::Own)
        } else if node.weight_bps == 0 {
            Some(Excuse::NoWeight)
        } else if node.last_submission == 0 {
            Some(Excuse::NeverSeen)
        } else if now < node.last_submission || now - node.last_submission < absence_threshold {
            // `now < last_submission` is the registry ahead of the clock we
            // were handed. Nothing good comes of extrapolating from it.
            Some(Excuse::Recent)
        } else {
            match offered.get(key) {
                Some(&at) if now < at || now - at < absence_threshold => {
                    Some(Excuse::AlreadyOffered)
                }
                _ => None,
            }
        };

        match excuse {
            Some(e) => excused.push((key.to_string(), e)),
            None => candidates.push(Candidate {
                public_key_hex: key.to_string(),
                silent_for: now - node.last_submission,
                weight_bps: node.weight_bps,
                reputation: node.reputation,
            }),
        }
    }

    // Longest silence first, then by key so the order is total. Sorting matters
    // only when the batch is truncated, and then it matters a lot: the node
    // that has been dead longest is the one still holding weight it has least
    // claim to, and it should not be the one perpetually left off the end.
    candidates.sort_by(|a, b| {
        b.silent_for
            .cmp(&a.silent_for)
            .then_with(|| a.public_key_hex.cmp(&b.public_key_hex))
    });

    let deferred = candidates.len().saturating_sub(max_batch);
    candidates.truncate(max_batch);

    SweepPlan {
        examined: nodes.len(),
        candidates,
        deferred,
        excused,
        absence_threshold,
        ledger_time: now,
    }
}

/// The loop that keeps the network's absence accounting current.
pub struct Sweeper {
    chain: Arc<dyn ChainClient>,
    own_public_key_hex: String,
    config: UpkeepConfig,
    /// Key to the ledger time we last offered it. See the module note on what
    /// this node cannot see.
    offered: tokio::sync::Mutex<HashMap<String, u64>>,
}

impl Sweeper {
    pub fn new(
        chain: Arc<dyn ChainClient>,
        own_public_key_hex: String,
        config: UpkeepConfig,
    ) -> Self {
        Self {
            chain,
            own_public_key_hex,
            config,
            offered: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Takes `Arc<Self>` rather than `self` so the HTTP API can hold the same
    /// instance. `/v1/upkeep` has to answer with the plan this loop would
    /// actually submit, and a second `Sweeper` would have an empty record of
    /// what had already been offered — it would report keys the loop is
    /// deliberately leaving alone, which is the one thing that endpoint exists
    /// to be right about.
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if !self.config.sweep_absent {
            tracing::debug!("absence sweeps are disabled; upkeep loop not running");
            return;
        }
        tracing::info!(
            interval = ?self.config.interval,
            max_batch = self.config.max_batch,
            "absence sweeps enabled"
        );

        let mut ticker = tokio::time::interval(self.config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match self.sweep_once().await {
                        Ok(Some(report)) => tracing::info!(
                            offered = report.plan.candidates.len(),
                            charged = report.charged,
                            tx = ?report.tx_hash,
                            "absence sweep complete"
                        ),
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "absence sweep failed");
                            metrics::counter!(
                                "aphelion_sweeps_total", "outcome" => "error"
                            )
                            .increment(1);
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("sweeper shutting down");
                        return;
                    }
                }
            }
        }
    }

    /// Read the registry, decide, and submit if there is anything to submit.
    ///
    /// `Ok(None)` when there was nothing to charge — the common case on a
    /// healthy network, and deliberately not a transaction.
    pub async fn sweep_once(&self) -> Result<Option<SweepReport>> {
        let plan = self.plan().await?;

        metrics::gauge!("aphelion_registry_nodes").set(plan.examined as f64);
        metrics::gauge!("aphelion_sweep_candidates").set(plan.candidates.len() as f64);

        if plan.is_empty() {
            tracing::debug!(
                examined = plan.examined,
                excused = ?plan.excuse_counts(),
                "nothing to sweep"
            );
            metrics::counter!("aphelion_sweeps_total", "outcome" => "nothing_to_do").increment(1);
            return Ok(None);
        }

        let keys = plan.keys();
        for c in &plan.candidates {
            tracing::info!(
                public_key = %c.public_key_hex,
                silent_for = c.silent_for,
                weight_bps = c.weight_bps,
                "offering a silent node to sweep_absent"
            );
        }

        let receipt = self.chain.sweep_absent(&keys).await?;

        // Recorded whatever the aggregator decided. A key it declined is one
        // our view was wrong about, and re-offering it next tick would repeat
        // the same mistake at the same price.
        {
            let mut offered = self.offered.lock().await;
            for key in &keys {
                offered.insert(key.clone(), plan.ledger_time);
            }
            // Keys no longer in the registry never come back; without this the
            // map is the one thing in the node that only ever grows.
            offered.retain(|_, &mut at| plan.ledger_time.saturating_sub(at) <= DROP_AFTER);
        }

        metrics::counter!("aphelion_sweeps_total", "outcome" => "submitted").increment(1);
        metrics::counter!("aphelion_nodes_charged_total").increment(receipt.charged as u64);

        if receipt.charged < keys.len() as u32 {
            tracing::info!(
                offered = keys.len(),
                charged = receipt.charged,
                "the aggregator declined some keys; it had seen them more \
                 recently than the registry showed, or somebody else swept first"
            );
        }

        Ok(Some(SweepReport {
            plan,
            charged: receipt.charged,
            tx_hash: receipt.tx_hash,
        }))
    }

    /// What a sweep would do right now. Reads only — this is what `/v1/upkeep`
    /// and `aphelion-node sweep` serve, and neither costs anything.
    pub async fn plan(&self) -> Result<SweepPlan> {
        let now = self.chain.ledger_time().await?;
        let absence_threshold = self.chain.absence_threshold().await?;
        let keys = self.chain.list_nodes().await?;

        let mut nodes = Vec::with_capacity(keys.len());
        for key in &keys {
            // A record that cannot be read is left out rather than guessed at.
            // The alternative -- treating an unreadable record as absent --
            // would charge a node for an RPC failure at our end.
            match self.chain.node_info(key).await {
                Ok(Some(node)) => nodes.push(node),
                Ok(None) => tracing::debug!(
                    public_key = %key,
                    "registry lists a key it has no record for; skipping"
                ),
                Err(e) => tracing::warn!(
                    public_key = %key, error = %e,
                    "could not read a registry record; excluding it from this sweep"
                ),
            }
        }

        let offered = self.offered.lock().await;
        Ok(plan_sweep(
            now,
            absence_threshold,
            &nodes,
            &self.own_public_key_hex,
            &offered,
            self.config.max_batch,
        ))
    }

    pub fn enabled(&self) -> bool {
        self.config.sweep_absent
    }
}

/// Forget that a key was offered once it is this stale. Long enough that it
/// cannot cause a re-offer inside an absence window at any sane threshold,
/// short enough that a departed operator's key does not sit here forever.
const DROP_AFTER: u64 = 7 * 24 * 60 * 60;

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_735_689_600;
    const THRESHOLD: u64 = 3_600;
    const SELF_KEY: &str = "0011";

    fn node(key: &str, last_submission: u64, weight_bps: u32) -> OnChainNode {
        OnChainNode {
            public_key_hex: key.into(),
            stake: 1_000 * 10_000_000,
            reputation: if weight_bps >= 10_000 { 8_000 } else { 5_000 },
            status: if weight_bps == 0 { "jailed" } else { "active" }.into(),
            weight_bps,
            last_submission,
            jailed_until: 0,
            unbonding_until: 0,
        }
    }

    fn plan(nodes: &[OnChainNode]) -> SweepPlan {
        plan_sweep(NOW, THRESHOLD, nodes, SELF_KEY, &HashMap::new(), 25)
    }

    fn excuse(plan: &SweepPlan, key: &str) -> Option<Excuse> {
        plan.excused.iter().find(|(k, _)| k == key).map(|(_, e)| *e)
    }

    #[test]
    fn a_node_silent_past_the_threshold_is_a_candidate() {
        let p = plan(&[node("aa", NOW - THRESHOLD - 1, 10_000)]);
        assert_eq!(p.keys(), vec!["aa".to_string()]);
        assert_eq!(p.candidates[0].silent_for, THRESHOLD + 1);
    }

    #[test]
    fn silence_of_exactly_the_threshold_counts() {
        // The contract charges at `>= absence_threshold`. Being stricter here
        // would leave a node uncharged that the network says is chargeable.
        let p = plan(&[node("aa", NOW - THRESHOLD, 10_000)]);
        assert_eq!(p.keys(), vec!["aa".to_string()]);
    }

    #[test]
    fn a_node_seen_a_moment_ago_is_left_alone() {
        let p = plan(&[node("aa", NOW - THRESHOLD + 1, 10_000)]);
        assert!(p.is_empty());
        assert_eq!(excuse(&p, "aa"), Some(Excuse::Recent));
    }

    #[test]
    fn this_node_is_never_in_its_own_batch() {
        // Long dead by its own reckoning, and still not its own business.
        let p = plan(&[node(SELF_KEY, NOW - 10 * THRESHOLD, 10_000)]);
        assert!(p.is_empty(), "a node offered itself to sweep_absent");
        assert_eq!(excuse(&p, SELF_KEY), Some(Excuse::Own));
    }

    #[test]
    fn the_own_key_is_matched_regardless_of_hex_case() {
        let mixed = "00AA";
        let p = plan_sweep(
            NOW,
            THRESHOLD,
            &[node("00aa", NOW - 10 * THRESHOLD, 10_000)],
            mixed,
            &HashMap::new(),
            25,
        );
        assert!(
            p.is_empty(),
            "a difference of hex case must not make a node a stranger to itself"
        );
    }

    #[test]
    fn a_node_with_no_weight_is_not_ground_down_further() {
        // Jailed or exiting. The aggregator declines it, so offering it would
        // be a fee for nothing -- and a jailed node's silence is the penalty
        // already running.
        let p = plan(&[node("aa", NOW - 10 * THRESHOLD, 0)]);
        assert!(p.is_empty());
        assert_eq!(excuse(&p, "aa"), Some(Excuse::NoWeight));
    }

    #[test]
    fn a_node_that_has_never_published_has_no_silence_to_measure() {
        // A freshly registered operator, minutes old. The contract starts its
        // clock rather than assuming the worst; there is nothing to pay for.
        let p = plan(&[node("aa", 0, 5_000)]);
        assert!(p.is_empty());
        assert_eq!(excuse(&p, "aa"), Some(Excuse::NeverSeen));
    }

    #[test]
    fn a_registry_timestamp_in_the_future_is_not_extrapolated_from() {
        let p = plan(&[node("aa", NOW + 600, 10_000)]);
        assert!(p.is_empty(), "a clock disagreement became a penalty");
        assert_eq!(excuse(&p, "aa"), Some(Excuse::Recent));
    }

    #[test]
    fn a_key_offered_recently_is_not_offered_again() {
        // The aggregator may have declined it, and would decline it again for
        // the same reason. Paying twice to find that out is the waste this
        // exists to avoid.
        let nodes = [node("aa", NOW - 10 * THRESHOLD, 10_000)];
        let mut offered = HashMap::new();
        offered.insert("aa".to_string(), NOW - THRESHOLD + 1);

        let p = plan_sweep(NOW, THRESHOLD, &nodes, SELF_KEY, &offered, 25);
        assert!(p.is_empty());
        assert_eq!(excuse(&p, "aa"), Some(Excuse::AlreadyOffered));
    }

    #[test]
    fn a_key_offered_a_full_window_ago_is_offered_again() {
        // Still silent after another whole window: there is a second charge to
        // make, and the contract's own clock has moved on with ours.
        let nodes = [node("aa", NOW - 10 * THRESHOLD, 10_000)];
        let mut offered = HashMap::new();
        offered.insert("aa".to_string(), NOW - THRESHOLD);

        let p = plan_sweep(NOW, THRESHOLD, &nodes, SELF_KEY, &offered, 25);
        assert_eq!(p.keys(), vec!["aa".to_string()]);
    }

    #[test]
    fn the_longest_silence_goes_first_when_the_batch_is_truncated() {
        let nodes = [
            node("aa", NOW - 2 * THRESHOLD, 10_000),
            node("bb", NOW - 9 * THRESHOLD, 10_000),
            node("cc", NOW - 5 * THRESHOLD, 10_000),
        ];
        let p = plan_sweep(NOW, THRESHOLD, &nodes, SELF_KEY, &HashMap::new(), 2);
        assert_eq!(p.keys(), vec!["bb".to_string(), "cc".to_string()]);
        assert_eq!(p.deferred, 1, "the rest are for the next pass, not dropped");
    }

    #[test]
    fn ties_are_broken_by_key_so_the_batch_is_deterministic() {
        // Two nodes that went quiet in the same round. Without a total order
        // the truncated batch would depend on registry iteration order, and
        // the node left off the end could be a different one every pass.
        let nodes = [
            node("cc", NOW - 3 * THRESHOLD, 10_000),
            node("bb", NOW - 3 * THRESHOLD, 10_000),
        ];
        let p = plan_sweep(NOW, THRESHOLD, &nodes, SELF_KEY, &HashMap::new(), 1);
        assert_eq!(p.keys(), vec!["bb".to_string()]);
    }

    #[test]
    fn a_healthy_network_produces_no_transaction_and_says_why() {
        let nodes = [
            node("aa", NOW - 60, 10_000),
            node("bb", NOW - 120, 10_000),
            node(SELF_KEY, NOW - 30, 10_000),
        ];
        let p = plan(&nodes);
        assert!(p.is_empty());
        assert_eq!(p.examined, 3);
        assert_eq!(
            p.excuse_counts(),
            vec![(Excuse::Own, 1), (Excuse::Recent, 2)]
        );
    }
}
