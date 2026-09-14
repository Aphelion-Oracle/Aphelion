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
    pub commit_deadline: u64,
    pub reveal_deadline: u64,
    pub committed: u32,
    pub revealed: u32,
    pub min_participants: u32,
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

    let Some(round) = &s.round else {
        // No round running. Opening one is housekeeping, so it is offered only
        // to a node that means to take part in it: a node that opens rounds it
        // will not join is paying for everybody else's beacon.
        if s.participate && s.eligible {
            return Action::OpenRound;
        }
        return Action::Idle {
            because: if !s.participate {
                Idle::NotParticipating
            } else {
                Idle::NotEligible
            },
        };
    };

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
            // which is exactly when a node that is already here should.
            if round.status == RoundStatus::Finalized || round.status == RoundStatus::Failed {
                Action::Idle {
                    because: Idle::Done,
                }
            } else if s.now > round.reveal_deadline || round.revealed >= round.committed {
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
                Action::Finalize { round_id: round.id }
            } else {
                Action::Idle {
                    because: Idle::MissedTheWindow,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_735_689_600;

    fn round(our_part: OurPart) -> RoundView {
        RoundView {
            id: 7,
            status: RoundStatus::Committing,
            commit_deadline: NOW + 100,
            reveal_deadline: NOW + 400,
            committed: 3,
            revealed: 0,
            min_participants: 3,
            our_part,
        }
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
    fn a_finished_round_asks_for_nothing() {
        for status in [RoundStatus::Finalized, RoundStatus::Failed] {
            let mut s = snapshot(OurPart::Revealed);
            s.now = NOW + 500;
            if let Some(r) = s.round.as_mut() {
                r.status = status;
            }
            assert_eq!(
                decide(&s),
                Action::Idle {
                    because: Idle::Done
                },
                "{status:?}"
            );
        }
    }
}
