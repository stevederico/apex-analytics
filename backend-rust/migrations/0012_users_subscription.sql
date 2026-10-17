-- Stripe subscription entitlement for Dottie Pro (separate from free-credit counters).
-- Paid users skip the lifetime free-credit gate in admit(); cancel re-closes that gate.
ALTER TABLE users ADD COLUMN subscription_stripe_id TEXT;
ALTER TABLE users ADD COLUMN subscription_status TEXT;
ALTER TABLE users ADD COLUMN subscription_expires INTEGER;

-- Idempotent Stripe webhook processing (skateboard webhook_events pattern).
CREATE TABLE IF NOT EXISTS stripe_webhook_events (
  event_id TEXT PRIMARY KEY,
  event_type TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
