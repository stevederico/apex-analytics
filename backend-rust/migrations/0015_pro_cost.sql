-- Real spend, in xAI "ticks" (1 tick = 1e-10 USD, so 10^10 ticks = $1).
--
-- xAI returns `usage.cost_in_usd_ticks` on chat completions — the actual amount
-- billed, after prompt-cache discounts and with the correct input/output split.
-- The token counters priced everything at the worst-case output rate and so ran
-- roughly 9x high (verified: a 757-token call billed $0.001014, which the token
-- meter valued near $0.009).
--
-- Tokens stay for the audio routes, where xAI bills on a separate meter and
-- reports no cost at all; those rows carry an ESTIMATED tick figure instead.
ALTER TABLE pro_usage ADD COLUMN cost_ticks INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pro_calls ADD COLUMN cost_ticks INTEGER NOT NULL DEFAULT 0;
-- 1 = cost_ticks is our own estimate (audio/voice), not a figure xAI reported.
ALTER TABLE pro_calls ADD COLUMN cost_estimated INTEGER NOT NULL DEFAULT 0;
