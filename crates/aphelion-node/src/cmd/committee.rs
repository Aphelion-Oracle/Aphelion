//! `duties`, `election` and `dispute`: taking part in the slashing contract.
//!
//! Three rules hold across all of them.
//!
//! **Nothing is automatic.** Every write here is a command an operator runs on
//! purpose. See the note at the top of [`aphelion_node::engine::duty`] for why
//! a node that voted on a schedule would be the failure the elected committee
//! exists to avoid.
//!
//! **A call that posts a bond is shown before it is sent.** `dispute open` and
//! `dispute appeal` move the operator's money and print what they would cost
//! unless `--commit` is given, the same shape as `sweep`. The rest either cost
//! a transaction fee and nothing else, or move money towards the caller.
//!
//! **Every refusal names the account that was signing.** The contract's answer
//! to an ineligible caller is `NotEligible`, which does not distinguish "your
//! node is jailed" from "you configured the wrong account". The address is
//! printed on the failure path so the second is one line away from being
//! ruled out.

use std::sync::Arc;

use aphelion_node::chain::committee::{
    CliCommittee, CommitteeClient, DisputeRecord, DisputeStatus, ElectionPhase,
};
use aphelion_node::chain::{ChainClient, CliChain};
use aphelion_node::config::Config;
use aphelion_node::engine::duty::{Consequence, Duty, Snapshot, Watch};
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ElectionCmd {
    /// Show the running election: its phase, its deadlines and who stands.
    Show,
    /// Open an election. Permissionless once the sitting committee's term is
    /// served; costs a transaction fee and nothing else.
    Open,
    /// Stand for a seat, on the strength of this node.
    Nominate,
    /// Cast this node's ballot for a candidate, weighted by what the node is
    /// worth right now.
    Ballot {
        /// The candidate's Stellar address (G...), as `election show` lists it.
        candidate: String,
    },
    /// Count a ballot that has closed and seat the winners. Permissionless.
    Finalize,
}

#[derive(Subcommand)]
pub enum DisputeCmd {
    /// List the disputes within the scan window, newest first.
    List {
        /// How many disputes back from the newest to read. Each is an RPC
        /// round trip.
        #[arg(long, default_value_t = aphelion_node::engine::duty::DEFAULT_SCAN_DEPTH)]
        depth: u64,
    },
    /// Show one dispute in full, including the evidence link and the clock.
    Show { id: u64 },
    /// File an allegation against a node, posting the dispute bond.
    Open {
        /// The accused node's public key, 32 bytes of hex.
        accused: String,
        /// The feed the allegation is about, e.g. `BTC_USD`.
        feed: String,
        /// The nonce the accused signed the disputed submission under. Not the
        /// aggregator's round id: the nonce is inside the signed payload, so
        /// the evidence answering this allegation can be checked against it.
        /// `SubmissionAccepted` carries both.
        nonce: u64,
        /// Where the evidence lives: a URL or a content hash. Not the evidence
        /// itself — the ledger is the wrong place for a data dump, and a hash
        /// is enough to prove nobody edited it afterwards.
        evidence: String,
        /// Post the bond and file. Without this the bond is printed and
        /// nothing is sent.
        #[arg(long)]
        commit: bool,
    },
    /// Cast a committee vote.
    Vote {
        id: u64,
        /// Find against the node.
        #[arg(long, conflicts_with = "dismiss")]
        uphold: bool,
        /// Find for the node.
        #[arg(long)]
        dismiss: bool,
    },
    /// Close voting and record the outcome. Permissionless once the deadline
    /// has passed.
    Resolve { id: u64 },
    /// Contest a resolved dispute, posting the appeal bond.
    Appeal {
        id: u64,
        /// Post the bond and appeal. Without this the bond is printed and
        /// nothing is sent.
        #[arg(long)]
        commit: bool,
    },
    /// Move the money. Permissionless once the appeal window has closed.
    Settle { id: u64 },
}

/// The two clients every command here needs, plus this node's identity.
struct Context {
    chain: Arc<dyn ChainClient>,
    committee: Arc<dyn CommitteeClient>,
    node: String,
}

impl Context {
    fn open(config: &Config) -> Result<Self> {
        let signer = NodeSigner::load(
            &config.node.key_path,
            aphelion_node::strkey::contract_id_bytes(&config.network.aggregator_contract)?,
        )?;
        Ok(Self {
            chain: Arc::new(CliChain::new(config.network.clone())?),
            committee: Arc::new(CliCommittee::new(&config.network)?),
            node: signer.public_key_hex(),
        })
    }

    fn watch(&self) -> Watch {
        Watch::new(
            Arc::clone(&self.chain),
            Arc::clone(&self.committee),
            self.node.clone(),
        )
    }
}

/// Add the signing account to a contract refusal.
///
/// `NotEligible` and `NotCommitteeMember` are the same error whether the node
/// is jailed or the operator pointed `operator_account` at the wrong address,
/// and only one of those is worth an hour of debugging.
fn explain(e: NodeError, account: &str) -> NodeError {
    match e {
        NodeError::Chain(msg) => NodeError::Chain(format!(
            "{msg}\n\nSigned as {account}. If the contract said the caller was \
             not eligible, check that this is the account that bonded the \
             node's stake (`registry.owner_of`), and set \
             `operator_account` in the [network] section if it is not."
        )),
        other => other,
    }
}

fn landed(tx_hash: Option<String>) {
    match tx_hash {
        Some(h) => println!("tx {h}"),
        None => println!("landed (the CLI printed no transaction hash)"),
    }
}

// ---------------------------------------------------------------------------
// duties
// ---------------------------------------------------------------------------

pub async fn duties(config: &Config, json: bool) -> Result<()> {
    let ctx = Context::open(config)?;
    let (snapshot, duties) = ctx.watch().duties().await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ledger_time": snapshot.now,
                "node": snapshot.standing.node,
                "standing": snapshot.standing,
                "duties": duties,
                "disputes_scanned": snapshot.scanned,
                "disputes_total": snapshot.total_disputes,
            }))
            .map_err(|e| NodeError::Other(e.into()))?
        );
        return Ok(());
    }

    print_standing(&snapshot);

    if duties.is_empty() {
        println!("\nNothing outstanding.");
    } else {
        println!();
        for d in &duties {
            println!("[{}] {}", d.consequence.as_str().to_uppercase(), d.detail);
            if let Some(left) = d.seconds_left(snapshot.now) {
                println!("    {}", describe_window(left));
            }
            println!("    {}", d.command);
        }
        summarise(&duties);
    }

    if !snapshot.scan_is_complete() {
        println!(
            "\nScanned disputes {}..{} of {}. Anything older was not read; \
             raise it with `dispute list --depth`.",
            snapshot.scanned.0, snapshot.scanned.1, snapshot.total_disputes
        );
    }
    Ok(())
}

fn print_standing(s: &Snapshot) {
    let st = &s.standing;
    println!("node       : {}", st.node);
    println!(
        "owner      : {}",
        st.owner.as_deref().unwrap_or("(not registered)")
    );
    println!("signing as : {}", st.signer);
    println!(
        "weight     : {} bps{}",
        st.weight_bps,
        if st.weight_bps == 0 {
            "  (no ballot and no candidacy: unregistered, jailed or exiting)"
        } else {
            ""
        }
    );
    println!(
        "committee  : {}",
        if st.on_committee {
            "seated"
        } else {
            "not a member"
        }
    );
    // Worth saying loudly: every committee call will be refused, and the
    // contract's reason for refusing will not mention the configuration.
    if let Some(owner) = &st.owner {
        if owner != &st.signer {
            println!(
                "\nWARNING: this node was bonded by {owner}, but committee actions \
                 would be signed\n         by {}. Set `operator_account` and \
                 `operator_secret_env` in [network].",
                st.signer
            );
        }
    }
}

fn describe_window(seconds: i64) -> String {
    if seconds < 0 {
        return format!("closed {} ago", humanise(-seconds));
    }
    format!("{} left", humanise(seconds))
}

fn humanise(seconds: i64) -> String {
    match seconds {
        s if s >= 172_800 => format!("{}d", s / 86_400),
        s if s >= 7_200 => format!("{}h", s / 3_600),
        s if s >= 120 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn summarise(duties: &[Duty]) {
    let costly = duties
        .iter()
        .filter(|d| d.consequence == Consequence::Costly)
        .count();
    let forfeited = duties
        .iter()
        .filter(|d| d.consequence == Consequence::Forfeited)
        .count();
    if costly > 0 || forfeited > 0 {
        println!(
            "\n{costly} that can cost stake, {forfeited} that can cost a say. \
             Deadlines are ledger time, not this machine's clock."
        );
    }
}

// ---------------------------------------------------------------------------
// elections
// ---------------------------------------------------------------------------

pub async fn election(config: &Config, cmd: ElectionCmd) -> Result<()> {
    let ctx = Context::open(config)?;
    let account = ctx.committee.account().to_string();

    match cmd {
        ElectionCmd::Show => {
            let now = ctx.chain.ledger_time().await?;
            let params = ctx.committee.params().await?;
            let seated = ctx.committee.committee().await?;

            println!("ledger time : {now}");
            println!("seats       : {} (quorum {})", params.seats, params.quorum);
            println!("committee   :");
            for m in &seated {
                println!("  {m}{}", if *m == account { "  <- you" } else { "" });
            }
            if seated.is_empty() {
                println!("  (none seated)");
            }

            let Some(id) = ctx.committee.current_election().await? else {
                let next = ctx.committee.next_election().await?;
                println!(
                    "\nNo election running. The next may be opened {}.",
                    if now >= next {
                        "now".to_string()
                    } else {
                        format!("at {next} ({} away)", humanise((next - now) as i64))
                    }
                );
                return Ok(());
            };
            let Some(e) = ctx.committee.election(id).await? else {
                return Err(NodeError::Chain(format!(
                    "the contract says election {id} is open but cannot read it back"
                )));
            };

            let phase = e.phase(now);
            println!("\nelection {id}: {phase:?}");
            println!("  nominations close {}", e.ballot_opens);
            println!("  ballot closes     {}", e.closes);
            println!(
                "  ballots cast      {} ({} bps turnout)",
                e.ballots, e.turnout
            );

            let candidates = ctx.committee.candidates(id).await?;
            if candidates.is_empty() {
                println!("\nNobody has stood.");
            } else {
                println!("\n{:<58} {:>12}  node", "candidate", "weight");
                for c in &candidates {
                    println!(
                        "{:<58} {:>8} bps  {}{}",
                        c.address,
                        c.weight,
                        &c.node[..16],
                        if c.address == account { "  <- you" } else { "" }
                    );
                }
            }

            match ctx.committee.ballot_of(id, &ctx.node).await? {
                Some(who) => println!("\nThis node's ballot: {who}"),
                None if phase == ElectionPhase::Balloting => println!(
                    "\nThis node has not voted. Cast it with \
                     `aphelion-node election ballot <candidate>`."
                ),
                None => {}
            }
            Ok(())
        }

        ElectionCmd::Open => {
            let r = ctx
                .committee
                .open_election()
                .await
                .map_err(|e| explain(e, &account))?;
            println!("opened election {}", r.value);
            landed(r.tx_hash);
            Ok(())
        }

        ElectionCmd::Nominate => {
            println!("standing on node {} as {account}", ctx.node);
            let r = ctx
                .committee
                .nominate(&ctx.node)
                .await
                .map_err(|e| explain(e, &account))?;
            landed(r.tx_hash);
            Ok(())
        }

        ElectionCmd::Ballot { candidate } => {
            println!("casting {}'s ballot for {candidate}", ctx.node);
            let r = ctx
                .committee
                .cast_ballot(&ctx.node, &candidate)
                .await
                .map_err(|e| explain(e, &account))?;
            landed(r.tx_hash);
            Ok(())
        }

        ElectionCmd::Finalize => {
            let r = ctx
                .committee
                .finalize_election()
                .await
                .map_err(|e| explain(e, &account))?;
            println!("election {}", r.value.to_lowercase());
            if r.value == "Failed" {
                println!(
                    "Too few eligible candidates drew weight to fill the quorum. \
                     The sitting committee stays where it was."
                );
            }
            landed(r.tx_hash);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// disputes
// ---------------------------------------------------------------------------

pub async fn dispute(config: &Config, cmd: DisputeCmd) -> Result<()> {
    let ctx = Context::open(config)?;
    let account = ctx.committee.account().to_string();

    match cmd {
        DisputeCmd::List { depth } => {
            let now = ctx.chain.ledger_time().await?;
            let total = ctx.committee.dispute_count().await?;
            if total == 0 {
                println!("No disputes have been filed.");
                return Ok(());
            }
            let first = total.saturating_sub(depth.max(1)) + 1;
            println!(
                "{:<5} {:<10} {:<12} {:>6} {:>8}  accused",
                "id", "feed", "status", "round", "votes"
            );
            for id in (first..=total).rev() {
                let Some(d) = ctx.committee.dispute(id).await? else {
                    continue;
                };
                println!(
                    "{:<5} {:<10} {:<12} {:>6} {:>3}/{:<4}  {}{}",
                    d.id,
                    d.feed,
                    describe_status(&d, now),
                    d.nonce,
                    d.votes_for,
                    d.votes_against,
                    &d.accused[..16],
                    if d.accused == ctx.node {
                        "  <- this node"
                    } else {
                        ""
                    }
                );
            }
            if first > 1 {
                println!(
                    "\n{} older not shown; raise --depth to reach them.",
                    first - 1
                );
            }
            Ok(())
        }

        DisputeCmd::Show { id } => {
            let now = ctx.chain.ledger_time().await?;
            let params = ctx.committee.params().await?;
            let Some(d) = ctx.committee.dispute(id).await? else {
                println!("No dispute {id}.");
                return Ok(());
            };
            println!("dispute {id}: {}", describe_status(&d, now));
            println!(
                "accused   : {}{}",
                d.accused,
                if d.accused == ctx.node {
                    "  <- this node"
                } else {
                    ""
                }
            );
            println!("reporter  : {}", d.reporter);
            println!("allegation: {} nonce {}", d.feed, d.nonce);
            println!(
                "evidence  : {}",
                if d.evidence.is_empty() {
                    "(none given)"
                } else {
                    &d.evidence
                }
            );
            println!("bond      : {}", d.bond);
            println!(
                "votes     : {} for, {} against (quorum {}, round {})",
                d.votes_for, d.votes_against, params.quorum, d.vote_round
            );
            println!("opened    : {}", d.opened_at);
            if d.status == DisputeStatus::Voting {
                println!(
                    "voting    : closes {} ({})",
                    d.deadline,
                    describe_window(d.deadline as i64 - now as i64)
                );
            }
            if let Some(settles) = d.settles_at(params.appeal_period) {
                println!(
                    "appeal    : window closes {settles} ({})",
                    describe_window(settles as i64 - now as i64)
                );
            }
            if let Some(a) = &d.appellant {
                println!("appealed  : by {a}, bond {}", d.appeal_bond);
            }

            // Both sides of the evidence, spelled out. The allegation names a
            // feed and a nonce, which is exactly what one command takes to
            // answer it and the other to check the answer -- and the flags are
            // the point of printing them: a committee that retypes them from
            // the bundle has checked the bundle against itself.
            println!();
            if d.accused == ctx.node {
                println!(
                    "answer it : aphelion-node replay {} {} --json",
                    d.feed, d.nonce
                );
            }
            println!("check it  : aphelion-node verify-evidence <bundle> \\");
            println!(
                "              --node {} --feed {} --nonce {} --aggregator {}",
                d.accused, d.feed, d.nonce, config.network.aggregator_contract
            );
            Ok(())
        }

        DisputeCmd::Open {
            accused,
            feed,
            nonce,
            evidence,
            commit,
        } => {
            let accused = normalise_key(&accused)?;
            let params = ctx.committee.params().await?;
            println!(
                "file against {accused}\n  {feed} nonce {nonce}\n  evidence {evidence}\n  \
                 bond {} as {account}",
                params.dispute_bond
            );
            if !commit {
                println!(
                    "\nNothing sent. The bond is forfeited to the operator if the \
                     committee dismisses this.\nRe-run with --commit to file it."
                );
                return Ok(());
            }
            let r = ctx
                .committee
                .open_dispute(&accused, &feed, nonce, &evidence)
                .await
                .map_err(|e| explain(e, &account))?;
            println!("\nfiled as dispute {}", r.value);
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Vote {
            id,
            uphold,
            dismiss,
        } => {
            if uphold == dismiss {
                return Err(NodeError::Config(
                    "pass exactly one of --uphold or --dismiss".into(),
                ));
            }
            println!(
                "voting to {} dispute {id} as {account}",
                if uphold { "uphold" } else { "dismiss" }
            );
            let r = ctx
                .committee
                .vote(id, uphold)
                .await
                .map_err(|e| explain(e, &account))?;
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Resolve { id } => {
            let r = ctx
                .committee
                .resolve(id)
                .await
                .map_err(|e| explain(e, &account))?;
            println!("dispute {id}: {:?}", r.value);
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Appeal { id, commit } => {
            let params = ctx.committee.params().await?;
            println!(
                "appeal dispute {id} as {account}, bond {}",
                params.appeal_bond
            );
            if !commit {
                println!(
                    "\nNothing sent. The bond comes back only if the second vote \
                     changes the outcome.\nRe-run with --commit to appeal."
                );
                return Ok(());
            }
            let r = ctx
                .committee
                .appeal(id)
                .await
                .map_err(|e| explain(e, &account))?;
            println!("\nappealed; the committee votes again");
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Settle { id } => {
            let r = ctx
                .committee
                .settle(id)
                .await
                .map_err(|e| explain(e, &account))?;
            println!("dispute {id} settled");
            landed(r.tx_hash);
            Ok(())
        }
    }
}

fn describe_status(d: &DisputeRecord, now: u64) -> String {
    match d.status {
        // "voting" and "awaiting a result nobody recorded" are different
        // situations with the same stored status, and only one of them is
        // something a committee member can still act on.
        DisputeStatus::Voting if now <= d.deadline => "voting".into(),
        DisputeStatus::Voting => "unresolved".into(),
        DisputeStatus::Upheld => "upheld".into(),
        DisputeStatus::Dismissed => "dismissed".into(),
        DisputeStatus::Settled => "settled".into(),
    }
}

/// Accept a key however an operator pasted it, and refuse what is not one
/// before it costs a transaction fee to find out.
fn normalise_key(s: &str) -> Result<String> {
    let k = s.trim().trim_start_matches("0x").to_ascii_lowercase();
    if k.len() != 64 || !k.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(NodeError::Config(format!(
            "`{s}` is not a node public key: expected 32 bytes of hex (64 characters)"
        )));
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_accepted_however_it_was_pasted() {
        let k = "AB".repeat(32);
        assert_eq!(normalise_key(&format!("0x{k}")).unwrap(), "ab".repeat(32));
        assert_eq!(normalise_key(&format!("  {k}  ")).unwrap(), "ab".repeat(32));
    }

    #[test]
    fn a_stellar_address_is_refused_before_it_costs_a_fee() {
        // The most likely wrong paste: the operator's account rather than the
        // node's signing key.
        let e = normalise_key("GCNS2KFQDCMRG4EWA2DQY5XZDPIAFUWNSA7236OCEMDCU7PY7G3ZHF2V")
            .unwrap_err()
            .to_string();
        assert!(e.contains("32 bytes of hex"), "{e}");
        assert!(normalise_key(&"z".repeat(64)).is_err());
    }

    #[test]
    fn a_window_reads_the_same_either_side_of_zero() {
        assert_eq!(describe_window(90), "90s left");
        assert_eq!(describe_window(3_600), "60m left");
        assert_eq!(describe_window(86_400), "24h left");
        assert_eq!(describe_window(604_800), "7d left");
        // A deadline that has just gone is the one an operator most needs to
        // see, so it is reported rather than clamped to zero.
        assert_eq!(describe_window(-4), "closed 4s ago");
        assert_eq!(describe_window(-259_200), "closed 3d ago");
    }

    #[test]
    fn an_unresolved_dispute_is_not_reported_as_one_still_being_voted_on() {
        let mut d = DisputeRecord {
            id: 1,
            accused: "aa".repeat(32),
            reporter: "G".into(),
            feed: "BTC_USD".into(),
            nonce: 1,
            evidence: String::new(),
            bond: 0,
            opened_at: 0,
            deadline: 100,
            resolved_at: 0,
            vote_round: 1,
            votes_for: 0,
            votes_against: 0,
            status: DisputeStatus::Voting,
            appellant: None,
            appeal_bond: 0,
        };
        assert_eq!(describe_status(&d, 100), "voting");
        assert_eq!(describe_status(&d, 101), "unresolved");
        d.status = DisputeStatus::Settled;
        assert_eq!(describe_status(&d, 101), "settled");
    }

    #[test]
    fn a_contract_refusal_names_the_account_that_was_signing() {
        let e = explain(
            NodeError::Chain("Error(Contract, #12) NotEligible".into()),
            "GWRONG",
        )
        .to_string();
        assert!(e.contains("GWRONG"), "{e}");
        assert!(e.contains("operator_account"), "{e}");

        // A configuration problem is already the operator's to fix and needs
        // no second paragraph about eligibility.
        let e = explain(NodeError::Config("no slashing_contract".into()), "G");
        assert!(matches!(e, NodeError::Config(_)));
    }
}
