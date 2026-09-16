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

use std::io::Write;
use std::sync::Arc;

use aphelion_node::chain::committee::{
    CliCommittee, CommitteeClient, DisputeRecord, DisputeStatus, ElectionPhase,
};
use aphelion_node::chain::{ChainClient, CliChain};
use aphelion_node::config::Config;
use aphelion_node::engine::duty::{Consequence, Duty, Snapshot, Watch};
use aphelion_node::engine::verify;
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use clap::Subcommand;
use serde::Serialize;

use crate::cmd::verify_evidence::{render_body, wrap};

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
        /// The case: the document the allegation rests on. Its SHA-256 goes on
        /// the ledger and the file stays here — the ledger is the wrong place
        /// for a data dump, and a digest is what stops the case being rewritten
        /// once the accused has answered it.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Where the committee and the accused can fetch it. Optional, and
        /// worth giving: a digest nobody can resolve to a document is a case
        /// nobody can read.
        #[arg(long)]
        uri: Option<String>,
        /// Post the bond and file. Without this the bond is printed and
        /// nothing is sent.
        #[arg(long)]
        commit: bool,
    },
    /// Answer an allegation: publish the digest of the evidence.
    ///
    /// The document stays where it is. What goes on the ledger is its SHA-256,
    /// which is the difference between an answer a committee can be shown was
    /// fixed before the votes came in and a file produced afterwards.
    Respond {
        id: u64,
        /// The answer. Usually the output of `replay --json`, in which case it
        /// is audited against the allegation before anything is sent; any other
        /// file is accepted and its digest published unchecked.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Where the committee can fetch it. Optional: a digest with no
        /// locator still fixes which document was answered with, and some
        /// operators will hand the file over privately.
        #[arg(long)]
        uri: Option<String>,
        /// Publish it. Without this the digest is printed and nothing is sent.
        #[arg(long)]
        commit: bool,
    },
    /// Check a document against the dispute it belongs to, with the ledger
    /// supplying the standard.
    ///
    /// The committee's counterpart to `open` and `respond` both, and the one
    /// verifier that can finish the job: `verify-evidence` needs the allegation
    /// typed at it because it deliberately has no chain, and this has one. The
    /// accused, the feed and the nonce are read off the dispute; the deployment
    /// comes from this node's own configuration; and `registry.owner_of`
    /// answers the questions no document can, which are whose stake the
    /// allegation is against and whose key signed the file.
    ///
    /// Either side of the record. A dispute holds a commitment from each party
    /// — the reporter's case, fixed when it was filed, and the accused's
    /// answers, appended while the vote is open — and which one these bytes are
    /// is read rather than assumed. It decides the standard: an answer is held
    /// to the accused's key and a case is not, because the ordinary allegation
    /// is one operator's node reporting what it saw.
    ///
    /// Exits on the same scale as `verify-evidence`: 0 sound, 1 unsupported, 2
    /// unrelated, misdescribed or unsigned — and 65 for a document that is not
    /// an evidence bundle, which is not a failing grade but the absence of one.
    Check {
        /// The dispute this file belongs to. Everything the file is measured
        /// against is read from it.
        id: u64,
        /// The file to check, or `-` for stdin. Read as bytes and never
        /// re-serialised: the digest on the record is over the document as it
        /// arrived.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Machine-readable output: the audit, plus what the ledger says about
        /// the file, the key that signed it and the allegation.
        #[arg(long)]
        json: bool,
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
                    "(no locator given)"
                } else {
                    &d.evidence
                }
            );
            // The digest is the allegation's own commitment, fixed when it was
            // filed. Printed next to the locator because the two are only
            // worth anything together: follow the one, check the other — and
            // `dispute check` below is how, on this side of the record as much
            // as on the accused's.
            println!("case      : {}", d.evidence_digest);
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

            // What the accused answered this round with, in the order they
            // gave it. An empty list is printed rather than skipped: whether a
            // dispute was answered at all is something a committee weighs, and
            // a line that appears only when there is an answer would make
            // silence look like a rendering accident.
            let answers = ctx.committee.responses(id, d.vote_round).await?;
            println!();
            if answers.is_empty() {
                println!("answers   : none on the record for round {}", d.vote_round);
            }
            for (i, a) in answers.iter().enumerate() {
                println!(
                    "answer {}  : {} at {} ({})",
                    i + 1,
                    a.digest,
                    a.at,
                    if a.uri.is_empty() {
                        "no locator given"
                    } else {
                        &a.uri
                    }
                );
            }
            if answers.len() > 1 {
                println!(
                    "            {} earlier answers stand on the record; the last is the \
                     one the accused is offering.",
                    answers.len() - 1
                );
            }

            // Both sides of the evidence, spelled out. The allegation names a
            // feed and a nonce, which is exactly what one command takes to
            // answer it and the other to check the answer -- and the flags are
            // the point of printing them: a committee that retypes them from
            // the bundle has checked the bundle against itself.
            println!();
            if d.accused == ctx.node {
                println!(
                    "answer it : aphelion-node replay {} {} --json > evidence.json",
                    d.feed, d.nonce
                );
                println!(
                    "            aphelion-node dispute respond {id} --file evidence.json --commit"
                );
            }
            // Two ways to check it, and the first is the one to reach for.
            // `dispute check` reads all five off the ledger; the flags below
            // are for somebody who has no configuration pointed at this
            // deployment, which is most of the people entitled to an opinion.
            println!("check it  : aphelion-node dispute check {id} --file <document>");
            println!("            either side of the record: the case above, or an answer");
            println!("            below, whichever file you were handed");
            println!("or, with no node of your own:");
            println!("            aphelion-node verify-evidence <bundle> \\");
            println!(
                "              --node {} --feed {} --nonce {} --aggregator {}{}",
                d.accused,
                d.feed,
                d.nonce,
                config.network.aggregator_contract,
                // The digest is the last thing printed because it is the last
                // thing to exist: it is only there once the accused has
                // answered, and a committee checking a file against the round
                // it was answered with wants all five on one command line.
                match answers.last() {
                    Some(a) => format!(" \\\n              --digest {}", a.digest),
                    None => String::new(),
                }
            );
            Ok(())
        }

        DisputeCmd::Open {
            accused,
            feed,
            nonce,
            file,
            uri,
            commit,
        } => {
            let accused = normalise_key(&accused)?;
            let params = ctx.committee.params().await?;
            // The same hash, over the same bytes, as the one the accused's
            // answer is pinned by and the one `verify-evidence --digest`
            // recomputes. Read as bytes and never normalised: a case rewritten
            // in whitespace is a rewritten case.
            let raw = std::fs::read(&file)
                .map_err(|e| NodeError::Config(format!("cannot read `{}`: {e}", file.display())))?;
            let digest = hex::encode(verify::sha256(&raw));
            println!(
                "file against {accused}\n  {feed} nonce {nonce}\n  case      {}\n  \
                 sha256    {digest}\n  uri       {}\n  bond      {} as {account}",
                file.display(),
                uri.as_deref().unwrap_or("(none)"),
                params.dispute_bond
            );
            if !commit {
                println!(
                    "\nNothing sent. The bond is forfeited to the operator if the \
                     committee dismisses this, and the digest above is fixed for the \
                     life of the dispute — unlike an answer, an allegation cannot be \
                     corrected.\nRe-run with --commit to file it."
                );
                return Ok(());
            }
            let r = ctx
                .committee
                .open_dispute(
                    &accused,
                    &feed,
                    nonce,
                    uri.as_deref().unwrap_or(""),
                    &digest,
                )
                .await
                .map_err(|e| explain(e, &account))?;
            println!("\nfiled as dispute {}", r.value);
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Respond {
            id,
            file,
            uri,
            commit,
        } => {
            let Some(d) = ctx.committee.dispute(id).await? else {
                return Err(NodeError::Config(format!("no dispute {id}")));
            };
            // Bytes, never a re-serialisation. The digest published here is the
            // one a committee will compute over the file they are handed, so
            // the two have to be over the same thing down to the last newline.
            let raw = std::fs::read(&file)
                .map_err(|e| NodeError::Config(format!("cannot read `{}`: {e}", file.display())))?;
            let digest = hex::encode(verify::sha256(&raw));

            println!("answer dispute {id} as {account}");
            println!("  allegation  {} nonce {}", d.feed, d.nonce);
            println!("  file        {}", file.display());
            println!("  sha256      {digest}");
            println!("  uri         {}", uri.as_deref().unwrap_or("(none)"));

            // A courtesy audit, and a refusal where it matters. An operator
            // about to commit to a document for the length of a voting period
            // should not find out from the committee that they answered with
            // the wrong round -- and they can still answer, once, with the
            // right one.
            let expect = verify::Expectations::parse(
                Some(&d.accused),
                Some(&d.feed),
                Some(d.nonce),
                Some(&config.network.aggregator_contract),
                None,
            )
            .map_err(NodeError::Config)?;
            match verify::verify_document(&raw, &expect) {
                Ok(audit) => {
                    println!("  audit       {}", audit.verdict);
                    if audit.verdict < verify::Verdict::Unsupported {
                        return Err(NodeError::Config(format!(
                            "this file reads as `{}` against dispute {id}, so publishing it \
                             would answer the allegation with something that establishes \
                             nothing about it. Run `aphelion-node replay {} {} --json` for \
                             the round the allegation names, and \
                             `aphelion-node verify-evidence <file> --node {} --feed {} \
                             --nonce {}` to see the findings in full.",
                            audit.verdict, d.feed, d.nonce, d.accused, d.feed, d.nonce
                        )));
                    }
                    if audit.verdict != verify::Verdict::Sound {
                        println!(
                            "\n  Warning: the observations in this bundle do not produce the \
                             price it signs.\n  It is still an answer, and the committee \
                             will read it as the one you chose to give."
                        );
                    }
                }
                // Anything that is not a bundle: a log, an archive, a written
                // account. The digest is published unjudged, and saying so is
                // the honest version of a check that did not happen.
                Err(_) => println!("  audit       not an evidence bundle; nothing here checked it"),
            }

            if !commit {
                println!(
                    "\nNothing sent. A published digest can be corrected by a later answer \
                     in the same voting round and can never be withdrawn.\nRe-run with \
                     --commit to publish it."
                );
                return Ok(());
            }
            let r = ctx
                .committee
                .respond(id, &digest, uri.as_deref().unwrap_or(""))
                .await
                .map_err(|e| explain(e, &account))?;
            println!(
                "\nanswered; the digest is on the record for round {}",
                d.vote_round
            );
            landed(r.tx_hash);
            Ok(())
        }

        DisputeCmd::Check { id, file, json } => {
            let Some(d) = ctx.committee.dispute(id).await? else {
                return Err(NodeError::Config(format!("no dispute {id}")));
            };

            // Bytes, and never re-serialised. Everything below turns on the
            // file being the file: a copy this command normalised on the way in
            // would have a different digest from the one on the ledger, and the
            // difference would be reported against whoever committed to it.
            let raw = read_document(&file)?;
            let digest = verify::sha256(&raw);

            // The standard comes off the ledger, in full. `verify-evidence`
            // asks for these to be typed because it has no chain to read them
            // from, and the typing is what makes them worth something there;
            // here the chain is the same source the committee is judging on,
            // and reading them is strictly better than retyping them.
            //
            // Both sides of it. A dispute holds a commitment from each party
            // and the file an operator was handed is as likely to be the
            // reporter's as the accused's -- likelier, before the accused has
            // answered -- so which side it is on is read rather than assumed.
            let answers = ctx.committee.responses(id, d.vote_round).await?;
            let refs: Vec<verify::AnswerRef<'_>> = answers
                .iter()
                .map(|a| verify::AnswerRef {
                    digest: &a.digest,
                    at: a.at,
                })
                .collect();
            let standing = verify::place(
                &digest,
                verify::CaseRef {
                    digest: &d.evidence_digest,
                    filed_at: d.opened_at,
                },
                &refs,
            );

            // The one expectation that moves with the side. An answer is held
            // to the accused's key, because an answer that does not carry the
            // accused's signature answers nothing. A case is not: the ordinary
            // allegation is one operator's node reporting what it saw, and a
            // reporter required to produce the accused's signature could only
            // ever file the case the accused had already signed for them.
            let expect = verify::Expectations::parse(
                standing.binds_to_accused().then_some(d.accused.as_str()),
                Some(&d.feed),
                Some(d.nonce),
                Some(&config.network.aggregator_contract),
                standing.expected_digest(),
            )
            .map_err(NodeError::Config)?;

            // The question `verify-evidence` prints and cannot answer: the
            // allegation names a key, and what binds a key to a person is the
            // registry. This command has one.
            let owner = ctx.committee.owner_of(&d.accused).await?;

            let audit = match verify::verify_document(&raw, &expect) {
                Ok(a) => a,
                // Not an error, and the distinction is the whole reason this
                // path exists. A dispute is answered in prose as often as in
                // JSON and filed in prose more often still, and the ledger's
                // answer to "are these the bytes that were committed to" does
                // not need the document to be a bundle. Printing the record
                // and stopping is the honest result; failing would report a
                // written account as a finding against whoever wrote it.
                Err(e) => {
                    ungradable(&d, owner.as_deref(), &standing, &digest, &e, json)?;
                    std::io::stdout().flush().ok();
                    std::process::exit(EX_DATAERR);
                }
            };

            // Who signed it, resolved through the registry rather than guessed
            // from the dispute. On the answer side this is the accused or the
            // audit has already said `unrelated`; on the case side it is the
            // shape of the allegation, and the four shapes are different cases.
            let signatory = match &audit.signed {
                None => None,
                Some(s) => {
                    let by = if s.public_key == d.accused {
                        owner.clone()
                    } else {
                        ctx.committee.owner_of(&s.public_key).await?
                    };
                    Some(verify::attribute(
                        &s.public_key,
                        &d.accused,
                        &d.reporter,
                        by.as_deref(),
                    ))
                }
            };

            if json {
                let report = CheckReport {
                    dispute: d.id,
                    accused: &d.accused,
                    owner: owner.as_deref(),
                    feed: &d.feed,
                    nonce: d.nonce,
                    vote_round: d.vote_round,
                    status: d.status,
                    document: &hex::encode(digest),
                    standing: &standing,
                    signatory: signatory.as_ref(),
                    audit: Some(&audit),
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|e| NodeError::Other(e.into()))?
                );
            } else {
                render_check(&d, owner.as_deref(), &standing, signatory.as_ref(), &audit);
            }

            std::io::stdout().flush().ok();
            // The same scale as `verify-evidence`, because a script that reads
            // one should read the other. Nothing about the dispute's own state
            // moves it: this is a judgement on a document.
            std::process::exit(audit.verdict.exit_code());
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

/// `EX_DATAERR`, as `sysexits.h` has meant it for forty years.
///
/// For a document this command cannot grade because it is not a bundle, which
/// is a different thing from a bundle that grades badly. It has to land outside
/// [`verify::Verdict::exit_code`]'s range of three for the same reason
/// `verify-evidence` puts a usage error outside it: 1 reads as `unsupported`,
/// which is a finding, and an operator who answered a dispute in prose has not
/// earned one.
const EX_DATAERR: i32 = 65;

/// A checked document, for something other than a person to read.
///
/// The allegation, whose stake it falls on, where the ledger puts the file, who
/// signed it, and the audit — in that order, because the audit is worth nothing
/// until the rest says what it was an audit of.
#[derive(Serialize)]
struct CheckReport<'a> {
    dispute: u64,
    accused: &'a str,
    /// `registry.owner_of` the accused. `None` where the registry has never
    /// seen the key, which is a fact about the allegation rather than about the
    /// file.
    owner: Option<&'a str>,
    feed: &'a str,
    nonce: u64,
    vote_round: u32,
    status: DisputeStatus,
    /// SHA-256 of the file as it arrived. At the top level rather than inside
    /// the audit because it is known whether or not the document could be
    /// graded, and it is the number every other field here is about.
    document: &'a str,
    standing: &'a verify::Standing,
    /// Absent where nothing was signed — an ungradable document, or one whose
    /// signature did not verify.
    signatory: Option<&'a verify::Signatory>,
    /// Absent where the document is not an evidence bundle. `null` is not a
    /// verdict and must not be read as one.
    audit: Option<&'a verify::Audit>,
}

/// A document the ledger has something to say about and this command cannot
/// grade: the record, and then plainly why there is no verdict under it.
fn ungradable(
    d: &DisputeRecord,
    owner: Option<&str>,
    standing: &verify::Standing,
    digest: &[u8; 32],
    why: &serde_json::Error,
    json: bool,
) -> Result<()> {
    let document = hex::encode(digest);
    if json {
        let report = CheckReport {
            dispute: d.id,
            accused: &d.accused,
            owner,
            feed: &d.feed,
            nonce: d.nonce,
            vote_round: d.vote_round,
            status: d.status,
            document: &document,
            standing,
            signatory: None,
            audit: None,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| NodeError::Other(e.into()))?
        );
        return Ok(());
    }

    render_allegation(d, owner);
    render_file(&document, standing, None);
    println!(
        "{}",
        wrap(
            &format!(
                "No verdict: this is not an Aphelion evidence bundle ({why}). A bundle is \
                 the output of `aphelion-node replay <feed> <nonce> --json`, and nothing \
                 else can be checked against a signature, a window and an arithmetic. A \
                 written account, a log archive or an exchange's own export is a \
                 legitimate document in a dispute and is read rather than graded — which \
                 is what the record above is for: it says who committed to these bytes \
                 and when, whatever they turn out to contain."
            ),
            0
        )
    );
    Ok(())
}

/// Read a document from a path or from stdin, as bytes.
fn read_document(path: &std::path::Path) -> Result<Vec<u8>> {
    if path == std::path::Path::new("-") {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| NodeError::Config(format!("cannot read the answer from stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read(path)
        .map_err(|e| NodeError::Config(format!("cannot read `{}`: {e}", path.display())))
}

/// The allegation first, then the file, then the audit.
///
/// The order is the argument. A committee member who reads the verdict before
/// they read what it was a verdict about has learned that some bundle is sound,
/// which is the one thing an evidence bundle can always be made to be.
fn render_check(
    d: &DisputeRecord,
    owner: Option<&str>,
    standing: &verify::Standing,
    signatory: Option<&verify::Signatory>,
    audit: &verify::Audit,
) {
    render_allegation(d, owner);
    render_file(
        audit.document_digest.as_deref().unwrap_or(""),
        standing,
        signatory,
    );
    render_body(audit);

    println!();
    // What is left over depends on which side of the record the file is on,
    // and the two are not the same question. For an answer it is the one the
    // arithmetic cannot reach. For a case it is larger and comes first: a
    // sound document proves what somebody signed, and the dispute is about
    // whether the accused's own submission was wrong.
    let remaining = if standing.is_case() {
        "A sound case is not a finding. What this establishes is that the payload in it \
         was signed by the key it names — not that the accused's submission was wrong, \
         which is what the committee is voting on and what the accused's answer is the \
         other half of. Two nodes that disagree are the ordinary case and the reason \
         the aggregator takes a median rather than a vote."
    } else {
        "Still to establish elsewhere, and not by any arithmetic: whether an observation \
         was left out. Four venues that agree look the same as four of six whose absent \
         two would have moved the median, and only the operator holds the full table."
    };
    println!("{}", wrap(remaining, 0));
}

/// What the ledger says the dispute is, before anything is read out of a file.
fn render_allegation(d: &DisputeRecord, owner: Option<&str>) {
    println!("Allegation (from the ledger, not from the file)");
    println!("  dispute     {}", d.id);
    println!("  accused     {}", d.accused);
    match owner {
        Some(o) => println!("  stake of    {o}"),
        // Worth saying loudly. An allegation against a key the registry does
        // not know is an allegation against nobody's stake, and no verdict on
        // any document changes that.
        None => {
            println!("  stake of    the registry does not know this key —");
            println!("              this allegation is against no registered node");
        }
    }
    println!("  allegation  {} nonce {}", d.feed, d.nonce);
    println!("  reporter    {}", d.reporter);
    println!(
        "  case        {} (filed {})",
        d.evidence_digest, d.opened_at
    );
    println!("  vote round  {}", d.vote_round);
    println!();
}

/// The file: what it hashes to, which commitment on the record is to these
/// bytes, and whose key is on the payload inside.
fn render_file(document: &str, standing: &verify::Standing, signatory: Option<&verify::Signatory>) {
    println!("The file");
    if !document.is_empty() {
        println!("  sha256      {document}");
    }
    println!("  record      {}", wrap(&standing.summary(), 14));
    if let Some(s) = signatory {
        // Above the audit rather than in it. Whose signature this is decides
        // what a verdict on it would even mean, and it is not something the
        // document gets to answer about itself.
        println!("  signed by   {}", wrap(&s.summary(), 14));
    }
    println!();
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
    use clap::Parser;

    /// Just enough of a parser to hand `DisputeCmd` a command line.
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: DisputeCmd,
    }

    fn parse(line: &str) -> std::result::Result<DisputeCmd, clap::Error> {
        Cli::try_parse_from(std::iter::once("dispute").chain(line.split_whitespace()))
            .map(|c| c.cmd)
    }

    /// The other end of the pin in `engine::duty`. That test asserts the duty
    /// prints `--file`; this one asserts the binary accepts it and rejects the
    /// name it used to be printed under, which shipped as a command an operator
    /// could not run.
    #[test]
    fn the_command_a_duty_prints_is_a_command_this_binary_accepts() {
        assert!(matches!(
            parse("respond 1 --file evidence.json --commit").unwrap(),
            DisputeCmd::Respond {
                id: 1,
                commit: true,
                ..
            }
        ));
        assert!(parse("respond 1 --bundle evidence.json --commit").is_err());
    }

    /// Checking an answer is not a write, so it has no `--commit` and must not
    /// grow one: a committee member who reads a bundle has not voted, and a
    /// flag that looked like assent would be the one mistake this command can
    /// make that costs somebody their stake.
    #[test]
    fn checking_an_answer_sends_nothing() {
        assert!(matches!(
            parse("check 7 --file evidence.json").unwrap(),
            DisputeCmd::Check {
                id: 7,
                json: false,
                ..
            }
        ));
        assert!(parse("check 7 --file evidence.json --commit").is_err());
        // The file is the whole input; there is nothing to check without one.
        assert!(parse("check 7").is_err());
    }

    /// A document that is not a bundle has no grade, and the code it exits with
    /// must not be readable as one. 1 is `unsupported` — a finding — and an
    /// operator who filed or answered a dispute in prose has not earned it.
    #[test]
    fn a_document_with_no_grade_exits_outside_the_range_of_grades() {
        for v in [
            verify::Verdict::Sound,
            verify::Verdict::Unsupported,
            verify::Verdict::Unrelated,
            verify::Verdict::Misdescribed,
            verify::Verdict::Unsigned,
        ] {
            assert_ne!(v.exit_code(), EX_DATAERR, "{v} would be read as ungradable");
        }
    }

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
            evidence_digest: "ab".repeat(32),
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
