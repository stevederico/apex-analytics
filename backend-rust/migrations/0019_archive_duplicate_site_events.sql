-- Move exact duplicate site_events into archived_duplicates.
-- Keeps the lowest id of each identical row. A row that differs in any
-- stored field stays in site_events. Runs once, on boot, inside a transaction.
-- Partition uses columns that exist after migration 0018. ua/bot are added
-- later by ensure_schema, so they are not part of this key.
CREATE TABLE archived_duplicates AS
SELECT e.*, CAST(strftime('%s','now') AS INTEGER) * 1000 AS archived_at
FROM site_events e
WHERE e.id IN (
  SELECT id FROM (
    SELECT id, ROW_NUMBER() OVER (
      PARTITION BY
        site_id, event_type, created_at,
        ifnull(name,''), ifnull(url,''), ifnull(referrer,''),
        ifnull(distinct_id,''), ifnull(session_id,''),
        ifnull(hostname,''), ifnull(browser,''), ifnull(os,''),
        ifnull(device,''), ifnull(country,''),
        ifnull(utm_source,''), ifnull(utm_medium,''), ifnull(utm_campaign,''),
        ifnull(props,''), ifnull(source_event_id,'')
      ORDER BY id
    ) AS rn
    FROM site_events
  )
  WHERE rn > 1
);

DELETE FROM site_events WHERE id IN (SELECT id FROM archived_duplicates);

CREATE INDEX idx_archived_duplicates_site ON archived_duplicates(site_id, created_at);
