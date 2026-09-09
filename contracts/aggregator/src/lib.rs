#![no_std]
//! # Aphelion price aggregator
//!
//! The aggregator is where independently operated nodes become one price.
//! Nodes sign observations off chain; this contract verifies those signatures,
//! weights each vote by the submitter's registry reputation, and publishes the
//! weighted median once a round has both enough voters and enough weight.
//!
//! ## Signature authority, not transaction authority
//!
//! [`Aggregator::submit_price`] takes no `require_auth` at all. A submission is
//! authorised by the Ed25519 signature over the canonical payload, so the
//! account that pays the fee carries no authority whatsoever. Anyone may relay
//! a signed price; only the key holder can produce one. That is what lets a
//! group of operators share one funded relayer without sharing any signing
//! power, and what makes a lost transaction re-relayable without re-signing.
//!
//! ## What closing a round means
//!
//! A round closes when it holds `quorum` distinct nodes *and* `min_weight_bps`
//! of total voting weight. Both, not either: a headcount alone can be bought
//! with fresh half-weight identities, and weight alone could let two proven
//! nodes speak for the network.
//!
//! At close, the weighted median is the published price. Every submission is
//! then measured against that median: inside `max_deviation_bps` earns
//! reputation and a reward from the pool, outside it costs reputation and
//! stake. The median is computed over *every* submission — that is what makes
//! it Byzantine-tolerant — but the published statistics (node count, oldest
//! timestamp, standard deviation, confidence) are computed over the in-band
//! submissions only, so a node that is being penalised cannot also widen the
//! interval that consumers rely on.
//!
//! ## Storage lifetimes
//!
//! Config and the feed index live in instance storage. Prices, history, nonces
//! and consumer balances are persistent and TTL-extended on write: a nonce
//! that expires would let a replayed submission through, and a price that
//! expires reads as "never published" to a consumer.

mod error;
mod events;
mod math;
mod message;
mod types;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod test;
#[cfg(test)]
mod test_vectors;

pub use error::AggregatorError;
pub use message::{DOMAIN_SEPARATOR, MESSAGE_LEN};
pub use types::*;

use soroban_sdk::auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation};
use soroban_sdk::{
    contract, contractclient, contractimpl, panic_with_error, token, Address, BytesN, Env, IntoVal,
    Symbol, Vec,
};

use events::{
    FeedConfigured, FeesForwarded, NodeAbsent, OutlierPenalized, PriceUpdated, RoundAbandoned,
    RoundFailed, SubmissionAccepted,
};

/// The slice of the registry this contract depends on.
///
/// Declared as a client trait rather than imported from the registry's wasm so
/// that the aggregator can be built, tested and deployed without a build
/// artefact of another contract on disk. The registry is addressed by
/// configuration, so an interface drift would surface as a failed cross
/// contract call — which is why the integration tests register both contracts
/// and exercise the real pair.
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    fn weight_of(env: Env, pubkey: BytesN<32>) -> u32;
    fn record_success(env: Env, pubkey: BytesN<32>, reward: i128);
    fn record_miss(env: Env, pubkey: BytesN<32>);
    fn penalize(env: Env, pubkey: BytesN<32>, reputation_delta: u32, slash_amount: i128);
    fn fund_rewards(env: Env, from: Address, amount: i128);
}

#[contract]
pub struct Aggregator;

#[contractimpl]
impl Aggregator {
    // -- lifecycle ----------------------------------------------------------

    /// One-time setup. Takes the whole [`Config`] as a struct rather than
    /// sixteen positional arguments, because a deployment script that
    /// transposes two of sixteen numbers is a deployment script that silently
    /// sets the slash amount to the reward.
    pub fn initialize(env: Env, config: Config) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, AggregatorError::AlreadyInitialized);
        }
        config.admin.require_auth();
        Self::validate_config(&env, &config);

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage()
            .instance()
            .set(&DataKey::Feeds, &Vec::<Symbol>::new(&env));
        env.storage().instance().set(&DataKey::RoundCounter, &0u64);
        env.storage().instance().set(&DataKey::Fees, &0i128);
    }

    /// Replace the configuration wholesale. Admin only.
    ///
    /// Wholesale rather than one setter per field: the parameters constrain
    /// each other (a round interval below the staleness window, a quorum above
    /// the node count) and validating them together is the only way to reject
    /// a combination that is individually plausible and jointly broken.
    pub fn set_config(env: Env, config: Config) {
        let current = Self::load_config(&env);
        current.admin.require_auth();
        Self::validate_config(&env, &config);
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn get_config(env: Env) -> Config {
        Self::load_config(&env)
    }

    /// Add a feed, or reconfigure an existing one. Admin only.
    pub fn set_feed(env: Env, feed: Symbol, enabled: bool, heartbeat: u64, min_nodes: u32) {
        let config = Self::load_config(&env);
        config.admin.require_auth();
        if heartbeat == 0 {
            panic_with_error!(&env, AggregatorError::InvalidConfig);
        }
        let mut feeds: Vec<Symbol> = env
            .storage()
            .instance()
            .get(&DataKey::Feeds)
            .unwrap_or_else(|| Vec::new(&env));
        if !feeds.contains(&feed) {
            feeds.push_back(feed.clone());
            env.storage().instance().set(&DataKey::Feeds, &feeds);
        }

        env.storage().instance().set(
            &DataKey::Feed(feed.clone()),
            &FeedConfig {
                feed: feed.clone(),
                enabled,
                heartbeat,
                min_nodes,
            },
        );

        FeedConfigured {
            feed,
            enabled,
            heartbeat,
            min_nodes,
        }
        .publish(&env);
    }

    pub fn feeds(env: Env) -> Vec<Symbol> {
        env.storage()
            .instance()
            .get(&DataKey::Feeds)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn feed_config(env: Env, feed: Symbol) -> FeedConfig {
        Self::load_feed(&env, &feed)
    }

    // -- submission ---------------------------------------------------------

    /// Verify and record one node's observation.
    ///
    /// Returns `true` when this submission was the one that closed the round —
    /// whether or not the round managed to publish a price. The caller is a
    /// relayer, and "the round is over" is the fact it needs; whether the
    /// network agreed is reported by events.
    ///
    /// Deliberately unauthenticated at the transaction level. See the module
    /// documentation.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_price(
        env: Env,
        feed: Symbol,
        pubkey: BytesN<32>,
        price: i128,
        timestamp: u64,
        confidence_bps: u32,
        nonce: u64,
        signature: BytesN<64>,
    ) -> bool {
        let config = Self::load_config(&env);
        let feed_config = Self::load_feed(&env, &feed);
        if !feed_config.enabled {
            panic_with_error!(&env, AggregatorError::FeedDisabled);
        }
        if price <= 0 {
            panic_with_error!(&env, AggregatorError::InvalidPrice);
        }

        let now = env.ledger().timestamp();
        if timestamp > now && timestamp - now > config.max_future_drift {
            panic_with_error!(&env, AggregatorError::FutureObservation);
        }
        if now > timestamp && now - timestamp > config.max_staleness {
            panic_with_error!(&env, AggregatorError::StaleObservation);
        }

        // The nonce is checked before the signature is verified. Both reject
        // the submission; checking the cheap one first keeps a replay flood
        // from being a way to spend the network's CPU budget.
        let nonce_key = DataKey::Nonce(pubkey.clone(), feed.clone());
        let last_nonce: u64 = env.storage().persistent().get(&nonce_key).unwrap_or(0);
        if nonce <= last_nonce {
            panic_with_error!(&env, AggregatorError::NonceNotIncreasing);
        }

        // Weight is read now and stored with the submission. A reputation
        // change between here and finalisation must not re-weight a vote that
        // has already been cast.
        let registry = RegistryClient::new(&env, &config.registry);
        let weight_bps = registry.weight_of(&pubkey);
        if weight_bps == 0 {
            panic_with_error!(&env, AggregatorError::NotAuthorizedNode);
        }

        let aggregator_id = message::contract_id_bytes(&env, &env.current_contract_address());
        let payload = message::price_message(
            &env,
            &aggregator_id,
            &feed,
            price,
            timestamp,
            confidence_bps,
            nonce,
        );
        // Traps on failure. A submission that does not verify never reaches
        // storage, so a forged signature costs the relayer a fee and changes
        // nothing else.
        env.crypto().ed25519_verify(&pubkey, &payload, &signature);

        env.storage().persistent().set(&nonce_key, &nonce);
        env.storage()
            .persistent()
            .extend_ttl(&nonce_key, TTL_THRESHOLD, TTL_EXTEND);

        let seen_key = DataKey::LastSeen(pubkey.clone());
        env.storage().persistent().set(&seen_key, &now);
        env.storage()
            .persistent()
            .extend_ttl(&seen_key, TTL_THRESHOLD, TTL_EXTEND);

        let mut round = Self::open_round(&env, &config, &feed, now);
        for existing in round.submissions.iter() {
            if existing.pubkey == pubkey {
                panic_with_error!(&env, AggregatorError::DuplicateSubmission);
            }
        }
        round.submissions.push_back(Submission {
            pubkey: pubkey.clone(),
            price,
            timestamp,
            confidence_bps,
            weight_bps,
        });

        SubmissionAccepted {
            feed: feed.clone(),
            pubkey,
            price,
            nonce,
            weight_bps,
            round_id: round.round_id,
        }
        .publish(&env);

        let needed = if feed_config.min_nodes > 0 {
            feed_config.min_nodes
        } else {
            config.quorum
        };
        let mut total_weight: u32 = 0;
        for s in round.submissions.iter() {
            total_weight = total_weight.saturating_add(s.weight_bps);
        }

        if round.submissions.len() >= needed && total_weight >= config.min_weight_bps {
            Self::finalize(&env, &config, &feed, &round, now);
            env.storage().persistent().remove(&DataKey::Round(feed));
            true
        } else {
            Self::save_round(&env, &feed, &round);
            false
        }
    }

    /// The round currently accepting submissions, if any.
    pub fn pending_round(env: Env, feed: Symbol) -> Option<PendingRound> {
        env.storage().persistent().get(&DataKey::Round(feed))
    }

    /// Charge a missed round to nodes that have been silent for longer than
    /// `absence_threshold`.
    ///
    /// Permissionless and explicitly batched. The alternative — sweeping the
    /// whole node set at the end of every round — makes the cost of closing a
    /// round grow with the size of the network, which punishes the feed for
    /// the network's success. Here the caller pays for the keys it names, one
    /// charge per silence, and the returned count says how many landed.
    pub fn sweep_absent(env: Env, pubkeys: Vec<BytesN<32>>) -> u32 {
        let config = Self::load_config(&env);
        let registry = RegistryClient::new(&env, &config.registry);
        let now = env.ledger().timestamp();
        let mut charged = 0u32;

        for pubkey in pubkeys.iter() {
            // A node with no weight is unknown, jailed or exiting. None of
            // those should be ground down further, and `record_miss` would
            // trap on a key the registry has never seen.
            if registry.weight_of(&pubkey) == 0 {
                continue;
            }

            let last_seen: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::LastSeen(pubkey.clone()))
                .unwrap_or(0);
            let last_swept: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::Swept(pubkey.clone()))
                .unwrap_or(0);
            let reference = last_seen.max(last_swept);

            if reference == 0 {
                // Never seen and never swept: there is no evidence of when the
                // silence began, so start the clock instead of assuming the
                // worst about a node that may have registered a moment ago.
                Self::mark_swept(&env, &pubkey, now);
                continue;
            }
            if now < reference || now - reference < config.absence_threshold {
                continue;
            }

            registry.record_miss(&pubkey);
            Self::mark_swept(&env, &pubkey, now);
            NodeAbsent {
                pubkey,
                last_seen,
                silent_for: now - reference,
            }
            .publish(&env);
            charged += 1;
        }
        charged
    }

    // -- reads --------------------------------------------------------------

    /// Current published price, or `None` if this feed has never published.
    ///
    /// A feed that has never published must not read as a price of zero in a
    /// consumer's contract, which is why this is an `Option` and not a
    /// defaulted value.
    pub fn get_price(env: Env, feed: Symbol) -> Option<PriceData> {
        env.storage().persistent().get(&DataKey::Price(feed))
    }

    /// Current price, rejected if older than `max_age` seconds.
    ///
    /// Prefer this over `get_price` plus a manual age check: the freshness
    /// requirement belongs in the same call that reads the value, or it
    /// eventually gets forgotten on some other code path.
    pub fn get_price_checked(env: Env, feed: Symbol, max_age: u64) -> PriceData {
        let price: PriceData = env
            .storage()
            .persistent()
            .get(&DataKey::Price(feed))
            .unwrap_or_else(|| panic_with_error!(&env, AggregatorError::NoPrice));
        let now = env.ledger().timestamp();
        if now > price.timestamp && now - price.timestamp > max_age {
            panic_with_error!(&env, AggregatorError::StalePrice);
        }
        price
    }

    /// Time-weighted average price over the last `window` seconds.
    ///
    /// Traps with `InsufficientHistory` when the retained observations do not
    /// span the whole window. A TWAP computed over a tenth of the window a
    /// caller asked for is not a conservative answer, it is a wrong one.
    pub fn get_twap(env: Env, feed: Symbol, window: u64) -> i128 {
        if window == 0 {
            panic_with_error!(&env, AggregatorError::InvalidWindow);
        }
        let history: Vec<Observation> = env
            .storage()
            .persistent()
            .get(&DataKey::History(feed.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        if history.is_empty() {
            panic_with_error!(&env, AggregatorError::NoPrice);
        }

        let now = env.ledger().timestamp();
        let window_start = now.saturating_sub(window);
        if history.get(0).unwrap().timestamp > window_start {
            panic_with_error!(&env, AggregatorError::InsufficientHistory);
        }

        let mut points: Vec<(u64, i128)> = Vec::new(&env);
        for obs in history.iter() {
            points.push_back((obs.timestamp, obs.price));
        }
        math::time_weighted_average(&points, window_start, now)
            .unwrap_or_else(|| panic_with_error!(&env, AggregatorError::InsufficientHistory))
    }

    /// The retained observation ring, oldest first.
    pub fn history(env: Env, feed: Symbol) -> Vec<Observation> {
        env.storage()
            .persistent()
            .get(&DataKey::History(feed))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Highest nonce accepted from a node for a feed. A node restored from a
    /// backup reads this to move its counter past anything already spent.
    pub fn last_nonce(env: Env, pubkey: BytesN<32>, feed: Symbol) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::Nonce(pubkey, feed))
            .unwrap_or(0)
    }

    /// Ledger close time, so a node can check its own clock against the chain
    /// rather than against itself.
    pub fn ledger_time(env: Env) -> u64 {
        env.ledger().timestamp()
    }

    // -- metering -----------------------------------------------------------

    /// Prepay for metered reads.
    pub fn deposit(env: Env, from: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, AggregatorError::InvalidAmount);
        }
        from.require_auth();
        let config = Self::load_config(&env);
        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let key = DataKey::Balance(from);
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(balance + amount));
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    pub fn balance(env: Env, consumer: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(consumer))
            .unwrap_or(0)
    }

    /// Return unspent prepaid balance.
    pub fn refund(env: Env, consumer: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, AggregatorError::InvalidAmount);
        }
        consumer.require_auth();
        let config = Self::load_config(&env);
        let key = DataKey::Balance(consumer.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if balance < amount {
            panic_with_error!(&env, AggregatorError::InsufficientBalance);
        }
        env.storage().persistent().set(&key, &(balance - amount));
        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &consumer,
            &amount,
        );
    }

    /// Read a price and pay for it out of prepaid balance.
    ///
    /// The free reads above stay free — a simulated read costs the network
    /// nothing and cannot be billed anyway. This is the entry point a consumer
    /// contract calls when it wants its usage to fund the operators it depends
    /// on, and it is the mechanism by which the reward pool refills without a
    /// foundation subsidy.
    pub fn get_price_metered(env: Env, consumer: Address, feed: Symbol, max_age: u64) -> PriceData {
        consumer.require_auth();
        let config = Self::load_config(&env);

        if config.read_fee > 0 {
            let key = DataKey::Balance(consumer.clone());
            let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);
            if balance < config.read_fee {
                panic_with_error!(&env, AggregatorError::InsufficientBalance);
            }
            env.storage()
                .persistent()
                .set(&key, &(balance - config.read_fee));

            let fees: i128 = env.storage().instance().get(&DataKey::Fees).unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::Fees, &(fees + config.read_fee));
        }

        Self::get_price_checked(env, feed, max_age)
    }

    /// Collected fees awaiting forwarding.
    pub fn collected_fees(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::Fees).unwrap_or(0)
    }

    /// Push collected read fees into the registry's reward pool.
    ///
    /// Permissionless: anyone may pay the fee to move money towards the people
    /// producing the data. Batched rather than forwarded per read, because a
    /// token transfer on every read would make metering cost more than the
    /// read it is charging for.
    pub fn forward_fees(env: Env) -> i128 {
        let config = Self::load_config(&env);
        let amount: i128 = env.storage().instance().get(&DataKey::Fees).unwrap_or(0);
        if amount <= 0 {
            return 0;
        }
        env.storage().instance().set(&DataKey::Fees, &0i128);

        // The registry pulls the tokens with `transfer`, one frame below this
        // one, so the aggregator's implicit authority as direct caller does
        // not reach it. This authorises exactly that one transfer, for exactly
        // this amount, and nothing else.
        let me = env.current_contract_address();
        env.authorize_as_current_contract(soroban_sdk::vec![
            &env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: config.token.clone(),
                    fn_name: Symbol::new(&env, "transfer"),
                    args: soroban_sdk::vec![
                        &env,
                        me.to_val(),
                        config.registry.to_val(),
                        amount.into_val(&env),
                    ],
                },
                sub_invocations: soroban_sdk::vec![&env],
            })
        ]);

        RegistryClient::new(&env, &config.registry).fund_rewards(&me, &amount);
        FeesForwarded { amount }.publish(&env);
        amount
    }

    // -- internals ----------------------------------------------------------

    fn load_config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, AggregatorError::NotInitialized))
    }

    fn load_feed(env: &Env, feed: &Symbol) -> FeedConfig {
        env.storage()
            .instance()
            .get(&DataKey::Feed(feed.clone()))
            .unwrap_or_else(|| panic_with_error!(env, AggregatorError::UnknownFeed))
    }

    fn validate_config(env: &Env, config: &Config) {
        let sane = config.quorum > 0
            && config.min_weight_bps > 0
            && config.max_deviation_bps > 0
            && config.max_staleness > 0
            && config.history_len > 0
            && config.reward_per_submission >= 0
            && config.outlier_slash >= 0
            && config.read_fee >= 0
            && config.round_timeout > 0
            && config.absence_threshold > 0;
        if !sane {
            panic_with_error!(env, AggregatorError::InvalidConfig);
        }
        // The registry is called cross-contract on every submission; an
        // account address there would fail at the first round rather than at
        // deployment.
        message::contract_id_bytes(env, &config.registry);
    }

    /// The open round for a feed, creating one if there is none and abandoning
    /// one that has been waiting too long.
    fn open_round(env: &Env, config: &Config, feed: &Symbol, now: u64) -> PendingRound {
        if let Some(round) = env
            .storage()
            .persistent()
            .get::<DataKey, PendingRound>(&DataKey::Round(feed.clone()))
        {
            if now.saturating_sub(round.opened_at) <= config.round_timeout {
                return round;
            }
            RoundAbandoned {
                feed: feed.clone(),
                round_id: round.round_id,
                num_submissions: round.submissions.len(),
                opened_at: round.opened_at,
            }
            .publish(env);
        }

        // A new round may not open until the previous publication is old
        // enough, which is what stops a node paying to republish the same
        // number as fast as it can build transactions.
        if let Some(price) = env
            .storage()
            .persistent()
            .get::<DataKey, PriceData>(&DataKey::Price(feed.clone()))
        {
            if now.saturating_sub(price.published_at) < config.min_round_interval {
                panic_with_error!(env, AggregatorError::RoundTooSoon);
            }
        }

        let round_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::RoundCounter)
            .unwrap_or(0)
            + 1;
        env.storage()
            .instance()
            .set(&DataKey::RoundCounter, &round_id);

        PendingRound {
            round_id,
            opened_at: now,
            submissions: Vec::new(env),
        }
    }

    fn save_round(env: &Env, feed: &Symbol, round: &PendingRound) {
        let key = DataKey::Round(feed.clone());
        env.storage().persistent().set(&key, round);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn mark_swept(env: &Env, pubkey: &BytesN<32>, now: u64) {
        let key = DataKey::Swept(pubkey.clone());
        env.storage().persistent().set(&key, &now);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    /// Close a round: take the median, publish, then settle every submitter
    /// against it.
    fn finalize(env: &Env, config: &Config, feed: &Symbol, round: &PendingRound, now: u64) {
        let mut pairs: Vec<(i128, u32)> = Vec::new(env);
        for s in round.submissions.iter() {
            pairs.push_back((s.price, s.weight_bps));
        }
        let median = math::weighted_median(env, &pairs)
            .unwrap_or_else(|| panic_with_error!(env, AggregatorError::MathOverflow));

        // In-band submissions decide what consumers are told; every submission
        // decided the median. Keeping those two sets apart is what stops a
        // node that is about to be penalised from also widening the confidence
        // interval its victims rely on.
        let mut in_band: Vec<i128> = Vec::new(env);
        let mut confidences: Vec<(i128, u32)> = Vec::new(env);
        let mut oldest: u64 = u64::MAX;
        let mut spread_bps: u32 = 0;

        let registry = RegistryClient::new(env, &config.registry);

        for s in round.submissions.iter() {
            let deviation = math::deviation_bps(s.price, median);
            if deviation <= config.max_deviation_bps {
                in_band.push_back(s.price);
                confidences.push_back((s.confidence_bps as i128, s.weight_bps));
                if s.timestamp < oldest {
                    oldest = s.timestamp;
                }
                if deviation > spread_bps {
                    spread_bps = deviation;
                }
                registry.record_success(&s.pubkey, &config.reward_per_submission);
            } else {
                registry.penalize(
                    &s.pubkey,
                    &config.outlier_rep_penalty,
                    &config.outlier_slash,
                );
                OutlierPenalized {
                    feed: feed.clone(),
                    pubkey: s.pubkey.clone(),
                    price: s.price,
                    median,
                    deviation_bps: deviation,
                    round_id: round.round_id,
                }
                .publish(env);
            }
        }

        // Possible when two distant submissions straddle the median: both are
        // outliers relative to a midpoint neither reported. The network did
        // not agree, so it publishes nothing — a stale price a consumer can
        // detect beats a fresh price nobody stands behind.
        if in_band.is_empty() {
            RoundFailed {
                feed: feed.clone(),
                round_id: round.round_id,
                median,
                num_submissions: round.submissions.len(),
            }
            .publish(env);
            return;
        }

        let deviation = math::stddev(&in_band)
            .unwrap_or_else(|| panic_with_error!(env, AggregatorError::MathOverflow));

        // The network's confidence is the wider of what the nodes claimed and
        // what they demonstrated. A node reporting a suspiciously tight
        // interval cannot narrow the published one below the disagreement the
        // round actually contains.
        let claimed = math::weighted_median(env, &confidences).unwrap_or(0);
        let claimed = if claimed < 0 { 0 } else { claimed as u32 };
        let confidence_bps = claimed.max(spread_bps);

        let price_data = PriceData {
            price: median,
            timestamp: oldest,
            num_nodes: in_band.len(),
            confidence_bps,
            deviation,
            round_id: round.round_id,
            published_at: now,
        };

        let price_key = DataKey::Price(feed.clone());
        env.storage().persistent().set(&price_key, &price_data);
        env.storage()
            .persistent()
            .extend_ttl(&price_key, TTL_THRESHOLD, TTL_EXTEND);

        Self::push_history(env, config, feed, now, median);

        PriceUpdated {
            feed: feed.clone(),
            price: median,
            round_id: round.round_id,
            num_nodes: price_data.num_nodes,
            confidence_bps,
            timestamp: oldest,
        }
        .publish(env);
    }

    /// Append to the TWAP ring, dropping the oldest entry when full.
    ///
    /// Keyed by publication time rather than observation time: a TWAP weights
    /// each price by how long it *stood as the network's answer*, and that
    /// interval starts when the round closed.
    fn push_history(env: &Env, config: &Config, feed: &Symbol, now: u64, price: i128) {
        let key = DataKey::History(feed.clone());
        let mut history: Vec<Observation> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));

        history.push_back(Observation {
            timestamp: now,
            price,
        });
        while history.len() > config.history_len {
            history.remove(0);
        }

        env.storage().persistent().set(&key, &history);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }
}
