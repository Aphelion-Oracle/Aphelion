#![no_std]
//! # Aphelion randomness beacon
//!
//! A public, unpredictable 32-byte value per round, produced by the same
//! staked node set that produces the prices — by commit and reveal.
//!
//! ```text
//!   open_round ──▶ commit ──────▶ reveal ───────▶ finalize
//!   (anyone,       (a node, with   (the same node,  (anyone, once the
//!    once the       H(secret)       with the        window closes or
//!    interval is    bound to this   secret)         everyone revealed)
//!    served)        round and key)
//! ```
//!
//! ## Why commit–reveal and not a VRF
//!
//! The usual answer to on-chain randomness is a verifiable random function, or
//! a threshold signature used as one: one party produces a value and a proof,
//! anybody checks the proof, and nobody could have produced a different value.
//! That is a better construction than this one, and Soroban cannot check it.
//! The host offers `ed25519_verify`, SHA-256 and Keccak; ECVRF needs
//! scalar–point arithmetic on the curve and a BLS threshold scheme needs
//! pairings. Implementing either in contract code would cost more per round
//! than the beacon is worth and would be a hand-rolled cryptographic
//! primitive in a contract holding stake, which is the worse of the two
//! problems.
//!
//! Commit–reveal needs a hash and a signature check, which is exactly what is
//! available, and it rests on a set of independently staked identities that
//! this network already has and already penalises. If Soroban later grows a
//! pairing or VRF host function, this contract is the thing to replace.
//!
//! ## What it guarantees, and what it does not
//!
//! The output is the XOR of every revealed secret, hashed. A participant who
//! committed before seeing anyone else's secret cannot steer it: their own
//! contribution was fixed, and XOR is order-independent, so they cannot grind
//! by choosing when to reveal either. As long as **one** secret was chosen by
//! somebody who did not know the rest, the output is unpredictable.
//!
//! The attack this construction cannot prevent is the last revealer's. Whoever
//! reveals last can compute the output before sending, and can therefore
//! choose between two outcomes: the one where they reveal, and the one where
//! they do not. That is one bit of influence per withholding participant, and
//! no amount of contract logic removes it — the information is genuinely
//! theirs at that moment.
//!
//! So it is priced rather than prevented. A node that commits and does not
//! reveal loses reputation and stake at finalisation, whoever calls it. A
//! consumer for whom one bit of adversarial choice is unacceptable should not
//! use a commit–reveal beacon — from anybody — and the documentation says so
//! rather than leaving it to be discovered.
//!
//! Two smaller consequences follow from the same fact, and both are deliberate:
//!
//! - **`min_participants` is a security parameter, not a liveness one.** It is
//!   the number of independent secrets the round insists on before it will
//!   publish anything. A round that falls short fails and publishes nothing,
//!   because a beacon assembled from too few parties is worse than no beacon:
//!   it looks exactly like a good one.
//! - **A failed round is recorded, not erased.** A consumer must be able to
//!   tell "this round will never have an answer" from "this round has not
//!   finished", and only one of those is worth waiting on.
//!
//! ## Why the node set
//!
//! Anyone could be allowed to commit. Restricting it to registered, unjailed
//! nodes buys two things: a Sybil cost, since contributing entropy requires
//! bonded stake rather than a fresh keypair, and something to take when a node
//! withholds a reveal. An open beacon can only ignore a no-show; this one can
//! charge for it.

#[cfg(test)]
extern crate std;

mod error;
mod events;
mod message;
mod types;

#[cfg(test)]
mod test;
#[cfg(test)]
mod test_vectors;

pub use error::RandomnessError;
pub use types::*;

use soroban_sdk::{
    contract, contractclient, contractimpl, panic_with_error, Bytes, BytesN, Env, Vec,
};

use events::{Committed, NoShow, Revealed, RoundFinalized, RoundOpened};

/// The slice of the registry this contract depends on.
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    /// Voting weight in basis points; zero for a node that is unknown, jailed
    /// or exiting. The same question the aggregator asks every round, so who
    /// counts is never maintained in two places.
    fn weight_of(env: Env, pubkey: BytesN<32>) -> u32;
    /// The no-show penalty specifically, not the dispute one. The registry
    /// authorises this against its `randomness` address, which a deployment
    /// points here with `set_randomness` — until it does, this call fails and
    /// no-shows go uncharged.
    fn slash_no_show(env: Env, pubkey: BytesN<32>, reputation_delta: u32, slash_amount: i128);
}

#[contract]
pub struct Randomness;

#[contractimpl]
impl Randomness {
    // -- lifecycle ----------------------------------------------------------

    pub fn initialize(env: Env, config: Config) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, RandomnessError::AlreadyInitialized);
        }
        config.admin.require_auth();
        Self::validate_config(&env, &config);

        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::RoundCounter, &0u64);
    }

    pub fn set_config(env: Env, config: Config) {
        let current = Self::load_config(&env);
        current.admin.require_auth();
        Self::validate_config(&env, &config);
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn get_config(env: Env) -> Config {
        Self::load_config(&env)
    }

    /// The range every governable parameter must stay inside. See the
    /// aggregator's function of the same name for why these are published.
    pub fn param_bounds(_env: Env) -> ParamBounds {
        ParamBounds {
            min_commit_window: MIN_COMMIT_WINDOW,
            max_commit_window: MAX_COMMIT_WINDOW,
            min_reveal_window: MIN_REVEAL_WINDOW,
            max_reveal_window: MAX_REVEAL_WINDOW,
            min_participants_floor: MIN_PARTICIPANTS_FLOOR,
            max_participants_floor: MAX_PARTICIPANTS_FLOOR,
            max_round_interval: MAX_ROUND_INTERVAL,
            max_no_show_rep_penalty: MAX_NO_SHOW_REP_PENALTY,
        }
    }

    // -- rounds -------------------------------------------------------------

    /// Open a round. Permissionless.
    ///
    /// Permissionless because a beacon nobody can start is a beacon that stops
    /// the first time whoever was starting it goes away, and there is nothing
    /// here for the caller to choose: the id is the next one, and both
    /// deadlines come from the configuration and the clock.
    pub fn open_round(env: Env) -> u64 {
        let config = Self::load_config(&env);
        let now = env.ledger().timestamp();

        // One round at a time. Overlapping rounds would let a participant who
        // has seen round N's reveals choose their secret for round N+1, which
        // is the whole thing commitment is for.
        if let Some(current) = Self::current_round_id(&env) {
            if let Some(round) = Self::round(&env, current) {
                let live = matches!(
                    round.status,
                    RoundStatus::Committing | RoundStatus::Revealing
                );
                if live && now <= round.reveal_deadline {
                    panic_with_error!(&env, RandomnessError::RoundInProgress);
                }
                // A round whose windows have closed but which nobody has
                // finalized must not block the beacon forever. It stays
                // finalizable -- the penalties and the output are still owed
                // -- and the next round starts regardless.
                if live && now.saturating_sub(round.opened_at) < config.min_round_interval {
                    panic_with_error!(&env, RandomnessError::RoundTooSoon);
                }
            }
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::RoundCounter)
            .unwrap_or(0)
            + 1;

        let round = Round {
            id,
            opened_at: now,
            commit_deadline: now + config.commit_window,
            reveal_deadline: now + config.commit_window + config.reveal_window,
            committed: Vec::new(&env),
            revealed: Vec::new(&env),
            accumulator: BytesN::from_array(&env, &[0u8; 32]),
            status: RoundStatus::Committing,
            output: BytesN::from_array(&env, &[0u8; 32]),
            finalized_at: 0,
        };

        env.storage().instance().set(&DataKey::RoundCounter, &id);
        env.storage().instance().set(&DataKey::CurrentRound, &id);
        Self::store_round(&env, &round);

        RoundOpened {
            round_id: id,
            opened_at: now,
            commit_deadline: round.commit_deadline,
            reveal_deadline: round.reveal_deadline,
        }
        .publish(&env);

        id
    }

    /// Commit to a secret for a round.
    ///
    /// `commitment` must be the SHA-256 of
    /// `"APHELION_RANDOM_V1" || contract_id || round_id || pubkey || secret`.
    /// Every field before the secret is load-bearing — see [`message`].
    ///
    /// The signature authorises the commitment as this node's; as with price
    /// submissions, the account paying for the transaction and the key
    /// authorising its contents are deliberately separable, so one funded
    /// relayer can serve several operators without holding any of their
    /// signing authority.
    pub fn commit(env: Env, pubkey: BytesN<32>, commitment: BytesN<32>, signature: BytesN<64>) {
        let config = Self::load_config(&env);
        let mut round = Self::live_round(&env);
        let now = env.ledger().timestamp();

        if round.status != RoundStatus::Committing || now > round.commit_deadline {
            panic_with_error!(&env, RandomnessError::NotCommitting);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Commitment(round.id, pubkey.clone()))
        {
            panic_with_error!(&env, RandomnessError::AlreadyCommitted);
        }

        // Registered, not jailed, not exiting. Asked of the registry rather
        // than tracked here, so a node jailed for lying about prices stops
        // contributing entropy in the same instant.
        if RegistryClient::new(&env, &config.registry).weight_of(&pubkey) == 0 {
            panic_with_error!(&env, RandomnessError::NotAuthorizedNode);
        }

        let contract = message::contract_id_bytes(&env, &env.current_contract_address());
        let payload = message::commit_message(&env, &contract, round.id, &commitment);
        env.crypto().ed25519_verify(&pubkey, &payload, &signature);

        env.storage()
            .persistent()
            .set(&DataKey::Commitment(round.id, pubkey.clone()), &commitment);
        env.storage().persistent().extend_ttl(
            &DataKey::Commitment(round.id, pubkey.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        round.committed.push_back(pubkey.clone());
        Self::store_round(&env, &round);

        Committed {
            round_id: round.id,
            node: pubkey,
            commitment,
        }
        .publish(&env);
    }

    /// Reveal the secret behind a commitment.
    ///
    /// Unsigned, deliberately. The secret only opens one commitment, and that
    /// commitment is already bound to this node by the preimage — so a third
    /// party who somehow learned the secret could submit it, and all they
    /// could do with that power is help the round finish on time.
    pub fn reveal(env: Env, pubkey: BytesN<32>, secret: BytesN<32>) {
        let mut round = Self::live_round(&env);
        let now = env.ledger().timestamp();

        // The commit window closing is what opens the reveal window. Both are
        // derived from the clock rather than from a status write, so nobody
        // has to call anything to move a round along.
        if now <= round.commit_deadline || now > round.reveal_deadline {
            panic_with_error!(&env, RandomnessError::NotRevealing);
        }

        let commitment: BytesN<32> = match env
            .storage()
            .persistent()
            .get(&DataKey::Commitment(round.id, pubkey.clone()))
        {
            Some(c) => c,
            None => panic_with_error!(&env, RandomnessError::DidNotCommit),
        };
        if env
            .storage()
            .persistent()
            .has(&DataKey::Revealed(round.id, pubkey.clone()))
        {
            panic_with_error!(&env, RandomnessError::AlreadyRevealed);
        }

        let contract = message::contract_id_bytes(&env, &env.current_contract_address());
        let preimage = message::commitment_preimage(&env, &contract, round.id, &pubkey, &secret);
        if env.crypto().sha256(&preimage).to_bytes() != commitment {
            panic_with_error!(&env, RandomnessError::BadReveal);
        }

        round.accumulator = Self::xor(&env, &round.accumulator, &secret);
        round.revealed.push_back(pubkey.clone());
        if round.status == RoundStatus::Committing {
            round.status = RoundStatus::Revealing;
        }
        Self::store_round(&env, &round);

        env.storage()
            .persistent()
            .set(&DataKey::Revealed(round.id, pubkey.clone()), &true);
        env.storage().persistent().extend_ttl(
            &DataKey::Revealed(round.id, pubkey.clone()),
            TTL_THRESHOLD,
            TTL_EXTEND,
        );

        Revealed {
            round_id: round.id,
            node: pubkey,
        }
        .publish(&env);
    }

    /// Close a round: publish the beacon, or record that there is none, and
    /// charge everyone who committed and did not reveal. Permissionless.
    ///
    /// Callable early only when every committer has revealed, because at that
    /// point waiting for the deadline adds nothing but latency. Otherwise the
    /// reveal window must have closed — finalising before it does would be
    /// deciding the output while somebody still had a right to change it.
    pub fn finalize(env: Env, round_id: u64) {
        let config = Self::load_config(&env);
        let mut round = match Self::round(&env, round_id) {
            Some(r) => r,
            None => panic_with_error!(&env, RandomnessError::UnknownRound),
        };
        if matches!(round.status, RoundStatus::Finalized | RoundStatus::Failed) {
            panic_with_error!(&env, RandomnessError::AlreadyFinalized);
        }

        let now = env.ledger().timestamp();
        let everyone_revealed =
            !round.committed.is_empty() && round.revealed.len() == round.committed.len();
        if now <= round.reveal_deadline && !everyone_revealed {
            panic_with_error!(&env, RandomnessError::NotReadyToFinalize);
        }

        // Charge the no-shows first, so that a round that fails still bills
        // the people who made it fail. Withholding to force a failure must not
        // be cheaper than withholding to flip a bit.
        let registry = RegistryClient::new(&env, &config.registry);
        for node in round.committed.iter() {
            if env
                .storage()
                .persistent()
                .has(&DataKey::Revealed(round.id, node.clone()))
            {
                continue;
            }
            registry.slash_no_show(&node, &config.no_show_rep_penalty, &config.no_show_slash);
            NoShow {
                round_id: round.id,
                node: node.clone(),
                rep_penalty: config.no_show_rep_penalty,
                slashed: config.no_show_slash,
            }
            .publish(&env);
        }

        if round.revealed.len() >= config.min_participants {
            // Hash the accumulator rather than publishing it raw. The XOR is
            // what makes the combination order-independent; the hash is what
            // stops a consumer who only cares about a few bits of the output
            // from reasoning backwards about individual secrets.
            let mut buf = Bytes::from_array(&env, &round.accumulator.to_array());
            buf.extend_from_array(&round.id.to_be_bytes());
            buf.extend_from_array(
                &message::contract_id_bytes(&env, &env.current_contract_address()).to_array(),
            );
            round.output = env.crypto().sha256(&buf).to_bytes();
            round.status = RoundStatus::Finalized;
        } else {
            round.status = RoundStatus::Failed;
        }
        round.finalized_at = now;
        Self::store_round(&env, &round);

        RoundFinalized {
            round_id: round.id,
            status: round.status,
            output: round.output.clone(),
            committed: round.committed.len(),
            revealed: round.revealed.len(),
        }
        .publish(&env);
    }

    // -- reads --------------------------------------------------------------

    pub fn get_round(env: Env, round_id: u64) -> Option<Round> {
        Self::round(&env, round_id)
    }

    pub fn round_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::RoundCounter)
            .unwrap_or(0)
    }

    /// The beacon for a round.
    ///
    /// `None` covers three different situations on purpose — no such round,
    /// not finished, finished without enough participants — and a consumer
    /// that needs to tell them apart should read `get_round` and look at the
    /// status. Collapsing them here keeps the common call a single question.
    pub fn random(env: Env, round_id: u64) -> Option<BytesN<32>> {
        match Self::round(&env, round_id) {
            Some(r) if r.status == RoundStatus::Finalized => Some(r.output),
            _ => None,
        }
    }

    /// The most recently finalized round's id and beacon.
    ///
    /// Walks back from the newest round rather than storing a pointer,
    /// because the newest round is usually the answer and a stored pointer is
    /// a second source of truth to keep correct. Bounded so that a long run of
    /// failed rounds cannot make this read unbounded work.
    pub fn latest(env: Env) -> Option<(u64, BytesN<32>)> {
        let newest = Self::round_count(env.clone());
        let floor = newest.saturating_sub(LATEST_SCAN_DEPTH);
        let mut id = newest;
        while id > floor {
            if let Some(r) = Self::round(&env, id) {
                if r.status == RoundStatus::Finalized {
                    return Some((id, r.output));
                }
            }
            id -= 1;
        }
        None
    }

    /// A uniform value in `0..bound`, derived from a round's beacon.
    ///
    /// Convenience, and worth being precise about: this reduces the first 16
    /// bytes of the beacon modulo `bound`. Modulo reduction is biased whenever
    /// `bound` does not divide the range, and here the range is 2^128 — so for
    /// any `bound` a caller can express in a `u64` the bias is at most
    /// 2^-64 of a share, which is far below the point at which anything else
    /// about this beacon is the weakest link. A caller who needs an unbiased
    /// draw from a bound near 2^128 should take `random` and rejection-sample
    /// it themselves.
    pub fn random_in_range(env: Env, round_id: u64, bound: u64) -> Option<u64> {
        if bound == 0 {
            panic_with_error!(&env, RandomnessError::InvalidBound);
        }
        let output = Self::random(env, round_id)?;
        let bytes = output.to_array();
        let mut head = [0u8; 16];
        head.copy_from_slice(&bytes[..16]);
        Some((u128::from_be_bytes(head) % bound as u128) as u64)
    }

    // -- internals ----------------------------------------------------------

    fn load_config(env: &Env) -> Config {
        match env.storage().instance().get(&DataKey::Config) {
            Some(c) => c,
            None => panic_with_error!(env, RandomnessError::NotInitialized),
        }
    }

    fn validate_config(env: &Env, config: &Config) {
        if config.no_show_slash < 0 {
            panic_with_error!(env, RandomnessError::InvalidConfig);
        }
        let in_range = (MIN_COMMIT_WINDOW..=MAX_COMMIT_WINDOW).contains(&config.commit_window)
            && (MIN_REVEAL_WINDOW..=MAX_REVEAL_WINDOW).contains(&config.reveal_window)
            && (MIN_PARTICIPANTS_FLOOR..=MAX_PARTICIPANTS_FLOOR).contains(&config.min_participants)
            && config.min_round_interval <= MAX_ROUND_INTERVAL
            && config.no_show_rep_penalty <= MAX_NO_SHOW_REP_PENALTY;
        if !in_range {
            panic_with_error!(env, RandomnessError::ParameterOutOfRange);
        }
        // The registry is called cross-contract on every commit; an account
        // address there would fail at the first commitment rather than here.
        message::contract_id_bytes(env, &config.registry);
    }

    fn current_round_id(env: &Env) -> Option<u64> {
        env.storage().instance().get(&DataKey::CurrentRound)
    }

    /// The round accepting commitments or reveals right now.
    fn live_round(env: &Env) -> Round {
        let id = match Self::current_round_id(env) {
            Some(id) => id,
            None => panic_with_error!(env, RandomnessError::NotCommitting),
        };
        match Self::round(env, id) {
            Some(r) => r,
            None => panic_with_error!(env, RandomnessError::UnknownRound),
        }
    }

    fn round(env: &Env, id: u64) -> Option<Round> {
        let key = DataKey::Round(id);
        let round: Option<Round> = env.storage().persistent().get(&key);
        if round.is_some() {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
        }
        round
    }

    fn store_round(env: &Env, round: &Round) {
        let key = DataKey::Round(round.id);
        env.storage().persistent().set(&key, round);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND);
    }

    fn xor(env: &Env, a: &BytesN<32>, b: &BytesN<32>) -> BytesN<32> {
        let (a, b) = (a.to_array(), b.to_array());
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = a[i] ^ b[i];
        }
        BytesN::from_array(env, &out)
    }
}

/// How far back `latest` will look for a finalized round.
///
/// A bound rather than a full scan: a long run of failed rounds must not turn
/// a read every consumer makes into work proportional to the contract's whole
/// history.
pub const LATEST_SCAN_DEPTH: u64 = 32;
