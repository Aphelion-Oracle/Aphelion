-- Aphelion node: local state.
--
-- This database is *per node operator* and is not consensus state — the chain
-- is the source of truth for prices. What lives here is the evidence trail:
-- every raw observation the node saw, every round it composed, and every
-- submission it signed. That matters for two reasons: a node accused of
-- reporting a bad price needs to show what its sources said at the time, and
-- the nonce sequence must survive a restart or the aggregator will reject the
-- node's next submission as a replay.

-- ---------------------------------------------------------------------------
-- Raw observations, one row per (source, feed, poll).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS raw_prices (
    id           BIGSERIAL PRIMARY KEY,
    feed_id      TEXT          NOT NULL,
    source       TEXT          NOT NULL,
    -- Fixed-point, scaled by 1e8. NUMERIC(40,0) because an i128 does not fit
    -- in BIGINT and we refuse to lose precision to a float.
    price_raw    NUMERIC(40,0) NOT NULL CHECK (price_raw > 0),
    -- When the exchange says the observation was made.
    observed_at  TIMESTAMPTZ   NOT NULL,
    -- When this node received it. The gap between the two is a useful signal
    -- that a source is lagging.
    received_at  TIMESTAMPTZ   NOT NULL DEFAULT now()
);

-- The round loop's hot query: "everything for this feed in the last N seconds".
CREATE INDEX IF NOT EXISTS raw_prices_feed_observed_idx
    ON raw_prices (feed_id, observed_at DESC);
-- Supports the per-source health and retention queries.
CREATE INDEX IF NOT EXISTS raw_prices_source_observed_idx
    ON raw_prices (source, observed_at DESC);

-- ---------------------------------------------------------------------------
-- Rounds this node composed, whether or not they were submitted.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS local_rounds (
    id              BIGSERIAL PRIMARY KEY,
    feed_id         TEXT          NOT NULL,
    nonce           BIGINT        NOT NULL,
    price_raw       NUMERIC(40,0) NOT NULL CHECK (price_raw > 0),
    confidence_bps  INTEGER       NOT NULL,
    source_count    INTEGER       NOT NULL,
    -- Spread between the highest and lowest surviving source, in bps. The
    -- single best indicator that something odd is happening upstream.
    spread_bps      INTEGER       NOT NULL,
    stddev_raw      NUMERIC(40,0) NOT NULL DEFAULT 0,
    observed_at     TIMESTAMPTZ   NOT NULL,
    signature       TEXT          NOT NULL,
    status          TEXT          NOT NULL DEFAULT 'pending',
    tx_hash         TEXT,
    error           TEXT,
    created_at      TIMESTAMPTZ   NOT NULL DEFAULT now(),
    settled_at      TIMESTAMPTZ,

    CONSTRAINT local_rounds_status_check
        CHECK (status IN ('pending', 'submitted', 'failed', 'skipped')),
    -- A nonce is consumed exactly once per feed. This constraint is what stops
    -- a crash-and-restart from producing two different prices under one nonce.
    CONSTRAINT local_rounds_feed_nonce_key UNIQUE (feed_id, nonce)
);

CREATE INDEX IF NOT EXISTS local_rounds_feed_created_idx
    ON local_rounds (feed_id, created_at DESC);
CREATE INDEX IF NOT EXISTS local_rounds_pending_idx
    ON local_rounds (status) WHERE status = 'pending';

-- ---------------------------------------------------------------------------
-- Monotonic nonce allocator, one row per feed.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS feed_nonces (
    feed_id    TEXT   PRIMARY KEY,
    next_nonce BIGINT NOT NULL DEFAULT 1 CHECK (next_nonce > 0)
);

-- ---------------------------------------------------------------------------
-- Per-source health, so /health can answer "which exchange is down?".
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS source_health (
    source               TEXT NOT NULL,
    feed_id              TEXT NOT NULL,
    last_success_at      TIMESTAMPTZ,
    last_error           TEXT,
    last_error_at        TIMESTAMPTZ,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    total_successes      BIGINT  NOT NULL DEFAULT 0,
    total_failures       BIGINT  NOT NULL DEFAULT 0,

    PRIMARY KEY (source, feed_id)
);

-- ---------------------------------------------------------------------------
-- Last-known on-chain view of this node, refreshed each round. Cached so the
-- API can answer without an RPC round trip, and so a node can still report its
-- reputation while its RPC endpoint is unreachable.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS node_snapshot (
    id            SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    public_key    TEXT,
    stake_raw     NUMERIC(40,0),
    reputation    INTEGER,
    status        TEXT,
    ledger_time   TIMESTAMPTZ,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
