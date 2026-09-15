//! All SQL lives here, so that "what does this node persist?" has one answer.

use aphelion_core::{FeedId, Price};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::PgPool;

use super::models::*;
use crate::error::{NodeError, Result};

#[derive(Clone)]
pub struct Repo {
    pool: PgPool,
}

impl Repo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -- observations -------------------------------------------------------

    /// Record one observation and mark the source healthy, in a single
    /// transaction: health that disagrees with the data it describes is worse
    /// than no health tracking at all.
    pub async fn record_observation(
        &self,
        feed: &FeedId,
        source: &str,
        price: Price,
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO raw_prices (feed_id, source, price_raw, observed_at)
             VALUES ($1, $2, $3::numeric, $4)",
        )
        .bind(feed.as_str())
        .bind(source)
        .bind(price.raw().to_string())
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO source_health (source, feed_id, last_success_at, consecutive_failures, total_successes)
             VALUES ($1, $2, now(), 0, 1)
             ON CONFLICT (source, feed_id) DO UPDATE SET
                 last_success_at      = now(),
                 consecutive_failures = 0,
                 total_successes      = source_health.total_successes + 1",
        )
        .bind(source)
        .bind(feed.as_str())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn record_source_failure(
        &self,
        feed: &FeedId,
        source: &str,
        detail: &str,
    ) -> Result<()> {
        // Truncate: an HTML error page from a proxy should not become a 40 KB
        // row repeated every poll.
        let detail: String = detail.chars().take(500).collect();
        sqlx::query(
            "INSERT INTO source_health (source, feed_id, last_error, last_error_at, consecutive_failures, total_failures)
             VALUES ($1, $2, $3, now(), 1, 1)
             ON CONFLICT (source, feed_id) DO UPDATE SET
                 last_error           = EXCLUDED.last_error,
                 last_error_at        = now(),
                 consecutive_failures = source_health.consecutive_failures + 1,
                 total_failures       = source_health.total_failures + 1",
        )
        .bind(source)
        .bind(feed.as_str())
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The freshest observation from each source for a feed, ignoring anything
    /// older than `max_age`.
    ///
    /// `DISTINCT ON` collapses to one row per source, so a chatty exchange
    /// polled every second cannot outvote a slower one inside a single round.
    pub async fn latest_per_source(
        &self,
        feed: &FeedId,
        max_age: std::time::Duration,
    ) -> Result<Vec<Observation>> {
        let cutoff = Utc::now()
            - ChronoDuration::from_std(max_age)
                .map_err(|e| NodeError::Other(anyhow::anyhow!("max_age out of range: {e}")))?;

        let rows = sqlx::query_as::<_, RawPriceRow>(
            "SELECT DISTINCT ON (source)
                    id, feed_id, source, price_raw::text AS price_text, observed_at, received_at
             FROM raw_prices
             WHERE feed_id = $1 AND observed_at >= $2
             ORDER BY source, observed_at DESC",
        )
        .bind(feed.as_str())
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(RawPriceRow::decode).collect()
    }

    pub async fn recent_observations(&self, feed: &FeedId, limit: i64) -> Result<Vec<Observation>> {
        let rows = sqlx::query_as::<_, RawPriceRow>(
            "SELECT id, feed_id, source, price_raw::text AS price_text, observed_at, received_at
             FROM raw_prices
             WHERE feed_id = $1
             ORDER BY observed_at DESC
             LIMIT $2",
        )
        .bind(feed.as_str())
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(RawPriceRow::decode).collect()
    }

    // -- nonces -------------------------------------------------------------

    /// Allocate the next nonce for a feed.
    ///
    /// The aggregator requires nonces to strictly increase per (node, feed),
    /// so this must be atomic and must survive a restart. `INSERT ... ON
    /// CONFLICT DO UPDATE ... RETURNING` does both in one statement: two
    /// concurrent round loops cannot be handed the same number.
    pub async fn next_nonce(&self, feed: &FeedId) -> Result<u64> {
        let (nonce,): (i64,) = sqlx::query_as(
            "INSERT INTO feed_nonces (feed_id, next_nonce) VALUES ($1, 2)
             ON CONFLICT (feed_id) DO UPDATE SET next_nonce = feed_nonces.next_nonce + 1
             RETURNING next_nonce - 1",
        )
        .bind(feed.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(nonce as u64)
    }

    /// Fast-forward the local counter past a nonce the chain has already seen.
    ///
    /// Needed when a node is restored from a backup that predates its last
    /// submission: without this it would reissue nonces the aggregator rejects.
    pub async fn bump_nonce_floor(&self, feed: &FeedId, seen_on_chain: u64) -> Result<()> {
        sqlx::query(
            "INSERT INTO feed_nonces (feed_id, next_nonce) VALUES ($1, $2)
             ON CONFLICT (feed_id) DO UPDATE
                SET next_nonce = GREATEST(feed_nonces.next_nonce, EXCLUDED.next_nonce)",
        )
        .bind(feed.as_str())
        .bind(seen_on_chain as i64 + 1)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- beacon ---------------------------------------------------------------

    /// Store a secret for a beacon round, before anything is submitted.
    ///
    /// The ordering is the point, and it is why this is a separate call from
    /// [`Self::mark_committed`] rather than one write afterwards. Between the
    /// commitment landing and the reveal, this row is the only copy of a
    /// secret nothing on chain can reconstruct, and a node that cannot reveal
    /// is slashed. So the row is committed first and the transaction is sent
    /// second; a crash in between leaves a secret with no commitment, which
    /// costs nothing.
    ///
    /// Idempotent on `round_id`, so a retry after a failed submission reuses
    /// the secret it already stored rather than generating a second one the
    /// first commitment would not open.
    pub async fn store_beacon_secret(
        &self,
        round_id: u64,
        secret_hex: &str,
        commitment: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO beacon_rounds (round_id, secret_hex, commitment)
             VALUES ($1, $2, $3)
             ON CONFLICT (round_id) DO NOTHING",
        )
        .bind(round_id as i64)
        .bind(secret_hex)
        .bind(commitment)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// What this node stored for a round, if anything.
    pub async fn beacon_round(&self, round_id: u64) -> Result<Option<BeaconRow>> {
        let row = sqlx::query_as::<_, BeaconRow>(
            "SELECT round_id, secret_hex, commitment, committed_at, revealed_at
             FROM beacon_rounds WHERE round_id = $1",
        )
        .bind(round_id as i64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record that the commitment reached the ledger. From here the reveal is
    /// owed.
    pub async fn mark_committed(&self, round_id: u64) -> Result<()> {
        sqlx::query("UPDATE beacon_rounds SET committed_at = now() WHERE round_id = $1")
            .bind(round_id as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_revealed(&self, round_id: u64) -> Result<()> {
        sqlx::query("UPDATE beacon_rounds SET revealed_at = now() WHERE round_id = $1")
            .bind(round_id as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Rounds this node committed to and has not opened, oldest first.
    ///
    /// Oldest first because its deadline is the nearest, and the node can only
    /// send one reveal per tick.
    pub async fn beacon_reveals_owed(&self) -> Result<Vec<BeaconRow>> {
        let rows = sqlx::query_as::<_, BeaconRow>(
            "SELECT round_id, secret_hex, commitment, committed_at, revealed_at
             FROM beacon_rounds
             WHERE committed_at IS NOT NULL AND revealed_at IS NULL
             ORDER BY round_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // -- rounds -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_round(
        &self,
        feed: &FeedId,
        nonce: u64,
        price: Price,
        confidence_bps: u32,
        source_count: u32,
        spread_bps: u32,
        stddev: Price,
        observed_at: DateTime<Utc>,
        signature_hex: &str,
        status: RoundStatus,
    ) -> Result<i64> {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO local_rounds
                (feed_id, nonce, price_raw, confidence_bps, source_count, spread_bps,
                 stddev_raw, observed_at, signature, status)
             VALUES ($1, $2, $3::numeric, $4, $5, $6, $7::numeric, $8, $9, $10)
             RETURNING id",
        )
        .bind(feed.as_str())
        .bind(nonce as i64)
        .bind(price.raw().to_string())
        .bind(confidence_bps as i32)
        .bind(source_count as i32)
        .bind(spread_bps as i32)
        .bind(stddev.raw().to_string())
        .bind(observed_at)
        .bind(signature_hex)
        .bind(status.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn settle_round(
        &self,
        id: i64,
        status: RoundStatus,
        tx_hash: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let error: Option<String> = error.map(|e| e.chars().take(1000).collect());
        sqlx::query(
            "UPDATE local_rounds
                SET status = $2, tx_hash = $3, error = $4, settled_at = now()
              WHERE id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(tx_hash)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The most recent round that actually landed on chain. Drives the
    /// "has the price moved enough to be worth a fee?" decision.
    pub async fn last_submitted_round(&self, feed: &FeedId) -> Result<Option<LocalRound>> {
        let row = sqlx::query_as::<_, LocalRoundRow>(
            "SELECT id, feed_id, nonce, price_raw::text AS price_text, confidence_bps,
                    source_count, spread_bps, observed_at, status, tx_hash, error, created_at
             FROM local_rounds
             WHERE feed_id = $1 AND status = 'submitted'
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(feed.as_str())
        .fetch_optional(&self.pool)
        .await?;

        row.map(LocalRoundRow::decode).transpose()
    }

    pub async fn recent_rounds(
        &self,
        feed: Option<&FeedId>,
        limit: i64,
    ) -> Result<Vec<LocalRound>> {
        let rows = sqlx::query_as::<_, LocalRoundRow>(
            "SELECT id, feed_id, nonce, price_raw::text AS price_text, confidence_bps,
                    source_count, spread_bps, observed_at, status, tx_hash, error, created_at
             FROM local_rounds
             WHERE ($1::text IS NULL OR feed_id = $1)
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(feed.map(|f| f.as_str()))
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(LocalRoundRow::decode).collect()
    }

    // -- health & snapshot --------------------------------------------------

    pub async fn source_health(&self) -> Result<Vec<SourceHealthRow>> {
        Ok(sqlx::query_as::<_, SourceHealthRow>(
            "SELECT source, feed_id, last_success_at, last_error, last_error_at,
                    consecutive_failures, total_successes, total_failures
             FROM source_health
             ORDER BY source, feed_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn save_node_snapshot(
        &self,
        public_key: &str,
        stake: i128,
        reputation: u32,
        status: &str,
        ledger_time: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO node_snapshot (id, public_key, stake_raw, reputation, status, ledger_time, updated_at)
             VALUES (1, $1, $2::numeric, $3, $4, $5, now())
             ON CONFLICT (id) DO UPDATE SET
                 public_key = EXCLUDED.public_key,
                 stake_raw  = EXCLUDED.stake_raw,
                 reputation = EXCLUDED.reputation,
                 status     = EXCLUDED.status,
                 ledger_time= EXCLUDED.ledger_time,
                 updated_at = now()",
        )
        .bind(public_key)
        .bind(stake.to_string())
        .bind(reputation as i32)
        .bind(status)
        .bind(ledger_time)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- replay -------------------------------------------------------------

    /// One round by the pair a dispute names it by.
    ///
    /// `(feed, nonce)` rather than the row id because that is the identifier
    /// the aggregator knows and therefore the one that appears in a challenge;
    /// the row id is local bookkeeping nobody else can see. The unique
    /// constraint on the pair is what makes this a single row.
    pub async fn round_by_nonce(&self, feed: &FeedId, nonce: u64) -> Result<Option<RecordedRound>> {
        let row = sqlx::query_as::<_, RecordedRoundRow>(
            "SELECT id, feed_id, nonce, price_raw::text AS price_text, confidence_bps,
                    source_count, spread_bps, stddev_raw::text AS stddev_text, observed_at,
                    signature, status, tx_hash, error, created_at
             FROM local_rounds
             WHERE feed_id = $1 AND nonce = $2",
        )
        .bind(feed.as_str())
        .bind(nonce as i64)
        .fetch_optional(&self.pool)
        .await?;

        row.map(RecordedRoundRow::decode).transpose()
    }

    /// Every observation dated inside a past round's age window.
    ///
    /// Candidates, not the answer. Which of these the round could actually
    /// *see* — and which one per source it would have picked — is
    /// [`crate::engine::replay::visible_at`], deliberately not this query.
    ///
    /// [`Self::latest_per_source`] does the equivalent narrowing in SQL because
    /// it runs every round for every feed and wants the index. This runs once,
    /// by hand, over one round's window, and the rule it applies is the part of
    /// the replay most likely to be got subtly wrong: a row the node had not
    /// yet received was not there to be selected, and admitting one invents a
    /// discrepancy for an honest node to explain. A rule that decides whether a
    /// dispute is answerable belongs where a test can reach it, not in a `WHERE`
    /// clause that no test in this repository can execute. So the SQL keeps only
    /// the bound the index serves, and the judgement moves to a pure function.
    ///
    /// The two bounds here are the age filter, which the index serves, and
    /// `received_at`, which keeps a long-lived node from dragging back every
    /// observation recorded since the round. `visible_at` applies the
    /// `received_at` rule again over what comes back. The duplication is
    /// deliberate and it is not symmetrical: this predicate can only ever be
    /// looser than the function's, so the worst a drift between them can do is
    /// transfer rows that are then discarded — never admit one the round could
    /// not see.
    ///
    /// Note what is *not* bounded: `observed_at` above. A venue with a fast
    /// clock can date an observation slightly in the future, and the live round
    /// query has no upper bound either, so it genuinely did see those.
    ///
    /// A window that comes back short is not evidence of anything: raw
    /// observations are pruned on a retention interval, and a round older than
    /// that has no inputs left to replay.
    pub async fn observations_in_window(
        &self,
        feed: &FeedId,
        max_age: std::time::Duration,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<Observation>> {
        let cutoff = as_of
            - ChronoDuration::from_std(max_age)
                .map_err(|e| NodeError::Other(anyhow::anyhow!("max_age out of range: {e}")))?;

        let rows = sqlx::query_as::<_, RawPriceRow>(
            "SELECT id, feed_id, source, price_raw::text AS price_text, observed_at, received_at
             FROM raw_prices
             WHERE feed_id = $1 AND observed_at >= $2 AND received_at <= $3
             ORDER BY source, observed_at DESC",
        )
        .bind(feed.as_str())
        .bind(cutoff)
        .bind(as_of)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(RawPriceRow::decode).collect()
    }

    /// Delete observations older than `retention`. Returns the row count.
    pub async fn prune(&self, retention: std::time::Duration) -> Result<i64> {
        let interval = format!("{} seconds", retention.as_secs());
        let (removed,): (i64,) = sqlx::query_as("SELECT prune_raw_prices($1::interval)")
            .bind(interval)
            .fetch_one(&self.pool)
            .await?;
        Ok(removed)
    }
}
