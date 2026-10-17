-- Backfill real-dollar cost for usage recorded before 0.167.0.
--
-- Rows written before cost metering carry tokens but no cost, so the dollar
-- dashboards read ~$0 for a fleet that really did spend money. There is no way
-- to recover the true figure — xAI reported it per call and we did not store it
-- — so this applies the blended rate measured from live traffic.
--
-- Two production calls, both grok-4.6 with a warm prompt cache:
--   712 tokens -> $0.000952   (13,371 ticks/token)
--   757 tokens -> $0.001014   (13,394 ticks/token)
-- Consistent at ~13,400 ticks per token ($1.34 per 1M tokens), well under the
-- $6/M worst-case output rate the old token caps assumed, because most tokens
-- are cached input.
--
-- Only touches rows that have no cost at all, so re-running changes nothing and
-- real per-call figures recorded from 0.167.0 onward are never overwritten.
UPDATE pro_usage
   SET cost_ticks = tokens * 13400
 WHERE cost_ticks = 0
   AND tokens > 0;
