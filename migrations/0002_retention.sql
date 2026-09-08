-- Retention helper.
--
-- Kept as a function rather than a cron job so that the node process owns its
-- own housekeeping: an operator running the container has nothing else to set
-- up, and the deletion is chunked so it never takes a long lock on the table
-- that the round loop reads every minute.

CREATE OR REPLACE FUNCTION prune_raw_prices(older_than INTERVAL, chunk_size INT DEFAULT 10000)
RETURNS BIGINT AS $$
DECLARE
    removed BIGINT := 0;
    batch   BIGINT := 0;
BEGIN
    LOOP
        DELETE FROM raw_prices
        WHERE id IN (
            SELECT id FROM raw_prices
            WHERE observed_at < now() - older_than
            ORDER BY id
            LIMIT chunk_size
        );
        GET DIAGNOSTICS batch = ROW_COUNT;
        removed := removed + batch;
        EXIT WHEN batch = 0;
    END LOOP;
    RETURN removed;
END;
$$ LANGUAGE plpgsql;
