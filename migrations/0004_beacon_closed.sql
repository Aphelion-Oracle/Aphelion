-- Beacon rounds that closed without this node's reveal.
--
-- `beacon_rounds` had two terminal states and needed three. A row was owed a
-- reveal from the moment `committed_at` was set until `revealed_at` was, and
-- nothing else could end it -- so a round the contract finalized or failed
-- while this node's commitment was still unopened stayed owed forever. The
-- loop went on offering a reveal the contract refuses, once per tick, for as
-- long as the row existed, and bought a reverted transaction each time.
--
-- Recording it as revealed would have stopped that and been a lie: the row is
-- the node's own account of what it did with a secret, and this is the case
-- where it did not open one. The distinction is not bookkeeping. A row that
-- ends here is the only local trace of a no-show penalty -- the stake and the
-- reputation were taken on chain by `finalize`, and `closed_at` is what lets
-- an operator find out which round it was and when.
--
-- Null for every row written before this migration, which is correct: a node
-- upgrading into it has no history of closures it never detected, and the
-- reconciliation that sets this column will fill it in on the next tick for
-- any round still outstanding.

ALTER TABLE beacon_rounds
    ADD COLUMN IF NOT EXISTS closed_at TIMESTAMPTZ;

-- The owed set now excludes them. Recreated rather than added to, because a
-- partial index is defined by its predicate and the old predicate is the bug.
DROP INDEX IF EXISTS beacon_rounds_owed;
CREATE INDEX IF NOT EXISTS beacon_rounds_owed
    ON beacon_rounds (round_id)
    WHERE committed_at IS NOT NULL AND revealed_at IS NULL AND closed_at IS NULL;
