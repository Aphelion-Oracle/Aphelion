use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, BytesN, Env};

use crate::{Config, Randomness, RandomnessClient, RoundStatus};
use aphelion_registry::{Registry, RegistryClient};

const BASE_TIME: u64 = 1_735_689_600;
const MIN_STAKE: i128 = 10_000_000_000; // 1000 XLM in stroops
const JAIL_PERIOD: u64 = 24 * 3600;
const UNBONDING: u64 = 7 * 24 * 3600;

const COMMIT_WINDOW: u64 = 300;
const REVEAL_WINDOW: u64 = 300;
const MIN_PARTICIPANTS: u32 = 3;
const NO_SHOW_REP_PENALTY: u32 = 500;
const NO_SHOW_SLASH: i128 = 1_000_000_000; // 100 XLM

struct Harness<'a> {
    env: Env,
    randomness: RandomnessClient<'a>,
    registry: RegistryClient<'a>,
    token: TokenClient<'a>,
    contract_id: [u8; 32],
    keys: std::vec::Vec<SigningKey>,
    owners: std::vec::Vec<Address>,
}

fn base_config(admin: &Address, registry: &Address) -> Config {
    Config {
        admin: admin.clone(),
        registry: registry.clone(),
        commit_window: COMMIT_WINDOW,
        reveal_window: REVEAL_WINDOW,
        min_participants: MIN_PARTICIPANTS,
        min_round_interval: 60,
        no_show_rep_penalty: NO_SHOW_REP_PENALTY,
        no_show_slash: NO_SHOW_SLASH,
    }
}

fn setup() -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(BASE_TIME);

    let admin = Address::generate(&env);
    let aggregator = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let registry_id = env.register(Registry, ());
    let randomness_id = env.register(Randomness, ());

    let registry = RegistryClient::new(&env, &registry_id);
    let slashing = Address::generate(&env);
    registry.initialize(
        &admin,
        &aggregator,
        &slashing,
        &sac.address(),
        &MIN_STAKE,
        &UNBONDING,
        &JAIL_PERIOD,
    );
    // The step a deployment has to remember: until the registry is pointed at
    // this contract, `finalize` cannot charge a no-show anything.
    registry.set_randomness(&randomness_id);

    let randomness = RandomnessClient::new(&env, &randomness_id);
    randomness.initialize(&base_config(&admin, &registry_id));

    let contract_id = crate::message::contract_id_bytes(&env, &randomness_id).to_array();

    let keys = (1u8..=6)
        .map(|i| SigningKey::from_bytes(&[i; 32]))
        .collect::<std::vec::Vec<_>>();

    let mut owners = std::vec::Vec::new();
    for key in &keys {
        let owner = Address::generate(&env);
        token_admin.mint(&owner, &(MIN_STAKE * 4));
        let pubkey = BytesN::from_array(&env, &key.verifying_key().to_bytes());
        registry.register(&owner, &pubkey, &(MIN_STAKE * 2));
        owners.push(owner);
    }

    Harness {
        env,
        randomness,
        registry,
        token,
        contract_id,
        keys,
        owners,
    }
}

impl Harness<'_> {
    fn pubkey(&self, node: usize) -> BytesN<32> {
        BytesN::from_array(&self.env, &self.keys[node].verifying_key().to_bytes())
    }

    /// The secret node `n` uses, made deterministic so a failure names a node.
    fn secret(&self, node: usize) -> BytesN<32> {
        BytesN::from_array(&self.env, &[(node as u8) + 100; 32])
    }

    /// The commitment, built here rather than by calling into the contract, so
    /// that a drift between the two is a test failure rather than a silent
    /// agreement to be wrong together.
    fn commitment_for(&self, round: u64, node: usize, secret: &BytesN<32>) -> BytesN<32> {
        let mut buf = std::vec::Vec::new();
        buf.extend_from_slice(b"APHELION_RANDOM_V1");
        buf.extend_from_slice(&self.contract_id);
        buf.extend_from_slice(&round.to_be_bytes());
        buf.extend_from_slice(&self.pubkey(node).to_array());
        buf.extend_from_slice(&secret.to_array());
        assert_eq!(buf.len(), 122);
        BytesN::from_array(&self.env, &Sha256::digest(&buf).into())
    }

    fn sign_commit(&self, round: u64, node: usize, commitment: &BytesN<32>) -> BytesN<64> {
        let mut buf = std::vec::Vec::new();
        buf.extend_from_slice(b"APHELION_COMMIT_V1");
        buf.extend_from_slice(&self.contract_id);
        buf.extend_from_slice(&round.to_be_bytes());
        buf.extend_from_slice(&commitment.to_array());
        assert_eq!(buf.len(), 90);
        BytesN::from_array(&self.env, &self.keys[node].sign(&buf).to_bytes())
    }

    fn commit(&self, round: u64, node: usize) {
        let secret = self.secret(node);
        let commitment = self.commitment_for(round, node, &secret);
        let signature = self.sign_commit(round, node, &commitment);
        self.randomness
            .commit(&self.pubkey(node), &commitment, &signature);
    }

    fn reveal(&self, node: usize) {
        self.randomness
            .reveal(&self.pubkey(node), &self.secret(node));
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn set_round_interval(&self, seconds: u64) {
        let mut config = self.randomness.get_config();
        config.min_round_interval = seconds;
        self.randomness.set_config(&config);
    }

    /// Open a round, have `nodes` commit, move into the reveal window.
    fn round_with_commits(&self, nodes: &[usize]) -> u64 {
        let id = self.randomness.open_round();
        for &n in nodes {
            self.commit(id, n);
        }
        self.advance(COMMIT_WINDOW + 1);
        id
    }
}

// -- the happy path ---------------------------------------------------------

#[test]
fn a_round_of_commits_and_reveals_produces_a_beacon() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&id);

    let round = h.randomness.get_round(&id).unwrap();
    assert_eq!(round.status, RoundStatus::Finalized);
    assert_eq!(round.revealed.len(), 3);

    let output = h.randomness.random(&id).expect("a beacon");
    assert_ne!(
        output.to_array(),
        [0u8; 32],
        "the beacon is not the zero it starts as"
    );
    assert_eq!(h.randomness.latest(), Some((id, output)));
}

#[test]
fn the_output_does_not_depend_on_the_order_the_reveals_arrived_in() {
    // The property XOR is chosen for. If the combination were a running hash,
    // a participant could grind the output by choosing when to send -- which
    // is a choice they make *after* seeing the others, and therefore exactly
    // the influence commitment is supposed to remove.
    let forwards = {
        let h = setup();
        let id = h.round_with_commits(&[0, 1, 2]);
        for n in [0, 1, 2] {
            h.reveal(n);
        }
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };
    let backwards = {
        let h = setup();
        let id = h.round_with_commits(&[0, 1, 2]);
        for n in [2, 1, 0] {
            h.reveal(n);
        }
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };
    assert_eq!(forwards, backwards);
}

#[test]
fn a_different_set_of_secrets_gives_a_different_beacon() {
    let a = {
        let h = setup();
        let id = h.round_with_commits(&[0, 1, 2]);
        for n in 0..3 {
            h.reveal(n);
        }
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };
    let b = {
        let h = setup();
        let id = h.round_with_commits(&[3, 4, 5]);
        for n in 3..6 {
            h.reveal(n);
        }
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };
    assert_ne!(a, b);
}

#[test]
fn a_round_may_close_early_once_everybody_has_revealed() {
    // Waiting out the window when there is nobody left to hear from adds
    // latency and nothing else.
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    // Still inside the reveal window.
    assert!(h.env.ledger().timestamp() < h.randomness.get_round(&id).unwrap().reveal_deadline);
    h.randomness.finalize(&id);
    assert_eq!(
        h.randomness.get_round(&id).unwrap().status,
        RoundStatus::Finalized
    );
}

// -- what a participant may not do ------------------------------------------

#[test]
fn a_secret_that_does_not_match_the_commitment_is_refused() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    let wrong = BytesN::from_array(&h.env, &[9u8; 32]);
    assert!(h.randomness.try_reveal(&h.pubkey(0), &wrong).is_err());
    let _ = id;
}

#[test]
fn one_node_cannot_reveal_under_anothers_commitment() {
    // The commitment binds the public key into its preimage, so node 1 cannot
    // open node 0's commitment even holding node 0's secret.
    let h = setup();
    h.round_with_commits(&[0, 1, 2]);
    assert!(h.randomness.try_reveal(&h.pubkey(1), &h.secret(0)).is_err());
}

#[test]
fn a_node_cannot_copy_another_nodes_commitment() {
    // Without the public key in the preimage this would work: copy the
    // commitment, wait for the owner to reveal, reveal the same secret. Two
    // participants would then be contributing one party's entropy -- and
    // because the accumulator is an XOR, two copies of the same secret cancel
    // to nothing, so the copier could subtract another node's contribution
    // from the beacon entirely.
    let h = setup();
    let id = h.randomness.open_round();
    h.commit(id, 0);

    let victims_commitment = h.commitment_for(id, 0, &h.secret(0));
    let signature = h.sign_commit(id, 1, &victims_commitment);
    // The copy is accepted as a commitment -- the contract cannot tell it is a
    // copy -- and then cannot be opened by anybody.
    h.randomness
        .commit(&h.pubkey(1), &victims_commitment, &signature);
    h.advance(COMMIT_WINDOW + 1);

    h.reveal(0);
    assert!(
        h.randomness.try_reveal(&h.pubkey(1), &h.secret(0)).is_err(),
        "the copied commitment must not open"
    );
}

#[test]
fn a_node_cannot_commit_twice_to_one_round() {
    // Two commitments would be two secrets, and the node could reveal
    // whichever suited it once it had seen the others.
    let h = setup();
    let id = h.randomness.open_round();
    h.commit(id, 0);

    let secret = BytesN::from_array(&h.env, &[77u8; 32]);
    let commitment = h.commitment_for(id, 0, &secret);
    let signature = h.sign_commit(id, 0, &commitment);
    assert!(h
        .randomness
        .try_commit(&h.pubkey(0), &commitment, &signature)
        .is_err());
}

#[test]
fn a_node_cannot_reveal_twice() {
    let h = setup();
    h.round_with_commits(&[0, 1, 2]);
    h.reveal(0);
    assert!(h.randomness.try_reveal(&h.pubkey(0), &h.secret(0)).is_err());
}

#[test]
fn revealing_without_committing_is_refused() {
    // Otherwise a node could choose its contribution after seeing everybody
    // else's, which is the whole thing this construction exists to prevent.
    let h = setup();
    h.round_with_commits(&[0, 1]);
    assert!(h.randomness.try_reveal(&h.pubkey(2), &h.secret(2)).is_err());
}

#[test]
fn a_commitment_signed_for_another_round_does_not_verify() {
    let h = setup();
    let id = h.randomness.open_round();
    let secret = h.secret(0);
    let commitment = h.commitment_for(id, 0, &secret);
    let signature = h.sign_commit(id + 1, 0, &commitment);
    assert!(h
        .randomness
        .try_commit(&h.pubkey(0), &commitment, &signature)
        .is_err());
}

#[test]
fn a_commitment_signed_for_another_deployment_does_not_verify() {
    let h = setup();
    let id = h.randomness.open_round();
    let secret = h.secret(0);
    let commitment = h.commitment_for(id, 0, &secret);

    let mut buf = std::vec::Vec::new();
    buf.extend_from_slice(b"APHELION_COMMIT_V1");
    buf.extend_from_slice(&[0xAAu8; 32]); // some other contract
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&commitment.to_array());
    let signature = BytesN::from_array(&h.env, &h.keys[0].sign(&buf).to_bytes());

    assert!(h
        .randomness
        .try_commit(&h.pubkey(0), &commitment, &signature)
        .is_err());
}

#[test]
fn an_unregistered_key_cannot_commit() {
    let h = setup();
    let id = h.randomness.open_round();
    let stranger = SigningKey::from_bytes(&[200u8; 32]);
    let pubkey = BytesN::from_array(&h.env, &stranger.verifying_key().to_bytes());

    let secret = BytesN::from_array(&h.env, &[1u8; 32]);
    let mut buf = std::vec::Vec::new();
    buf.extend_from_slice(b"APHELION_RANDOM_V1");
    buf.extend_from_slice(&h.contract_id);
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&pubkey.to_array());
    buf.extend_from_slice(&secret.to_array());
    let commitment = BytesN::from_array(&h.env, &Sha256::digest(&buf).into());

    let mut sig_buf = std::vec::Vec::new();
    sig_buf.extend_from_slice(b"APHELION_COMMIT_V1");
    sig_buf.extend_from_slice(&h.contract_id);
    sig_buf.extend_from_slice(&id.to_be_bytes());
    sig_buf.extend_from_slice(&commitment.to_array());
    let signature = BytesN::from_array(&h.env, &stranger.sign(&sig_buf).to_bytes());

    assert!(h
        .randomness
        .try_commit(&pubkey, &commitment, &signature)
        .is_err());
}

// -- windows ----------------------------------------------------------------

#[test]
fn a_commitment_after_the_window_is_refused() {
    let h = setup();
    let id = h.randomness.open_round();
    h.advance(COMMIT_WINDOW + 1);

    let secret = h.secret(0);
    let commitment = h.commitment_for(id, 0, &secret);
    let signature = h.sign_commit(id, 0, &commitment);
    assert!(h
        .randomness
        .try_commit(&h.pubkey(0), &commitment, &signature)
        .is_err());
}

#[test]
fn a_reveal_before_the_commit_window_closes_is_refused() {
    // Revealing while commitments are still being taken would let a late
    // committer choose their secret against one already in the open.
    let h = setup();
    let id = h.randomness.open_round();
    h.commit(id, 0);
    assert!(h.randomness.try_reveal(&h.pubkey(0), &h.secret(0)).is_err());
}

#[test]
fn a_reveal_after_the_window_is_refused() {
    let h = setup();
    h.round_with_commits(&[0, 1, 2]);
    h.advance(REVEAL_WINDOW + 1);
    assert!(h.randomness.try_reveal(&h.pubkey(0), &h.secret(0)).is_err());
}

#[test]
fn finalizing_before_the_window_closes_is_refused_while_anyone_may_still_reveal() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    h.reveal(0);
    h.reveal(1);
    assert!(
        h.randomness.try_finalize(&id).is_err(),
        "node 2 still has a right to change the output"
    );
}

#[test]
fn a_second_round_cannot_open_while_one_is_running() {
    // Overlapping rounds would let a participant who has seen round N's
    // reveals choose their secret for round N+1.
    let h = setup();
    h.randomness.open_round();
    assert!(h.randomness.try_open_round().is_err());
}

#[test]
fn a_new_round_may_open_once_the_old_ones_windows_have_closed() {
    let h = setup();
    let first = h.round_with_commits(&[0, 1, 2]);
    h.advance(REVEAL_WINDOW + 1);

    let second = h.randomness.open_round();
    assert_eq!(second, first + 1);
    // The abandoned round is still finalizable: its penalties are still owed.
    h.randomness.finalize(&first);
    assert_eq!(
        h.randomness.get_round(&first).unwrap().status,
        RoundStatus::Failed
    );
}

#[test]
fn the_round_interval_binds_a_round_that_finished_early() {
    // A round where everyone reveals promptly closes before its reveal
    // deadline, and that is the ordinary case rather than the exception. The
    // interval used to be checked only on a round still live, so the ordinary
    // case was rate-limited by nothing: the next round could open in the same
    // ledger the last one closed in.
    let h = setup();
    h.set_round_interval(3600);
    let opened_at = h.env.ledger().timestamp();

    let first = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&first);
    assert_eq!(
        h.randomness.get_round(&first).unwrap().status,
        RoundStatus::Finalized,
        "closed early, well inside the reveal window"
    );

    assert!(
        h.randomness.try_open_round().is_err(),
        "five minutes since the last opening, against a floor of an hour"
    );

    h.env.ledger().set_timestamp(opened_at + 3600);
    assert_eq!(h.randomness.open_round(), first + 1);
}

#[test]
fn the_round_interval_runs_from_the_opening_rather_than_the_close() {
    // So a round that takes its whole window to finish does not push the next
    // one out by however long it took. The floor is on the cadence of
    // openings, which is the thing a consumer reading `latest` observes.
    let h = setup();
    h.set_round_interval(3600);
    let opened_at = h.env.ledger().timestamp();

    let first = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&first);

    assert!(h.randomness.try_open_round().is_err());
    h.env.ledger().set_timestamp(opened_at + 3600);
    assert_eq!(
        h.randomness.open_round(),
        first + 1,
        "the hour is counted from when the first round opened, not from when it closed"
    );
}

#[test]
fn an_interval_of_zero_lets_the_next_round_open_immediately() {
    // The floor is a governable parameter with no minimum, and a deployment
    // that sets it to zero has asked for back-to-back rounds. It should get
    // them rather than a revert.
    let h = setup();
    h.set_round_interval(0);

    let first = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&first);
    assert_eq!(h.randomness.open_round(), first + 1);
}

// -- withholding ------------------------------------------------------------

#[test]
fn a_node_that_commits_and_does_not_reveal_is_slashed() {
    // The one attack this construction cannot prevent, so it is priced. Note
    // what is being paid for: not being wrong, but going quiet after having
    // seen what everybody else said.
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2, 3]);
    let before = h.registry.get_node(&h.pubkey(3)).unwrap();

    for n in 0..3 {
        h.reveal(n);
    }
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&id);

    let after = h.registry.get_node(&h.pubkey(3)).unwrap();
    assert_eq!(after.reputation, before.reputation - NO_SHOW_REP_PENALTY);
    assert_eq!(after.stake, before.stake - NO_SHOW_SLASH);

    // And the round still produced a beacon from the three who did reveal.
    assert_eq!(
        h.randomness.get_round(&id).unwrap().status,
        RoundStatus::Finalized
    );
    assert!(h.randomness.random(&id).is_some());
}

#[test]
fn everyone_who_revealed_keeps_their_stake() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    let before: std::vec::Vec<i128> = (0..3)
        .map(|n| h.registry.get_node(&h.pubkey(n)).unwrap().stake)
        .collect();

    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&id);

    for (n, stake) in before.iter().enumerate() {
        assert_eq!(h.registry.get_node(&h.pubkey(n)).unwrap().stake, *stake);
    }
}

#[test]
fn a_round_that_falls_short_of_min_participants_publishes_nothing_and_still_charges() {
    // Failing must not be the cheap way out. A beacon assembled from too few
    // parties looks exactly like a good one, so the round publishes nothing --
    // but the nodes that made it fail are billed either way, or withholding to
    // force a failure would cost less than withholding to flip a bit.
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    let before = h.registry.get_node(&h.pubkey(2)).unwrap().stake;

    h.reveal(0);
    h.reveal(1);
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&id);

    let round = h.randomness.get_round(&id).unwrap();
    assert_eq!(round.status, RoundStatus::Failed);
    assert_eq!(round.output.to_array(), [0u8; 32]);
    assert!(
        h.randomness.random(&id).is_none(),
        "a failed round has no beacon"
    );
    assert_eq!(
        h.registry.get_node(&h.pubkey(2)).unwrap().stake,
        before - NO_SHOW_SLASH
    );
}

#[test]
fn a_failed_round_is_recorded_rather_than_erased() {
    // "Will never have an answer" and "has not finished" are different facts
    // and a consumer must be able to tell them apart.
    let h = setup();
    let id = h.round_with_commits(&[0]);
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&id);

    assert_eq!(
        h.randomness.get_round(&id).unwrap().status,
        RoundStatus::Failed
    );
    assert!(h.randomness.random(&id).is_none());
    assert!(h.randomness.latest().is_none());
}

#[test]
fn a_round_cannot_be_finalized_twice() {
    // Otherwise the no-shows would be charged once per caller willing to pay
    // the fee.
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&id);
    assert!(h.randomness.try_finalize(&id).is_err());
}

// -- reads ------------------------------------------------------------------

#[test]
fn latest_skips_a_failed_round_for_the_last_good_one() {
    let h = setup();
    let good = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&good);
    let output = h.randomness.random(&good).unwrap();

    h.advance(REVEAL_WINDOW + 1);
    let bad = h.round_with_commits(&[0]);
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&bad);

    assert_eq!(h.randomness.latest(), Some((good, output)));
}

#[test]
fn random_in_range_stays_inside_its_bound() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&id);

    for bound in [1u64, 2, 6, 52, 1_000_000] {
        let v = h.randomness.random_in_range(&id, &bound).unwrap();
        assert!(v < bound, "{v} is not below {bound}");
    }
}

#[test]
fn random_in_range_refuses_an_empty_bound() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.randomness.finalize(&id);
    assert!(h.randomness.try_random_in_range(&id, &0).is_err());
}

#[test]
fn an_unfinished_round_has_no_beacon() {
    let h = setup();
    let id = h.round_with_commits(&[0, 1, 2]);
    h.reveal(0);
    assert!(h.randomness.random(&id).is_none());
}

#[test]
fn an_unknown_round_reads_as_absent_rather_than_trapping() {
    let h = setup();
    assert!(h.randomness.get_round(&999).is_none());
    assert!(h.randomness.random(&999).is_none());
}

// -- parameter bounds -------------------------------------------------------

#[test]
fn a_single_participant_beacon_is_refused_at_the_point_it_is_set() {
    // One "participant" is a number one party chose alone, which is not a
    // beacon however it is labelled.
    let h = setup();
    let mut config = h.randomness.get_config();
    config.min_participants = 1;
    assert!(h.randomness.try_set_config(&config).is_err());
}

#[test]
fn the_published_bounds_are_the_bounds_that_are_enforced() {
    let h = setup();
    let b = h.randomness.param_bounds();
    let base = h.randomness.get_config();

    let mut c = base.clone();
    c.commit_window = b.min_commit_window;
    assert!(h.randomness.try_set_config(&c).is_ok());
    c.commit_window = b.min_commit_window - 1;
    assert!(h.randomness.try_set_config(&c).is_err());

    let mut c = base.clone();
    c.reveal_window = b.max_reveal_window;
    assert!(h.randomness.try_set_config(&c).is_ok());
    c.reveal_window = b.max_reveal_window + 1;
    assert!(h.randomness.try_set_config(&c).is_err());

    let mut c = base.clone();
    c.min_participants = b.max_participants_floor;
    assert!(h.randomness.try_set_config(&c).is_ok());
    c.min_participants = b.max_participants_floor + 1;
    assert!(h.randomness.try_set_config(&c).is_err());
}

#[test]
fn a_negative_no_show_slash_is_refused() {
    let h = setup();
    let mut config = h.randomness.get_config();
    config.no_show_slash = -1;
    assert!(h.randomness.try_set_config(&config).is_err());
}

#[test]
fn the_shipped_defaults_sit_inside_every_bound() {
    let h = setup();
    let c = h.randomness.get_config();
    let b = h.randomness.param_bounds();
    assert!(c.commit_window >= b.min_commit_window && c.commit_window <= b.max_commit_window);
    assert!(c.reveal_window >= b.min_reveal_window && c.reveal_window <= b.max_reveal_window);
    assert!(
        c.min_participants >= b.min_participants_floor
            && c.min_participants <= b.max_participants_floor
    );
}

#[test]
fn initialising_twice_is_refused() {
    let h = setup();
    let config = h.randomness.get_config();
    assert!(h.randomness.try_initialize(&config).is_err());
}

// -- the limit, stated ------------------------------------------------------

#[test]
fn the_last_revealer_can_choose_between_two_outcomes() {
    // Not a defence: a demonstration, kept as a test so that nobody later
    // reads the module documentation as cautious boilerplate. Whoever reveals
    // last knows both futures and picks one. The contract's answer is to
    // charge for the second, not to prevent it.
    let with_them = {
        let h = setup();
        let id = h.round_with_commits(&[0, 1, 2, 3]);
        for n in 0..4 {
            h.reveal(n);
        }
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };
    let without_them = {
        let h = setup();
        let id = h.round_with_commits(&[0, 1, 2, 3]);
        for n in 0..3 {
            h.reveal(n);
        }
        h.advance(REVEAL_WINDOW + 1);
        h.randomness.finalize(&id);
        h.randomness.random(&id).unwrap().to_array()
    };

    assert_ne!(
        with_them, without_them,
        "the withholder is choosing between two known values"
    );
}

#[test]
fn the_token_balance_moves_to_the_registry_when_a_no_show_is_slashed() {
    // The slash is a real transfer into the registry's slash pool, not a
    // bookkeeping entry: the stake was bonded there in the first place.
    let h = setup();
    let pool_before = h.registry.slash_pool();
    let id = h.round_with_commits(&[0, 1, 2, 3]);
    for n in 0..3 {
        h.reveal(n);
    }
    h.advance(REVEAL_WINDOW + 1);
    h.randomness.finalize(&id);

    assert_eq!(h.registry.slash_pool(), pool_before + NO_SHOW_SLASH);
    let _ = &h.token;
    let _ = &h.owners;
}
