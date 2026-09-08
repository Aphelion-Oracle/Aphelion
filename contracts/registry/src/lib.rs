#![no_std]
//! # Aphelion node registry
//!
//! The registry answers one question for the rest of the network: *how much
//! should this node's opinion count?* Everything it stores — stake,
//! reputation, status — exists to produce the `weight_bps` that the aggregator
//! applies when it takes a weighted median.
//!
//! ## Why identity is a public key, not an address
//!
//! A node is identified by its Ed25519 public key. Submissions are
//! authenticated by signature over a canonical payload, so the Stellar account
//! that pays for the transaction carries no authority at all. Three things
//! follow, and all three are deliberate:
//!
//! * Several operators can share one funded relayer account without sharing
//!   any signing power.
//! * A node's signing key can live somewhere the fee-paying key does not.
//! * A submission can be re-relayed by anyone if the original transaction is
//!   lost, without the node having to re-sign.
//!
//! ## Why stake and reputation are both needed
//!
//! Stake alone prices an attack but does not price *repetition*: a
//! well-capitalised attacker can be wrong repeatedly as long as they can
//! afford it. Reputation alone is free to farm: spin up identities, behave for
//! a while, defect at the profitable moment. Together, an attacker must both
//! bond capital and behave correctly for a sustained period before their
//! influence is worth anything — and defecting destroys the second while
//! costing them the first.

mod error;
mod types;

#[cfg(test)]
mod test;

pub use error::RegistryError;
pub use types::*;

use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short, token, Address, BytesN, Env,
    Symbol, Vec,
};

#[contract]
pub struct Registry;

#[contractimpl]
impl Registry {
    /// One-time setup. `aggregator` and `slasher` may point at the admin
    /// initially and be repointed once those contracts are deployed, which is
    /// how the three-contract deployment bootstraps without a circular
    /// dependency.
    pub fn initialize(
        env: Env,
        admin: Address,
        aggregator: Address,
        slasher: Address,
        token: Address,
        min_stake: i128,
        unbonding_period: u64,
    ) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, RegistryError::AlreadyInitialized);
        }
        if min_stake <= 0 {
            panic_with_error!(&env, RegistryError::InvalidAmount);
        }
        admin.require_auth();

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                aggregator,
                slasher,
                token,
                min_stake,
                unbonding_period,
            },
        );
        env.storage()
            .instance()
            .set(&DataKey::NodeIndex, &Vec::<BytesN<32>>::new(&env));
        env.storage().instance().set(&DataKey::RewardPool, &0i128);
        env.storage().instance().set(&DataKey::SlashPool, &0i128);
    }

    // -- administration -----------------------------------------------------

    pub fn set_aggregator(env: Env, aggregator: Address) {
        let mut config = Self::config(&env);
        config.admin.require_auth();
        config.aggregator = aggregator;
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn set_slasher(env: Env, slasher: Address) {
        let mut config = Self::config(&env);
        config.admin.require_auth();
        config.slasher = slasher;
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn set_min_stake(env: Env, min_stake: i128) {
        if min_stake <= 0 {
            panic_with_error!(&env, RegistryError::InvalidAmount);
        }
        let mut config = Self::config(&env);
        config.admin.require_auth();
        config.min_stake = min_stake;
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn set_admin(env: Env, new_admin: Address) {
        let mut config = Self::config(&env);
        config.admin.require_auth();
        config.admin = new_admin;
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn get_config(env: Env) -> Config {
        Self::config(&env)
    }

    // -- registration and stake --------------------------------------------

    /// Bond stake and join the network.
    ///
    /// The stake is transferred to this contract, not merely attested, so that
    /// slashing is a matter of arithmetic on funds already held rather than a
    /// claim against an account that may be empty by the time it matters.
    pub fn register(env: Env, owner: Address, pubkey: BytesN<32>, stake: i128) {
        owner.require_auth();
        let config = Self::config(&env);

        if stake < config.min_stake {
            panic_with_error!(&env, RegistryError::StakeTooLow);
        }
        if env.storage().persistent().has(&DataKey::Node(pubkey.clone())) {
            panic_with_error!(&env, RegistryError::NodeAlreadyRegistered);
        }

        token::Client::new(&env, &config.token).transfer(
            &owner,
            &env.current_contract_address(),
            &stake,
        );

        let node = Node {
            pubkey: pubkey.clone(),
            owner: owner.clone(),
            stake,
            reputation: STARTING_REPUTATION,
            status: NodeStatus::Active,
            registered_at: env.ledger().timestamp(),
            last_submission: 0,
            consecutive_misses: 0,
            total_rewards: 0,
            total_slashed: 0,
            unbonding_until: 0,
        };
        Self::save_node(&env, &node);

        let mut index: Vec<BytesN<32>> = env
            .storage()
            .instance()
            .get(&DataKey::NodeIndex)
            .unwrap_or_else(|| Vec::new(&env));
        index.push_back(pubkey.clone());
        env.storage().instance().set(&DataKey::NodeIndex, &index);

        env.events()
            .publish((symbol_short!("registerd"), pubkey), (owner, stake));
    }

    /// Add to an existing bond. Raises the cost of misbehaving, and is the
    /// only way back above `min_stake` after a slash.
    pub fn add_stake(env: Env, pubkey: BytesN<32>, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RegistryError::InvalidAmount);
        }
        let config = Self::config(&env);
        let mut node = Self::node(&env, &pubkey);
        node.owner.require_auth();

        token::Client::new(&env, &config.token).transfer(
            &node.owner,
            &env.current_contract_address(),
            &amount,
        );
        node.stake += amount;

        // Topping back up above the minimum releases a node from jail, but
        // does not restore its reputation: capital buys a second chance, not
        // a clean record.
        if node.status == NodeStatus::Jailed
            && node.stake >= config.min_stake
            && node.reputation >= JAIL_THRESHOLD
        {
            node.status = NodeStatus::Active;
        }
        Self::save_node(&env, &node);

        env.events()
            .publish((symbol_short!("stake_add"), pubkey), amount);
    }

    /// Begin exiting. The node stops voting immediately but its stake stays
    /// locked for `unbonding_period`, which is the window in which a dispute
    /// over its past submissions can still reach it.
    pub fn request_unbond(env: Env, pubkey: BytesN<32>) {
        let config = Self::config(&env);
        let mut node = Self::node(&env, &pubkey);
        node.owner.require_auth();

        node.status = NodeStatus::Exiting;
        node.unbonding_until = env.ledger().timestamp() + config.unbonding_period;
        Self::save_node(&env, &node);

        env.events()
            .publish((symbol_short!("unbonding"), pubkey), node.unbonding_until);
    }

    /// Withdraw stake after the unbonding period.
    pub fn withdraw(env: Env, pubkey: BytesN<32>) -> i128 {
        let config = Self::config(&env);
        let node = Self::node(&env, &pubkey);
        node.owner.require_auth();

        if node.status != NodeStatus::Exiting || env.ledger().timestamp() < node.unbonding_until {
            panic_with_error!(&env, RegistryError::StillBonded);
        }

        let amount = node.stake;
        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &node.owner,
                &amount,
            );
        }

        env.storage()
            .persistent()
            .remove(&DataKey::Node(pubkey.clone()));
        let index: Vec<BytesN<32>> = env
            .storage()
            .instance()
            .get(&DataKey::NodeIndex)
            .unwrap_or_else(|| Vec::new(&env));
        let mut remaining = Vec::new(&env);
        for key in index.iter() {
            if key != pubkey {
                remaining.push_back(key);
            }
        }
        env.storage().instance().set(&DataKey::NodeIndex, &remaining);

        env.events()
            .publish((symbol_short!("withdrawn"), pubkey), (node.owner, amount));
        amount
    }

    // -- reward pool --------------------------------------------------------

    /// Anyone may top up the pool that pays node rewards. Used by the
    /// foundation during bootstrapping and, later, by the aggregator forwarding
    /// consumer fees.
    pub fn fund_rewards(env: Env, from: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RegistryError::InvalidAmount);
        }
        from.require_auth();
        let config = Self::config(&env);

        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let pool: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPool)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPool, &(pool + amount));

        env.events().publish((symbol_short!("funded"),), amount);
    }

    pub fn reward_pool(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::RewardPool)
            .unwrap_or(0)
    }

    pub fn slash_pool(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashPool)
            .unwrap_or(0)
    }

    // -- called by the aggregator ------------------------------------------

    /// Credit a node for a submission that landed inside the consensus band.
    ///
    /// The reward is paid from the pool if it can cover it, and skipped
    /// otherwise. An empty pool must not be able to stall consensus — a round
    /// that cannot pay is still a round that produced a correct price.
    pub fn record_success(env: Env, pubkey: BytesN<32>, reward: i128) {
        let config = Self::config(&env);
        config.aggregator.require_auth();

        let mut node = Self::node(&env, &pubkey);
        node.reputation = (node.reputation + REPUTATION_REWARD).min(MAX_REPUTATION);
        node.last_submission = env.ledger().timestamp();
        node.consecutive_misses = 0;

        if node.status == NodeStatus::Jailed
            && node.reputation >= JAIL_THRESHOLD
            && node.stake >= config.min_stake
        {
            node.status = NodeStatus::Active;
            env.events().publish((symbol_short!("unjailed"), pubkey.clone()), node.reputation);
        }

        let pool: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPool)
            .unwrap_or(0);
        if reward > 0 && pool >= reward {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &node.owner,
                &reward,
            );
            env.storage()
                .instance()
                .set(&DataKey::RewardPool, &(pool - reward));
            node.total_rewards += reward;
        }

        Self::save_node(&env, &node);
    }

    /// Note that a node did not take part in a round it could have.
    ///
    /// Missing rounds is not slashable — an operator's server can be down for
    /// honest reasons — but it does erode weight, so a node that has quietly
    /// stopped working stops influencing the median before anyone notices.
    pub fn record_miss(env: Env, pubkey: BytesN<32>) {
        let config = Self::config(&env);
        config.aggregator.require_auth();

        let mut node = Self::node(&env, &pubkey);
        node.consecutive_misses += 1;
        node.reputation = node.reputation.saturating_sub(REPUTATION_MISS);
        Self::maybe_jail(&env, &mut node);
        Self::save_node(&env, &node);
    }

    /// Penalise a node for a submission the aggregator mechanically judged to
    /// be outside the consensus band.
    ///
    /// Deliberately separate from [`Self::slash`] rather than one function
    /// with a role check. Two callers with different authority and different
    /// evidentiary standards -- an arithmetic outlier check versus a human
    /// vote -- should not share an entry point where a mistake in a role test
    /// silently grants one the other's power.
    pub fn penalize(env: Env, pubkey: BytesN<32>, reputation_delta: u32, slash_amount: i128) {
        let config = Self::config(&env);
        config.aggregator.require_auth();
        Self::apply_penalty(&env, pubkey, reputation_delta, slash_amount, symbol_short!("outlier"));
    }

    /// Penalise a node following a resolved dispute. Callable only by the
    /// slashing contract.
    pub fn slash(env: Env, pubkey: BytesN<32>, reputation_delta: u32, slash_amount: i128) {
        let config = Self::config(&env);
        config.slasher.require_auth();
        Self::apply_penalty(&env, pubkey, reputation_delta, slash_amount, symbol_short!("dispute"));
    }

    // -- reads --------------------------------------------------------------

    /// The node's record, or `None` if it is not registered.
    ///
    /// Returning an option rather than trapping matters: the aggregator calls
    /// this on every submission, including submissions from keys that were
    /// never registered, and a trap there would be a denial-of-service vector.
    pub fn get_node(env: Env, pubkey: BytesN<32>) -> Option<NodeView> {
        env.storage()
            .persistent()
            .get::<DataKey, Node>(&DataKey::Node(pubkey))
            .map(|node| NodeView {
                weight_bps: weight_for(&node.status, node.reputation),
                pubkey: node.pubkey,
                owner: node.owner,
                stake: node.stake,
                reputation: node.reputation,
                status: node.status,
                last_submission: node.last_submission,
                consecutive_misses: node.consecutive_misses,
                total_rewards: node.total_rewards,
                total_slashed: node.total_slashed,
                unbonding_until: node.unbonding_until,
            })
    }

    /// Voting weight in basis points; zero for an unknown or jailed node.
    pub fn weight_of(env: Env, pubkey: BytesN<32>) -> u32 {
        env.storage()
            .persistent()
            .get::<DataKey, Node>(&DataKey::Node(pubkey))
            .map(|n| weight_for(&n.status, n.reputation))
            .unwrap_or(0)
    }

    pub fn list_nodes(env: Env) -> Vec<BytesN<32>> {
        env.storage()
            .instance()
            .get(&DataKey::NodeIndex)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Total weight currently available. The aggregator uses this to tell a
    /// quorum failure ("not enough nodes reported") from a network that has
    /// too few healthy nodes to reach quorum at all.
    pub fn total_weight(env: Env) -> u32 {
        let index: Vec<BytesN<32>> = env
            .storage()
            .instance()
            .get(&DataKey::NodeIndex)
            .unwrap_or_else(|| Vec::new(&env));
        let mut total = 0u32;
        for key in index.iter() {
            if let Some(node) = env
                .storage()
                .persistent()
                .get::<DataKey, Node>(&DataKey::Node(key))
            {
                total = total.saturating_add(weight_for(&node.status, node.reputation));
            }
        }
        total
    }

    // -- internals ----------------------------------------------------------

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, RegistryError::NotInitialized))
    }

    fn node(env: &Env, pubkey: &BytesN<32>) -> Node {
        env.storage()
            .persistent()
            .get(&DataKey::Node(pubkey.clone()))
            .unwrap_or_else(|| panic_with_error!(env, RegistryError::NodeNotFound))
    }

    fn save_node(env: &Env, node: &Node) {
        env.storage()
            .persistent()
            .set(&DataKey::Node(node.pubkey.clone()), node);
        // Node records must outlive quiet periods: a node that stops
        // submitting is exactly the record a dispute needs later.
        env.storage().persistent().extend_ttl(
            &DataKey::Node(node.pubkey.clone()),
            NODE_TTL_THRESHOLD,
            NODE_TTL_EXTEND,
        );
    }

    fn maybe_jail(env: &Env, node: &mut Node) {
        if node.status == NodeStatus::Active && node.reputation < JAIL_THRESHOLD {
            node.status = NodeStatus::Jailed;
            env.events()
                .publish((symbol_short!("jailed"), node.pubkey.clone()), node.reputation);
        }
    }

    /// Shared body of [`Self::penalize`] and [`Self::slash`]. Authorisation is
    /// the caller's responsibility and has already happened by this point.
    fn apply_penalty(
        env: &Env,
        pubkey: BytesN<32>,
        reputation_delta: u32,
        slash_amount: i128,
        reason: Symbol,
    ) {
        if slash_amount < 0 {
            panic_with_error!(env, RegistryError::InvalidAmount);
        }
        let mut node = Self::node(env, &pubkey);
        node.reputation = node.reputation.saturating_sub(reputation_delta);

        // Seizing is capped at the remaining stake rather than trapping. A
        // node whose stake has already been reduced by an earlier penalty
        // should still take the reputation hit for a second offence; trapping
        // here would let a nearly-empty node block its own punishment.
        let seized = slash_amount.min(node.stake);
        if seized > 0 {
            node.stake -= seized;
            node.total_slashed += seized;
            let pool: i128 = env.storage().instance().get(&DataKey::SlashPool).unwrap_or(0);
            env.storage().instance().set(&DataKey::SlashPool, &(pool + seized));
        }

        Self::maybe_jail(env, &mut node);
        Self::save_node(env, &node);

        env.events()
            .publish((symbol_short!("penalized"), pubkey), (reason, reputation_delta, seized));
    }
}

/// Ledger units. A node record is extended by roughly 30 days whenever it is
/// within 7 days of expiry.
const NODE_TTL_THRESHOLD: u32 = 120_960; // ~7 days at 5s ledgers
const NODE_TTL_EXTEND: u32 = 518_400; // ~30 days
