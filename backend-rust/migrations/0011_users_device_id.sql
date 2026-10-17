-- Per-device identity for the Pro free credit: a SHA-256 of the Mac's
-- IOPlatformUUID, sent by the desktop at registration. Re-registering with a
-- fresh email on the same machine shares the same device_id, so the ~$1 free
-- credit is per device, not per account. Nullable: old clients never send it.
ALTER TABLE users ADD COLUMN device_id TEXT;
CREATE INDEX IF NOT EXISTS idx_users_device_id ON users(device_id);
