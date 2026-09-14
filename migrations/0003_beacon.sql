-- Beacon secrets.
--
-- The one table in this schema where losing a row costs the operator stake.
--
-- A commit-reveal round is two transactions separated by a window measured in
-- minutes. Between them the node holds the only copy of a secret that nothing
-- on chain can reconstruct: the commitment is a hash, and the contract will
-- charge a no-show penalty to anybody who does not open theirs. So the secret
-- is written here and the write is committed *before* the commitment is
-- submitted -- never after, and never held only in memory. A node that
-- crashes between the two must come back able to reveal.
--
-- That ordering is the whole reason this is a table rather than a field on an
-- in-memory struct. It costs one round trip per beacon round, which is a price
-- worth paying exactly once per round for the ability to restart.

CREATE TABLE IF NOT EXISTS beacon_rounds (
    round_id     BIGINT PRIMARY KEY CHECK (round_id > 0),

    -- 32 bytes, hex. Written before the commitment is submitted.
    secret_hex   TEXT   NOT NULL CHECK (secret_hex ~ '^[0-9a-f]{64}$'),
    -- The commitment derived from it, kept so a node can prove to itself what
    -- it committed to without recomputing the preimage.
    commitment   TEXT   NOT NULL CHECK (commitment ~ '^[0-9a-f]{64}$'),

    -- Set when the secret is stored, before anything is sent. A row with a
    -- null committed_at is a secret the node generated and then failed to
    -- submit: harmless, and distinguishable from one it owes a reveal on.
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    committed_at TIMESTAMPTZ,
    revealed_at  TIMESTAMPTZ
);

-- The lookup the reveal path makes every tick: rounds this node committed to
-- and has not yet opened. Partial, because that set is small and shrinking
-- while the table only grows.
CREATE INDEX IF NOT EXISTS beacon_rounds_owed
    ON beacon_rounds (round_id)
    WHERE committed_at IS NOT NULL AND revealed_at IS NULL;
