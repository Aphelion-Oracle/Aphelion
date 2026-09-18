//! Taking part in the randomness beacon.
//!
//! The contract's shape is commit, wait, reveal. For a node that means holding
//! a secret across a window measured in minutes, and the whole design of this
//! module follows from one fact about that window: **a node that commits and
//! cannot reveal is slashed.** Not for being wrong — for going quiet after
//! everyone else has spoken, which the contract cannot tell apart from
//! withholding on purpose. See `contracts/randomness` for why it cannot.
//!
//! Three consequences.
//!
//! **The secret is durable before the commitment is sent.** It is written to
//! Postgres and that write is committed first; only then is the commitment
//! submitted. The other order is a node that crashes in the wrong millisecond
//! and pays for it, and the failure is silent for a whole reveal window.
//!
//! **Committing is opt-in; revealing is not.** An operator turns
//! `[beacon] participate` on, the way they turn absence sweeps on, because it
//! spends transaction fees on work nobody is obliged to do. But once a
//! commitment is on the ledger the reveal is owed, and the loop will send it
//! whether or not participation has since been switched off. Turning the
//! setting off stops the node starting anything new; it does not abandon a
//! round it has already entered.
//!
//! **A lost secret is reported, not papered over.** If the row is gone and the
//! commitment is not — a restore from an old backup, someone dropping a table
//! — there is nothing to do and the stake is already forfeit. That reads as a
//! distinct state here rather than as "nothing to do", because the two look
//! identical from outside and only one of them should wake somebody.
//!
//! **A closed round is where the next one starts.** The contract runs one
//! round at a time and `open_round` is permissionless, so nothing opens round
//! N+1 unless somebody decides to. That somebody is this loop, and the
//! decision belongs at the moment the previous round closes rather than to a
//! separate schedule, because that moment is the only one at which the
//! contract will accept the call. A beacon whose rounds have to be started by
//! hand produces one value and stops.
//!
//! The judgement lives in [`decide`], a pure function of a snapshot, so the
//! interesting part is testable against struct literals rather than a chain.
//! The same split [`super::duty`] and [`super::status`] use.

use serde::Serialize;

/// Where a round is, as the contract reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStatus {
    Committing,
    Revealing,
    Finalized,
    Failed,
}

impl RoundStatus {
    /// Whether the contract is done with this round.
    ///
    /// The distinction every branch below turns on, and the one that decides
    /// whether a transaction is worth paying for: `finalize` and `reveal` both
    /// revert on a round in either of these states, and no amount of this
    /// node's diligence changes that.
    pub fn is_over(self) -> bool {
        matches!(self, Self::Finalized | Self::Failed)
    }
}

/// What this node knows about its own part in a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OurPart {
    /// Nothing sent, nothing stored.
    None,
    /// A secret is stored and the commitment has not been submitted. The
    /// in-between state that exists because the secret is written first.
    SecretStored,
    /// The commitment is on the ledger and the secret is in hand.
    Committed,
    /// The commitment is on the ledger and the secret is gone.
    SecretLost,
    /// Revealed.
    Revealed,
}

/// Everything the decision depends on.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Ledger time, not this machine's clock: every deadline below is the
    /// contract's.
    pub now: u64,
    /// The round the contract is currently running, if any.
    pub round: Option<RoundView>,
    /// Whether this node is registered, unjailed and therefore allowed to
    /// commit at all.
    pub eligible: bool,
    /// `[beacon] participate`.
    pub participate: bool,
    /// Rounds this node committed to and has not revealed, oldest first.
    /// Usually empty or one; more than one means several rounds went by
    /// without the node running.
    pub owed: Vec<OwedReveal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoundView {
    pub id: u64,
    pub status: RoundStatus,
    /// When the contract opened it. The next round's earliest opening is this
    /// plus `min_round_interval`, so this is half of the answer to "may I open
    /// one yet".
    pub opened_at: u64,
    pub commit_deadline: u64,
    pub reveal_deadline: u64,
    pub committed: u32,
    pub revealed: u32,
    pub min_participants: u32,
    /// `min_round_interval`, the contract's floor on how often a round may
    /// open. The other half.
    pub min_round_interval: u64,
    pub our_part: OurPart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OwedReveal {
    pub round_id: u64,
    pub reveal_deadline: u64,
    pub secret_lost: bool,
}

/// What to do now.
///
/// One action rather than a list: the loop does one thing per tick and the
/// ordering below is the priority, so returning a list would only invite a
/// caller to act on the second item when the first was the urgent one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Open a round. Permissionless housekeeping — it costs a fee and pays
    /// nothing back, so it is only ever offered when this node also intends to
    /// take part in the round it opens.
    OpenRound,
    /// Generate a secret, store it, then commit.
    Commit { round_id: u64 },
    /// Submit the commitment for a secret already stored. The resume path
    /// after a crash between the write and the send.
    SubmitStoredCommitment { round_id: u64 },
    /// Open a commitment. Stake is at risk until this lands.
    Reveal {
        round_id: u64,
        /// Seconds until the reveal window closes. Negative if it already has,
        /// which the loop still attempts — the contract is the authority on
        /// its own deadline and a node's clock is not.
        seconds_left: i64,
    },
    /// Close a round whose windows have passed. Housekeeping; anybody may.
    Finalize { round_id: u64 },
    /// The commitment is on the ledger, the secret is not, and nothing can be
    /// done about it. Carried as an action so the caller has to handle it.
    SecretLost { round_id: u64 },
    /// Nothing to do, with the reason — "not participating" and "waiting for
    /// the reveal window" are both idle and only one of them is worth saying
    /// twice.
    Idle { because: Idle },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Idle {
    /// No `randomness_contract`, or `participate` is off and nothing is owed.
    NotParticipating,
    /// Registered but carrying no weight: unregistered, jailed or exiting.
    NotEligible,
    /// Committed, and the commit window has not closed yet.
    WaitingToReveal,
    /// This node has already done everything this round asks of it.
    Done,
    /// A round is running that this node did not enter, and the commit window
    /// has closed. Nothing to do until the next one.
    MissedTheWindow,
    /// The last round is over and the contract's `min_round_interval` has not
    /// elapsed since it opened. Opening now would revert.
    ///
    /// Distinct from [`Idle::Done`] because it is the state a beacon spends
    /// most of its time in once it is running properly, and reading "done" for
    /// it would leave an operator unable to tell a beacon that is between
    /// rounds from one that has stopped.
    BetweenRounds,
}

/// The whole judgement.
pub fn decide(s: &Snapshot) -> Action {
    // Owed reveals come first, before eligibility, before participation, and
    // before whatever the current round is doing. A node that has been jailed
    // since committing still owes the reveal; a node whose operator switched
    // participation off still owes it; a node three rounds behind owes the
    // oldest one first. The commitment is on the ledger and the penalty does
    // not care why the node stopped.
    if let Some(owed) = s.owed.first() {
        if owed.secret_lost {
            return Action::SecretLost {
                round_id: owed.round_id,
            };
        }
        return Action::Reveal {
            round_id: owed.round_id,
            seconds_left: owed.reveal_deadline as i64 - s.now as i64,
        };
    }

    // No round has ever been opened. Nothing bounds the first one but the
    // willingness to pay for it.
    let Some(round) = &s.round else {
        return open_next(s, None);
    };

    // A round the contract has closed asks nothing of anybody, whatever this
    // node's part in it was: `finalize` reverts with `AlreadyFinalized` and
    // `reveal` reverts too, so every action below would be a fee spent on a
    // refusal. The only live question is whether to open the round after it,
    // and that question is the same for a node that revealed, a node that
    // missed the window and a node that was never eligible.
    //
    // [`OurPart::SecretLost`] included, which is the one that looks like it
    // should be an exception. It is not: the commitment went unopened, the
    // no-show penalty was charged by `finalize`, and there has never been
    // anything for the node to do about it — so raising it as an error on
    // every tick for the rest of the deployment's life reports a loss that
    // already happened, over and over, in the channel reserved for things
    // somebody can act on. It is raised for as long as the round is live,
    // which is the window in which an operator restoring the right backup
    // could still have opened it, and `beacon status` goes on showing it
    // afterwards.
    if round.status.is_over() {
        return open_next(s, Some(round));
    }

    match round.our_part {
        // The resume path: the secret survived, the commitment never went. Do
        // this even if participation has since been switched off — the secret
        // is already stored and committing it is cheaper than the alternative
        // of a round with one fewer participant.
        OurPart::SecretStored if s.now <= round.commit_deadline => {
            Action::SubmitStoredCommitment { round_id: round.id }
        }
        // The window closed on a secret that was never committed. Nothing is
        // owed -- nothing reached the ledger -- so this is idle rather than
        // lost.
        OurPart::SecretStored => Action::Idle {
            because: Idle::MissedTheWindow,
        },

        OurPart::SecretLost => Action::SecretLost { round_id: round.id },

        OurPart::Committed if s.now > round.commit_deadline => Action::Reveal {
            round_id: round.id,
            seconds_left: round.reveal_deadline as i64 - s.now as i64,
        },
        OurPart::Committed => Action::Idle {
            because: Idle::WaitingToReveal,
        },

        OurPart::Revealed => {
            // Everyone who committed has opened, or the window is over. Either
            // way the round can be closed and nobody is obliged to close it,
            // which is exactly when a node that is already here should. A
            // round already closed never reaches this far.
            if s.now > round.reveal_deadline || round.revealed >= round.committed {
                Action::Finalize { round_id: round.id }
            } else {
                Action::Idle {
                    because: Idle::Done,
                }
            }
        }

        OurPart::None => {
            if !s.participate {
                return Action::Idle {
                    because: Idle::NotParticipating,
                };
            }
            if !s.eligible {
                return Action::Idle {
                    because: Idle::NotEligible,
                };
            }
            if round.status == RoundStatus::Committing && s.now <= round.commit_deadline {
                Action::Commit { round_id: round.id }
            } else if s.now > round.reveal_deadline {
                // A round nobody closed. Housekeeping, and this node is here.
                // Still open, because a closed one was returned above.
                Action::Finalize { round_id: round.id }
            } else {
                Action::Idle {
                    because: Idle::MissedTheWindow,
                }
            }
        }
    }
}

/// Whether to open the round after `previous`, which is `None` only for the
/// very first round a deployment ever runs.
///
/// Opening is housekeeping: it costs a fee and pays nothing back, so it is
/// offered only to a node that means to take part in the round it opens. A
/// node that opens rounds it will not join is paying for everybody else's
/// beacon.
///
/// The interval is checked here rather than left to the contract for the usual
/// reason: `open_round` reverts with `RoundTooSoon` *after* the transaction has
/// been paid for, and with several operators running this loop every one of
/// them would buy that refusal on every tick between rounds. Checking it costs
/// nothing — `opened_at` and `min_round_interval` are already on the snapshot.
///
/// It fails towards not paying, which is the opposite of the round loop's
/// [`super::round::Authority`], and the asymmetry is deliberate. Silence there
/// is a missed round somebody may charge to this node's reputation; silence
/// here costs nothing at all, because the next tick asks again and any one of
/// the other operators opening the round serves this node just as well as
/// opening it itself.
fn open_next(s: &Snapshot, previous: Option<&RoundView>) -> Action {
    if !s.participate {
        return Action::Idle {
            because: Idle::NotParticipating,
        };
    }
    if !s.eligible {
        return Action::Idle {
            because: Idle::NotEligible,
        };
    }
    if let Some(previous) = previous {
        if s.now.saturating_sub(previous.opened_at) < previous.min_round_interval {
            return Action::Idle {
                because: Idle::BetweenRounds,
            };
        }
    }
    Action::OpenRound
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_735_689_600;
    /// The round in these fixtures opened here, a hundred seconds before
    /// `NOW`, so that "how long since it opened" is a different number from
    /// "how long until anything expires".
    const OPENED_AT: u64 = NOW - 100;
    const INTERVAL: u64 = 600;

    fn round(our_part: OurPart) -> RoundView {
        RoundView {
            id: 7,
            status: RoundStatus::Committing,
            opened_at: OPENED_AT,
            commit_deadline: NOW + 100,
            reveal_deadline: NOW + 400,
            committed: 3,
            revealed: 0,
            min_participants: 3,
            min_round_interval: INTERVAL,
            our_part,
        }
    }

    /// A snapshot whose round is over, however this node left it.
    fn finished(our_part: OurPart, status: RoundStatus) -> Snapshot {
        let mut s = snapshot(our_part);
        s.now = OPENED_AT + INTERVAL;
        if let Some(r) = s.round.as_mut() {
            r.status = status;
        }
        s
    }

    fn snapshot(our_part: OurPart) -> Snapshot {
        Snapshot {
            now: NOW,
            round: Some(round(our_part)),
            eligible: true,
            participate: true,
            owed: Vec::new(),
        }
    }

    #[test]
    fn an_eligible_node_commits_inside_the_window() {
        assert_eq!(
            decide(&snapshot(OurPart::None)),
            Action::Commit { round_id: 7 }
        );
    }

    #[test]
    fn a_committed_node_waits_until_the_commit_window_closes() {
        // Revealing early is refused by the contract, and asking is a wasted
        // fee every tick until the window turns over.
        assert_eq!(
            decide(&snapshot(OurPart::Committed)),
            Action::Idle {
                because: Idle::WaitingToReveal
            }
        );
    }

    #[test]
    fn a_committed_node_reveals_once_the_window_turns_over() {
        let mut s = snapshot(OurPart::Committed);
        s.now = NOW + 101;
        assert_eq!(
            decide(&s),
            Action::Reveal {
                round_id: 7,
                seconds_left: 299
            }
        );
    }

    #[test]
    fn an_owed_reveal_outranks_everything_else() {
        // Jailed since committing, participation switched off, a fresh round
        // running that this node could join -- and the only thing that matters
        // is the commitment already on the ledger.
        let mut s = snapshot(OurPart::None);
        s.eligible = false;
        s.participate = false;
        s.owed = vec![OwedReveal {
            round_id: 5,
            reveal_deadline: NOW + 50,
            secret_lost: false,
        }];
        assert_eq!(
            decide(&s),
            Action::Reveal {
                round_id: 5,
                seconds_left: 50
            }
        );
    }

    #[test]
    fn the_oldest_owed_reveal_goes_first() {
        let mut s = snapshot(OurPart::None);
        s.owed = vec![
            OwedReveal {
                round_id: 5,
                reveal_deadline: NOW + 10,
                secret_lost: false,
            },
            OwedReveal {
                round_id: 6,
                reveal_deadline: NOW + 500,
                secret_lost: false,
            },
        ];
        assert_eq!(
            decide(&s),
            Action::Reveal {
                round_id: 5,
                seconds_left: 10
            }
        );
    }

    #[test]
    fn a_reveal_past_its_deadline_is_still_attempted() {
        // The contract is the authority on its own deadline and this node's
        // clock is not. Declining to try because our arithmetic says the
        // window closed would forfeit stake over a few seconds of skew.
        let mut s = snapshot(OurPart::None);
        s.owed = vec![OwedReveal {
            round_id: 5,
            reveal_deadline: NOW - 5,
            secret_lost: false,
        }];
        assert_eq!(
            decide(&s),
            Action::Reveal {
                round_id: 5,
                seconds_left: -5
            }
        );
    }

    #[test]
    fn a_lost_secret_is_its_own_state_rather_than_nothing_to_do() {
        let mut s = snapshot(OurPart::None);
        s.owed = vec![OwedReveal {
            round_id: 5,
            reveal_deadline: NOW + 50,
            secret_lost: true,
        }];
        assert_eq!(decide(&s), Action::SecretLost { round_id: 5 });
    }

    #[test]
    fn a_stored_secret_that_was_never_committed_is_submitted() {
        assert_eq!(
            decide(&snapshot(OurPart::SecretStored)),
            Action::SubmitStoredCommitment { round_id: 7 }
        );
    }

    #[test]
    fn a_stored_secret_is_submitted_even_with_participation_switched_off() {
        // It is already on disk; a round with one fewer participant is worse
        // than a fee, and nothing is owed either way.
        let mut s = snapshot(OurPart::SecretStored);
        s.participate = false;
        assert_eq!(decide(&s), Action::SubmitStoredCommitment { round_id: 7 });
    }

    #[test]
    fn a_stored_secret_past_the_window_owes_nothing() {
        // Nothing reached the ledger, so nothing can be charged.
        let mut s = snapshot(OurPart::SecretStored);
        s.now = NOW + 101;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::MissedTheWindow
            }
        );
    }

    #[test]
    fn participation_off_starts_nothing_new() {
        let mut s = snapshot(OurPart::None);
        s.participate = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotParticipating
            }
        );
    }

    #[test]
    fn a_jailed_node_does_not_commit() {
        let mut s = snapshot(OurPart::None);
        s.eligible = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotEligible
            }
        );
    }

    #[test]
    fn a_participating_node_opens_a_round_when_there_is_none() {
        let mut s = snapshot(OurPart::None);
        s.round = None;
        assert_eq!(decide(&s), Action::OpenRound);
    }

    #[test]
    fn a_node_that_will_not_take_part_does_not_open_rounds_for_others() {
        // Opening costs a fee and pays nothing. A node that opens rounds it
        // will not join is subsidising everybody else's beacon.
        let mut s = snapshot(OurPart::None);
        s.round = None;
        s.participate = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotParticipating
            }
        );

        s.participate = true;
        s.eligible = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotEligible
            }
        );
    }

    #[test]
    fn a_node_that_missed_the_commit_window_waits_for_the_next_round() {
        let mut s = snapshot(OurPart::None);
        s.now = NOW + 101;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::MissedTheWindow
            }
        );
    }

    #[test]
    fn a_round_nobody_closed_is_closed_by_whoever_is_here() {
        let mut s = snapshot(OurPart::None);
        s.now = NOW + 500; // past the reveal deadline
        assert_eq!(decide(&s), Action::Finalize { round_id: 7 });
    }

    #[test]
    fn a_revealed_node_finalizes_once_every_committer_has_opened() {
        let mut s = snapshot(OurPart::Revealed);
        s.now = NOW + 200;
        if let Some(r) = s.round.as_mut() {
            r.status = RoundStatus::Revealing;
            r.revealed = 3;
            r.committed = 3;
        }
        assert_eq!(decide(&s), Action::Finalize { round_id: 7 });
    }

    #[test]
    fn a_revealed_node_waits_while_somebody_may_still_open() {
        let mut s = snapshot(OurPart::Revealed);
        s.now = NOW + 200;
        if let Some(r) = s.round.as_mut() {
            r.status = RoundStatus::Revealing;
            r.revealed = 2;
            r.committed = 3;
        }
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::Done
            }
        );
    }

    #[test]
    fn a_finished_round_is_where_the_next_one_starts() {
        // The bug this replaced: a finished round returned `Idle::Done` and
        // `OpenRound` was reachable only when no round had ever existed. So
        // the beacon produced exactly one value in the life of a deployment
        // and then sat still, with every participating node reporting that it
        // had done everything asked of it -- which was true, and the reason
        // nobody would have noticed.
        for status in [RoundStatus::Finalized, RoundStatus::Failed] {
            for part in [
                OurPart::Revealed,
                OurPart::None,
                OurPart::SecretStored,
                OurPart::SecretLost,
            ] {
                assert_eq!(
                    decide(&finished(part, status)),
                    Action::OpenRound,
                    "{status:?} / {part:?}"
                );
            }
        }
    }

    #[test]
    fn a_finished_round_is_never_finalized_again() {
        // `finalize` reverts with `AlreadyFinalized`, after the fee. The node
        // that pays it is the one that was not in the round, because it is the
        // one whose branch reads "past the reveal deadline, so close it".
        let mut s = finished(OurPart::None, RoundStatus::Finalized);
        s.now = NOW + 5_000; // long past every deadline
        assert_eq!(decide(&s), Action::OpenRound);
    }

    #[test]
    fn a_round_that_closed_too_recently_is_not_reopened() {
        // `open_round` reverts with `RoundTooSoon` after the fee, and with
        // several operators running this loop every one of them would buy that
        // refusal on every tick until the interval elapsed.
        let mut s = finished(OurPart::Revealed, RoundStatus::Finalized);
        s.now = OPENED_AT + INTERVAL - 1;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::BetweenRounds
            }
        );

        s.now = OPENED_AT + INTERVAL;
        assert_eq!(decide(&s), Action::OpenRound);
    }

    #[test]
    fn the_interval_is_measured_from_the_opening_not_from_the_close() {
        // As the contract measures it. A round that finalizes early does not
        // buy the next one an earlier start, and one that runs to its deadline
        // does not push it out.
        let mut s = finished(OurPart::Revealed, RoundStatus::Finalized);
        s.now = OPENED_AT + INTERVAL;
        if let Some(r) = s.round.as_mut() {
            // Closed the moment it opened; the floor has still elapsed.
            r.reveal_deadline = OPENED_AT;
        }
        assert_eq!(decide(&s), Action::OpenRound);
    }

    #[test]
    fn a_node_that_will_not_take_part_does_not_open_the_next_round_either() {
        // The same rule as the first round: opening is a fee that pays nothing
        // back, and a node that will not join the round it opens is
        // subsidising everybody else's beacon.
        let mut s = finished(OurPart::Revealed, RoundStatus::Finalized);
        s.participate = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotParticipating
            }
        );

        s.participate = true;
        s.eligible = false;
        assert_eq!(
            decide(&s),
            Action::Idle {
                because: Idle::NotEligible
            }
        );
    }

    #[test]
    fn an_owed_reveal_still_outranks_opening_the_next_round() {
        // The round on the ledger is over and a new one could be opened; the
        // commitment this node has not opened is on an earlier round and is
        // still the thing with stake behind it.
        let mut s = finished(OurPart::Revealed, RoundStatus::Finalized);
        s.owed = vec![OwedReveal {
            round_id: 5,
            reveal_deadline: s.now + 50,
            secret_lost: false,
        }];
        assert_eq!(
            decide(&s),
            Action::Reveal {
                round_id: 5,
                seconds_left: 50
            }
        );
    }

    #[test]
    fn a_live_round_is_still_finalized_by_whoever_is_here() {
        // The check hoisted above the match must not have taken this with it:
        // a round past its reveal deadline that nobody has closed is exactly
        // the one that needs closing.
        for status in [RoundStatus::Committing, RoundStatus::Revealing] {
            let mut s = snapshot(OurPart::None);
            s.now = NOW + 500;
            if let Some(r) = s.round.as_mut() {
                r.status = status;
            }
            assert_eq!(decide(&s), Action::Finalize { round_id: 7 }, "{status:?}");
        }
    }
}
