//! `Watch`: assembling the snapshot the duty rules are derived from.
//!
//! `engine::duty`'s unit tests cover the rules, against struct literals. What
//! they cannot cover is the assembly — which reads happen, how a bounded scan
//! picks its window, and whether "this member has already voted" is asked about
//! the right account. Those are the parts that produce a wrong answer without
//! producing a wrong-looking one: a scan that silently reads the oldest
//! disputes instead of the newest reports no duties at all, and so does a
//! healthy network.
//!
//! The fake below answers from a fixture and counts what it was asked, so both
//! are assertions rather than assumptions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aphelion_node::chain::committee::{
    CandidateRecord, CommitteeClient, DisputeRecord, DisputeStatus, ElectionRecord, Receipt,
    SlashingParams,
};
use aphelion_node::chain::{ChainClient, MockChain, OnChainNode};
use aphelion_node::engine::duty::{DutyKind, Watch};
use aphelion_node::error::{NodeError, Result};
use async_trait::async_trait;

const ME: &str = "11111111111111111111111111111111111111111111111111111111111111aa";
const OTHER: &str = "22222222222222222222222222222222222222222222222222222222222222bb";
const OWNER: &str = "GOWNER";

#[derive(Default)]
struct Fixture {
    disputes: HashMap<u64, DisputeRecord>,
    dispute_count: u64,
    committee: Vec<String>,
    votes: HashMap<(u64, String), bool>,
    election: Option<ElectionRecord>,
    candidates: Vec<CandidateRecord>,
    ballots: HashMap<String, String>,
    next_election: u64,
    owner: Option<String>,
}

struct FakeCommittee {
    account: String,
    fixture: Fixture,
    /// Every read, in order. The scan's window is an assertion, not a hope.
    reads: Mutex<Vec<String>>,
}

impl FakeCommittee {
    fn new(fixture: Fixture) -> Self {
        Self {
            account: OWNER.into(),
            fixture,
            reads: Mutex::new(Vec::new()),
        }
    }

    fn note(&self, what: impl Into<String>) {
        self.reads.lock().unwrap().push(what.into());
    }

    fn reads(&self) -> Vec<String> {
        self.reads.lock().unwrap().clone()
    }
}

fn params() -> SlashingParams {
    SlashingParams {
        quorum: 3,
        voting_period: 86_400,
        appeal_period: 43_200,
        dispute_bond: 1_000,
        appeal_bond: 3_000,
        seats: 5,
        nomination_period: 3_600,
        election_period: 7_200,
        term_length: 100_000,
    }
}

fn dispute(id: u64, accused: &str, status: DisputeStatus, deadline: u64) -> DisputeRecord {
    DisputeRecord {
        id,
        accused: accused.into(),
        reporter: "GREPORTER".into(),
        feed: "BTC_USD".into(),
        nonce: id,
        evidence: "ipfs://bafy".into(),
        bond: 1_000,
        opened_at: 0,
        deadline,
        resolved_at: if status == DisputeStatus::Voting {
            0
        } else {
            10
        },
        vote_round: 1,
        votes_for: 0,
        votes_against: 0,
        status,
        appellant: None,
        appeal_bond: 0,
    }
}

#[async_trait]
impl CommitteeClient for FakeCommittee {
    fn account(&self) -> &str {
        &self.account
    }

    async fn params(&self) -> Result<SlashingParams> {
        Ok(params())
    }

    async fn committee(&self) -> Result<Vec<String>> {
        Ok(self.fixture.committee.clone())
    }

    async fn owner_of(&self, _public_key_hex: &str) -> Result<Option<String>> {
        Ok(self.fixture.owner.clone())
    }

    async fn current_election(&self) -> Result<Option<u64>> {
        Ok(self.fixture.election.as_ref().map(|e| e.id))
    }

    async fn next_election(&self) -> Result<u64> {
        Ok(self.fixture.next_election)
    }

    async fn election(&self, _id: u64) -> Result<Option<ElectionRecord>> {
        Ok(self.fixture.election.clone())
    }

    async fn candidates(&self, _id: u64) -> Result<Vec<CandidateRecord>> {
        Ok(self.fixture.candidates.clone())
    }

    async fn ballot_of(&self, _id: u64, public_key_hex: &str) -> Result<Option<String>> {
        Ok(self.fixture.ballots.get(public_key_hex).cloned())
    }

    async fn dispute_count(&self) -> Result<u64> {
        Ok(self.fixture.dispute_count)
    }

    async fn dispute(&self, id: u64) -> Result<Option<DisputeRecord>> {
        self.note(format!("dispute {id}"));
        Ok(self.fixture.disputes.get(&id).cloned())
    }

    async fn vote_of(&self, dispute_id: u64, member: &str) -> Result<Option<bool>> {
        self.note(format!("vote_of {dispute_id} {member}"));
        Ok(self
            .fixture
            .votes
            .get(&(dispute_id, member.into()))
            .copied())
    }

    async fn open_election(&self) -> Result<Receipt<u64>> {
        unimplemented!("Watch never writes")
    }
    async fn nominate(&self, _: &str) -> Result<Receipt<()>> {
        unimplemented!("Watch never writes")
    }
    async fn cast_ballot(&self, _: &str, _: &str) -> Result<Receipt<()>> {
        unimplemented!("Watch never writes")
    }
    async fn finalize_election(&self) -> Result<Receipt<String>> {
        unimplemented!("Watch never writes")
    }
    async fn open_dispute(&self, _: &str, _: &str, _: u64, _: &str) -> Result<Receipt<u64>> {
        unimplemented!("Watch never writes")
    }
    async fn vote(&self, _: u64, _: bool) -> Result<Receipt<()>> {
        unimplemented!("Watch never writes")
    }
    async fn resolve(&self, _: u64) -> Result<Receipt<DisputeStatus>> {
        unimplemented!("Watch never writes")
    }
    async fn appeal(&self, _: u64) -> Result<Receipt<()>> {
        unimplemented!("Watch never writes")
    }
    async fn settle(&self, _: u64) -> Result<Receipt<()>> {
        unimplemented!("Watch never writes")
    }
}

/// A chain fixed at one ledger time, with one registered node.
struct Clock {
    now: u64,
    weight_bps: u32,
}

#[async_trait]
impl ChainClient for Clock {
    async fn ledger_time(&self) -> Result<u64> {
        Ok(self.now)
    }
    async fn submit_price(
        &self,
        _: &str,
        _: &aphelion_node::signer::SignedSubmission,
    ) -> Result<aphelion_node::chain::SubmitReceipt> {
        Err(NodeError::Chain("not used".into()))
    }
    async fn latest_price(
        &self,
        _: &aphelion_core::FeedId,
    ) -> Result<Option<aphelion_node::chain::OnChainPrice>> {
        Ok(None)
    }
    async fn node_info(&self, public_key_hex: &str) -> Result<Option<OnChainNode>> {
        Ok(Some(OnChainNode {
            public_key_hex: public_key_hex.into(),
            stake: 1,
            reputation: self.weight_bps,
            status: "Active".into(),
            weight_bps: self.weight_bps,
            last_submission: 0,
        }))
    }
    async fn last_nonce(&self, _: &str, _: &aphelion_core::FeedId) -> Result<u64> {
        Ok(0)
    }
    async fn list_nodes(&self) -> Result<Vec<String>> {
        Ok(vec![ME.into()])
    }
    async fn absence_threshold(&self) -> Result<u64> {
        Ok(600)
    }
    async fn sweep_absent(&self, _: &[String]) -> Result<aphelion_node::chain::SweepReceipt> {
        Err(NodeError::Chain("not used".into()))
    }
}

fn watch(now: u64, weight_bps: u32, fixture: Fixture) -> (Watch, Arc<FakeCommittee>) {
    let committee = Arc::new(FakeCommittee::new(fixture));
    let chain: Arc<dyn ChainClient> = Arc::new(Clock { now, weight_bps });
    let w = Watch::new(
        chain,
        Arc::clone(&committee) as Arc<dyn CommitteeClient>,
        ME.to_uppercase(),
    );
    (w, committee)
}

#[tokio::test]
async fn a_key_given_in_upper_case_still_matches_the_ledgers() {
    // The operator's key can arrive from `pubkey`, a config file or a paste.
    // Comparing it case-sensitively against the contract's lower-case hex
    // would report every dispute against this node as somebody else's.
    let mut f = Fixture {
        dispute_count: 1,
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    f.disputes
        .insert(1, dispute(1, ME, DisputeStatus::Voting, 1_000));

    let (w, _) = watch(500, 7_500, f);
    let (snapshot, duties) = w.duties().await.unwrap();
    assert_eq!(snapshot.standing.node, ME);
    assert_eq!(
        duties.iter().map(|d| d.kind).collect::<Vec<_>>(),
        vec![DutyKind::AnswerDispute]
    );
}

#[tokio::test]
async fn the_scan_reads_the_newest_disputes_and_says_what_it_covered() {
    let mut f = Fixture {
        dispute_count: 100,
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    for id in 1..=100 {
        f.disputes
            .insert(id, dispute(id, OTHER, DisputeStatus::Settled, 1_000));
    }

    let (w, fake) = watch(2_000, 7_500, f);
    let w = w.with_scan_depth(10);
    let snapshot = w.snapshot().await.unwrap();

    assert_eq!(snapshot.scanned, (91, 100));
    assert_eq!(snapshot.total_disputes, 100);
    // A window that silently read 1..10 would look identical in the duty list
    // — nothing outstanding either way — so the range is asserted directly.
    assert!(!snapshot.scan_is_complete());
    assert_eq!(
        fake.reads(),
        (91..=100)
            .rev()
            .map(|id| format!("dispute {id}"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_scan_that_reached_the_first_dispute_reports_itself_complete() {
    let mut f = Fixture {
        dispute_count: 3,
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    for id in 1..=3 {
        f.disputes
            .insert(id, dispute(id, OTHER, DisputeStatus::Settled, 10));
    }
    let (w, _) = watch(2_000, 7_500, f);
    let snapshot = w.with_scan_depth(50).snapshot().await.unwrap();
    assert_eq!(snapshot.scanned, (1, 3));
    assert!(snapshot.scan_is_complete());
}

#[tokio::test]
async fn an_empty_slashing_contract_is_a_quiet_answer_not_a_failure() {
    // What a fresh deployment looks like on the day it is registered against.
    let f = Fixture {
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    let (w, _) = watch(1_000, 5_000, f);
    let (snapshot, duties) = w.duties().await.unwrap();
    assert_eq!(snapshot.scanned, (0, 0));
    assert!(snapshot.scan_is_complete());
    assert!(duties.is_empty());
}

#[tokio::test]
async fn a_member_is_only_asked_about_votes_that_are_still_open() {
    let mut f = Fixture {
        dispute_count: 3,
        committee: vec![OWNER.into()],
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    f.disputes
        .insert(1, dispute(1, OTHER, DisputeStatus::Voting, 1_000));
    f.disputes
        .insert(2, dispute(2, OTHER, DisputeStatus::Settled, 1_000));
    f.disputes
        .insert(3, dispute(3, OTHER, DisputeStatus::Voting, 1_000));
    f.votes.insert((3, OWNER.into()), true);

    let (w, fake) = watch(500, 7_500, f);
    let (snapshot, duties) = w.duties().await.unwrap();

    // A settled dispute cannot be voted on, so asking about it is a round trip
    // spent to learn nothing.
    let asked: Vec<_> = fake
        .reads()
        .into_iter()
        .filter(|r| r.starts_with("vote_of"))
        .collect();
    assert_eq!(
        asked,
        vec![format!("vote_of 3 {OWNER}"), format!("vote_of 1 {OWNER}"),]
    );

    assert_eq!(snapshot.voted.iter().copied().collect::<Vec<_>>(), vec![3]);
    let outstanding: Vec<_> = duties
        .iter()
        .filter(|d| d.kind == DutyKind::VoteOnDispute)
        .map(|d| d.subject)
        .collect();
    assert_eq!(outstanding, vec![1]);
}

#[tokio::test]
async fn a_node_that_is_not_on_the_committee_is_never_asked_how_it_voted() {
    let mut f = Fixture {
        dispute_count: 1,
        next_election: u64::MAX,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    f.disputes
        .insert(1, dispute(1, OTHER, DisputeStatus::Voting, 1_000));

    let (w, fake) = watch(500, 7_500, f);
    w.snapshot().await.unwrap();
    assert!(
        !fake.reads().iter().any(|r| r.starts_with("vote_of")),
        "asked for a vote it could not have cast: {:?}",
        fake.reads()
    );
}

#[tokio::test]
async fn a_signing_account_that_did_not_bond_the_node_is_visible_in_the_standing() {
    // The failure this exists to make obvious: `operator_account` left at the
    // submitter, so every committee call is refused with `NotEligible` and
    // nothing says why.
    let f = Fixture {
        next_election: u64::MAX,
        owner: Some("GSOMEBODYELSE".into()),
        ..Default::default()
    };
    let (w, _) = watch(1_000, 7_500, f);
    let snapshot = w.snapshot().await.unwrap();
    assert_eq!(snapshot.standing.owner.as_deref(), Some("GSOMEBODYELSE"));
    assert_eq!(snapshot.standing.signer, OWNER);
    assert!(!snapshot.standing.on_committee);
}

#[tokio::test]
async fn an_election_this_node_has_voted_in_is_read_back_as_voted() {
    let mut f = Fixture {
        next_election: 0,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    f.election = Some(ElectionRecord {
        id: 7,
        opened_at: 0,
        ballot_opens: 100,
        closes: 900,
        seats: 5,
        quorum: 3,
        status: "Running".into(),
        finalized_at: 0,
        ballots: 1,
        turnout: 7_500,
        seated: 0,
    });
    f.candidates = vec![CandidateRecord {
        address: OWNER.into(),
        node: ME.into(),
        weight: 7_500,
    }];

    let (w, _) = watch(500, 7_500, f);
    let (snapshot, duties) = w.duties().await.unwrap();
    assert!(
        snapshot.nominated,
        "the operator stands and was not seen to"
    );
    assert!(!snapshot.balloted);
    assert_eq!(
        duties.iter().map(|d| d.kind).collect::<Vec<_>>(),
        vec![DutyKind::CastBallot]
    );

    // And once it has voted, the duty goes.
    let mut f = Fixture {
        next_election: 0,
        owner: Some(OWNER.into()),
        ..Default::default()
    };
    f.election = Some(ElectionRecord {
        id: 7,
        opened_at: 0,
        ballot_opens: 100,
        closes: 900,
        seats: 5,
        quorum: 3,
        status: "Running".into(),
        finalized_at: 0,
        ballots: 1,
        turnout: 7_500,
        seated: 0,
    });
    f.ballots.insert(ME.into(), OWNER.into());
    let (w, _) = watch(500, 7_500, f);
    let (snapshot, duties) = w.duties().await.unwrap();
    assert!(snapshot.balloted);
    assert!(duties.is_empty());
}

/// Belt and braces: the mock chain the rest of the suite uses is a
/// `ChainClient`, and `Watch` holds one. This is the compile-time check that
/// the two fit together, so a signature change is caught here rather than in a
/// deployment.
#[tokio::test]
async fn watch_accepts_the_shipped_mock_chain() {
    let f = Fixture {
        next_election: u64::MAX,
        ..Default::default()
    };
    let committee = Arc::new(FakeCommittee::new(f));
    let w = Watch::new(
        Arc::new(MockChain::new(1_700_000_000)) as Arc<dyn ChainClient>,
        committee as Arc<dyn CommitteeClient>,
        ME,
    );
    assert!(w.snapshot().await.is_ok());
}
