-- Append-only audit ledger: one row per Dottie Pro relay call.
--
-- pro_usage stays the fast aggregate the spend caps read on the hot path; it
-- answers "how much" but never "who, when, on which route, and why was it
-- refused". Reconstructing that from aggregates is impossible, which is exactly
-- the gap an abuse investigation hits.
--
-- Metadata only. Message content, audio bytes and TTS text are never recorded
-- here or anywhere else.
CREATE TABLE IF NOT EXISTS pro_calls (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id           INTEGER NOT NULL,
  email             TEXT,
  route             TEXT    NOT NULL,            -- chat | stt | tts | voice_mint
  model             TEXT,
  outcome           TEXT    NOT NULL,            -- ok | rejected | upstream_error
  reason            TEXT,                        -- which cap refused it, or the upstream failure
  status            INTEGER,                     -- upstream HTTP status when there was one
  prompt_tokens     INTEGER,                     -- from usage when upstream reports it
  completion_tokens INTEGER,
  tokens            INTEGER NOT NULL DEFAULT 0,  -- what we actually billed
  estimated         INTEGER NOT NULL DEFAULT 0,  -- 1 = synthetic estimate (audio/voice), not a real token count
  seconds           REAL,                        -- stt audio seconds, or voice TTL granted
  paid              INTEGER NOT NULL DEFAULT 0,
  ip                TEXT,
  created_at        INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pro_calls_user    ON pro_calls(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_pro_calls_created ON pro_calls(created_at);
CREATE INDEX IF NOT EXISTS idx_pro_calls_route   ON pro_calls(route, created_at);
