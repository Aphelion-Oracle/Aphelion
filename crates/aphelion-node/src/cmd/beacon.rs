//! `beacon`: taking part in the randomness contract.
//!
//! The judgement is [`aphelion_node::engine::beacon::decide`]; everything here
//! is the I/O around it — reading the round off the chain, reading what this
//! node stored for it, and carrying out the one action that comes back.
//!
//! Two rules, and the first is the one the whole design turns on.
//!
//! **The secret is stored before the commitment is sent.** Not after, not in
//! memory. Between the two transactions this node holds the only copy of
//! something nothing on chain can reconstruct, and a node that cannot reveal is
//! slashed — so the write is committed first and a crash in the gap costs a
//! round rather than stake.
//!
//! **A reveal is never optional.** `commit` and `open` are gated on
//! `[beacon] participate`; `reveal` is not, and neither is the tick that sends
//! one. An operator who switches participation off stops the node entering new
//! rounds; it does not release it from a commitment already on the ledger.

use std::sync::Arc;

use aphelion_node::chain::randomness::{BeaconClient, CliBeacon};
use aphelion_node::chain::{ChainClient, CliChain};
use aphelion_node::config::Config;
use aphelion_node::db::Repo;
use aphelion_node::engine::beacon::{decide, Action, Idle, OurPart, OwedReveal, Snapshot};
use aphelion_node::error::{NodeError, Result};
use aphelion_node::signer::NodeSigner;
use clap::Subcommand;
use rand::RngCore;
use sha2::{Digest, Sha256};

#[derive(Subcommand)]
pub enum BeaconCmd {
    /// What the beacon is doing, what this node's part in it is, and what it
    /// would do next.
    ///
    /// Reads only. Needs the database, because what this node stored for a
    /// round is half the answer.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Do the one thing the current state calls for, and stop.
    ///
    /// The same decision the loop makes every `[beacon] interval`, run once.
    /// Useful from cron for an operator who would rather not leave
    /// `participate` on.
    Tick,
    /// Open a round. Permissionless; costs a fee and pays nothing back.
    Open,
    /// Commit a secret to the running round.
    Commit,
    /// Open the oldest commitment this node has not revealed.
    Reveal,
    /// Close a round whose windows have passed. Permissionless.
    Finalize {
        /// Defaults to the round the contract is currently running.
        round_id: Option<u64>,
    },
}

/// Everything the beacon commands need.
struct Context {
    beacon: Arc<dyn BeaconClient>,
    chain: Arc<dyn ChainClient>,
    repo: Repo,
    signer: NodeSigner,
    contract_id: [u8; 32],
}

impl Context {
    async fn open(config: &Config) -> Result<Self> {
        let cli = CliBeacon::new(&config.network)?;
        let contract_id = cli.contract_id()?;
        Ok(Self {
            beacon: Arc::new(cli),
            chain: Arc::new(CliChain::new(config.network.clone())?),
            repo: Repo::new(aphelion_node::db::connect(&config.database).await?),
            signer: NodeSigner::load(
                &config.node.key_path,
                aphelion_node::strkey::contract_id_bytes(&config.network.aggregator_contract)?,
            )?,
            contract_id,
        })
    }

    fn pubkey(&self) -> String {
        self.signer.public_key_hex()
    }

    /// Assemble what [`decide`] needs.
    async fn snapshot(&self, config: &Config) -> Result<Snapshot> {
        let pubkey = self.pubkey();
        let now = self.chain.ledger_time().await?;

        // Weight rather than mere registration: the contract asks the registry
        // the same question, so a jailed node learns here that it cannot
        // commit instead of learning it from a reverted transaction.
        let eligible = self
            .chain
            .node_info(&pubkey)
            .await?
            .map(|n| n.weight_bps > 0)
            .unwrap_or(false);

        let count = self.beacon.round_count().await?;
        let round = if count == 0 {
            None
        } else {
            self.beacon.round(count).await?
        };

        let mut view = None;
        if let Some(r) = &round {
            let stored = self.repo.beacon_round(r.id).await?;
            let secret_on_disk = stored.is_some();
            let params = self.beacon.params().await?;
            view = Some(aphelion_node::engine::beacon::RoundView {
                id: r.id,
                status: r.status,
                opened_at: r.opened_at,
                commit_deadline: r.commit_deadline,
                reveal_deadline: r.reveal_deadline,
                committed: r.committed.len() as u32,
                revealed: r.revealed.len() as u32,
                min_participants: params.min_participants,
                min_round_interval: params.min_round_interval,
                our_part: r.our_part(&pubkey, secret_on_disk),
            });
        }

        // Owed reveals come from the database rather than from the chain: the
        // chain knows this node committed, and only the database knows whether
        // the secret to open it still exists.
        let mut owed = Vec::new();
        for row in self.repo.beacon_reveals_owed().await? {
            let id = row.round_id();
            let chain_round = self.beacon.round(id).await?;
            let Some(chain_round) = chain_round else {
                continue;
            };
            if chain_round.has_revealed(&pubkey) {
                // It landed and this node did not record it. Reconcile rather
                // than try again: a second reveal is refused by the contract
                // and would be a wasted fee every tick forever.
                self.repo.mark_revealed(id).await?;
                continue;
            }
            // A round the contract has finished with is not owed anything,
            // whatever this table says -- the window is shut and the penalty,
            // if any, has already been applied. Reconciled for the same reason
            // as the branch above and in the other direction: the reveal will
            // never be accepted now, and a row left owed would buy a refusal
            // every tick for as long as the node runs.
            //
            // After the revealed check, never before it. A round can be
            // finalized *and* carry this node's reveal, and that is the
            // ordinary ending rather than a closure.
            if chain_round.status.is_over() {
                self.repo.mark_closed_unrevealed(id).await?;
                continue;
            }
            owed.push(OwedReveal {
                round_id: id,
                reveal_deadline: chain_round.reveal_deadline,
                secret_lost: row.secret().is_err(),
            });
        }

        Ok(Snapshot {
            now,
            round: view,
            eligible,
            participate: config.beacon.participate,
            owed,
        })
    }
}

/// The secret, its commitment, and the signature over it.
fn build_commitment(
    contract_id: &[u8; 32],
    round_id: u64,
    pubkey: &[u8; 32],
    secret: &[u8; 32],
) -> [u8; 32] {
    let preimage =
        aphelion_core::message::commitment_preimage(contract_id, round_id, pubkey, secret);
    Sha256::digest(preimage).into()
}

pub async fn run(config: &Config, cmd: BeaconCmd) -> Result<()> {
    let ctx = Context::open(config).await?;
    match cmd {
        BeaconCmd::Status { json } => status(config, &ctx, json).await,
        BeaconCmd::Tick => {
            let action = decide(&ctx.snapshot(config).await?);
            perform(&ctx, &action).await
        }

        BeaconCmd::Open => {
            let landed = ctx.beacon.open_round().await?;
            println!("round opened");
            landed_line(landed.tx_hash);
            Ok(())
        }
        BeaconCmd::Commit => {
            let snapshot = ctx.snapshot(config).await?;
            let Some(round) = snapshot.round.as_ref() else {
                return Err(NodeError::Chain(
                    "no round is running; `beacon open` starts one".into(),
                ));
            };
            commit(&ctx, round.id).await
        }
        BeaconCmd::Reveal => {
            let snapshot = ctx.snapshot(config).await?;
            match snapshot.owed.first() {
                Some(owed) if !owed.secret_lost => reveal(&ctx, owed.round_id).await,
                Some(owed) => Err(lost(owed.round_id)),
                None => {
                    println!("nothing to reveal: no commitment of this node's is unopened");
                    Ok(())
                }
            }
        }
        BeaconCmd::Finalize { round_id } => {
            let id = match round_id {
                Some(id) => id,
                None => ctx.beacon.round_count().await?,
            };
            let landed = ctx.beacon.finalize(id).await?;
            println!("round {id} finalized");
            landed_line(landed.tx_hash);
            Ok(())
        }
    }
}

/// One pass: read, decide, act.
///
/// What `beacon tick` runs and what the loop in `main.rs` calls every
/// `[beacon] interval`. Opening a context per pass costs a key load and a
/// connection; the alternative is holding the signing key in a long-lived
/// task for the sake of one call a minute.
pub async fn tick(config: &Config) -> Result<()> {
    let ctx = Context::open(config).await?;
    let action = decide(&ctx.snapshot(config).await?);
    perform(&ctx, &action).await
}

/// Carry out one decision.
async fn perform(ctx: &Context, action: &Action) -> Result<()> {
    match action {
        Action::OpenRound => {
            let landed = ctx.beacon.open_round().await?;
            println!("opened a round");
            landed_line(landed.tx_hash);
            Ok(())
        }
        Action::Commit { round_id } | Action::SubmitStoredCommitment { round_id } => {
            commit(ctx, *round_id).await
        }
        Action::Reveal { round_id, .. } => reveal(ctx, *round_id).await,
        Action::Finalize { round_id } => {
            let landed = ctx.beacon.finalize(*round_id).await?;
            println!("finalized round {round_id}");
            landed_line(landed.tx_hash);
            Ok(())
        }
        Action::SecretLost { round_id } => Err(lost(*round_id)),
        Action::Idle { because } => {
            println!("nothing to do: {}", describe_idle(*because));
            Ok(())
        }
    }
}

/// Store a secret, then commit to it.
///
/// The order is the feature. `store_beacon_secret` is idempotent on the round
/// id, so a retry after a failed submission commits to the secret already
/// stored rather than generating a second one — which would produce a
/// commitment the stored secret does not open, and a penalty at finalisation.
async fn commit(ctx: &Context, round_id: u64) -> Result<()> {
    let pubkey_bytes = ctx.signer.public_key().to_bytes();

    let existing = ctx.repo.beacon_round(round_id).await?;
    let secret: [u8; 32] = match &existing {
        Some(row) => row.secret()?,
        None => {
            let mut secret = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut secret);
            secret
        }
    };

    let commitment = build_commitment(&ctx.contract_id, round_id, &pubkey_bytes, &secret);

    // Durable first. Everything after this point can fail and be retried.
    if existing.is_none() {
        ctx.repo
            .store_beacon_secret(round_id, &hex::encode(secret), &hex::encode(commitment))
            .await?;
    }

    let signature = ctx
        .signer
        .sign_commitment(&ctx.contract_id, round_id, &commitment);

    let landed = ctx
        .beacon
        .commit(
            &ctx.pubkey(),
            &hex::encode(commitment),
            &hex::encode(signature),
        )
        .await?;
    ctx.repo.mark_committed(round_id).await?;

    println!("committed to round {round_id}");
    println!("  commitment {}", hex::encode(commitment));
    landed_line(landed.tx_hash);
    println!("  the reveal is now owed; this node will be penalised if it does not send it");
    Ok(())
}

async fn reveal(ctx: &Context, round_id: u64) -> Result<()> {
    let row = ctx
        .repo
        .beacon_round(round_id)
        .await?
        .ok_or_else(|| lost(round_id))?;
    let secret = row.secret()?;

    let landed = ctx
        .beacon
        .reveal(&ctx.pubkey(), &hex::encode(secret))
        .await?;
    ctx.repo.mark_revealed(round_id).await?;

    println!("revealed round {round_id}");
    landed_line(landed.tx_hash);
    Ok(())
}

fn lost(round_id: u64) -> NodeError {
    NodeError::Other(anyhow::anyhow!(
        "round {round_id}: this node's commitment is on the ledger and the secret that opens \
         it is not in the database. Nothing can recover it -- the commitment is a hash. The \
         no-show penalty will be applied when the round is finalized.\n\n\
         This means the `beacon_rounds` row was lost after the commitment was sent: a restore \
         from a backup older than the commitment, or a database that was rebuilt. The secret \
         is written before the commitment is submitted precisely so that a crash cannot cause \
         this, so a restore is the likely cause."
    ))
}

fn describe_idle(idle: Idle) -> &'static str {
    match idle {
        Idle::NotParticipating => {
            "[beacon] participate is off, and nothing is owed. Turn it on to take part."
        }
        Idle::NotEligible => {
            "this node carries no weight: unregistered, jailed or exiting. The contract \
             refuses commitments from it."
        }
        Idle::WaitingToReveal => "committed; waiting for the commit window to close",
        Idle::Done => "this node has done everything this round asks of it",
        Idle::MissedTheWindow => "the commit window closed before this node entered the round",
        Idle::BetweenRounds => {
            "the last round is closed and the next one may not open yet; see \
             `min_round_interval` in the contract's configuration"
        }
    }
}

fn landed_line(tx_hash: Option<String>) {
    match tx_hash {
        Some(h) => println!("  tx {h}"),
        None => println!("  landed (the CLI printed no transaction hash)"),
    }
}

async fn status(config: &Config, ctx: &Context, json: bool) -> Result<()> {
    let snapshot = ctx.snapshot(config).await?;
    let action = decide(&snapshot);
    let params = ctx.beacon.params().await?;
    let latest = ctx.beacon.latest().await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": ctx.pubkey(),
                "params": params,
                "snapshot": snapshot,
                "action": action,
                "latest": latest.as_ref().map(|(id, out)| serde_json::json!({
                    "round_id": id, "beacon": out
                })),
            }))
            .map_err(|e| NodeError::Other(e.into()))?
        );
        return Ok(());
    }

    println!("node       : {}", ctx.pubkey());
    println!(
        "eligible   : {}",
        if snapshot.eligible {
            "yes"
        } else {
            "no  (unregistered, jailed or exiting)"
        }
    );
    println!(
        "participate: {}",
        if snapshot.participate { "yes" } else { "no" }
    );

    match latest {
        Some((id, output)) => println!("latest     : round {id}  {output}"),
        None => println!("latest     : no finalized round yet"),
    }

    match &snapshot.round {
        None => println!("\nno round running"),
        Some(r) => {
            println!("\nround {}", r.id);
            println!("  status   : {:?}", r.status);
            println!(
                "  commits  : {} ({} needed to publish)",
                r.committed, r.min_participants
            );
            println!("  reveals  : {}", r.revealed);
            println!("  our part : {}", describe_part(r.our_part));
            if r.status.is_over() {
                // Its deadlines are behind it and counting them down further
                // says nothing. What an operator wants from a closed round is
                // when the next one can start, which is the number this loop
                // is now waiting on.
                let opens_in = (r.opened_at + r.min_round_interval) as i64 - snapshot.now as i64;
                if opens_in > 0 {
                    println!("  next     : a round may open in {opens_in}s");
                } else {
                    println!("  next     : a round may open now");
                }
            } else {
                println!(
                    "  commit by: {}s   reveal by: {}s",
                    r.commit_deadline as i64 - snapshot.now as i64,
                    r.reveal_deadline as i64 - snapshot.now as i64
                );
            }
        }
    }

    if !snapshot.owed.is_empty() {
        println!("\nreveals owed");
        for owed in &snapshot.owed {
            println!(
                "  round {}  in {}s{}",
                owed.round_id,
                owed.reveal_deadline as i64 - snapshot.now as i64,
                if owed.secret_lost {
                    "   SECRET LOST -- this will be penalised"
                } else {
                    ""
                }
            );
        }
    }

    // The one configuration mistake that costs stake rather than a round: a
    // poll slower than the window it has to catch. Checked against the
    // contract's real number rather than the ceiling `validate` guesses at.
    let interval = config.beacon.interval.as_secs();
    if snapshot.participate && interval * 2 > params.reveal_window {
        println!(
            "\nWARNING: beacon.interval is {interval}s and the contract's reveal window is \
             {}s. A node that looks less than twice per window can sleep through one, and a \
             missed reveal is slashed.",
            params.reveal_window
        );
    }

    println!("\nnext: {}", describe_action(&action));
    Ok(())
}

fn describe_part(part: OurPart) -> &'static str {
    match part {
        OurPart::None => "not in this round",
        OurPart::SecretStored => "secret stored, commitment not sent",
        OurPart::Committed => "committed, reveal owed",
        OurPart::SecretLost => "committed, SECRET LOST",
        OurPart::Revealed => "revealed",
    }
}

fn describe_action(action: &Action) -> String {
    match action {
        Action::OpenRound => "open a round (`beacon open`)".into(),
        Action::Commit { round_id } => format!("commit to round {round_id} (`beacon commit`)"),
        Action::SubmitStoredCommitment { round_id } => {
            format!("send the stored commitment for round {round_id} (`beacon commit`)")
        }
        Action::Reveal {
            round_id,
            seconds_left,
        } => format!("reveal round {round_id}, {seconds_left}s left (`beacon reveal`)"),
        Action::Finalize { round_id } => {
            format!("finalize round {round_id} (`beacon finalize {round_id}`)")
        }
        Action::SecretLost { round_id } => {
            format!("nothing can be done for round {round_id}: the secret is gone")
        }
        Action::Idle { because } => describe_idle(*because).to_string(),
    }
}
