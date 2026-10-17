//! HS256 tokens for the Dottie Pro relay.
//!
//! The bearer a desktop install holds used to be 32 random bytes, meaningful
//! only as a row key in this service's `users` table. That made Pro — now its
//! own service — unable to identify a caller without asking us on every
//! request. A signed token carries the identity instead, so Pro verifies it
//! offline and never depends on analytics being up.
//!
//! The token IS still `users.token`: it is stored and looked up exactly as
//! before, so every guard here keeps working unchanged. Only its contents
//! changed.
//!
//! MIGRATION IS AUTOMATIC. Pro rejects a non-JWT bearer with 404, and the
//! shipped desktop reads a 404 on a guarded endpoint as "bad token" and
//! re-registers (`RegistrationManager.bootstrapTokenIfNeeded(force: true)`),
//! which lands here and mints a JWT. No app release, no forced upgrade.
//!
//! EXPIRY works the same way: an expired token 404s, the client re-registers,
//! and gets a fresh one. That is what makes a short-ish lifetime safe, and it
//! is the only revocation this scheme has — a token cannot be recalled before
//! it expires, so the window is the blast radius.
//!
//! No JWT crate: HS256 is an HMAC and a base64url encode, done with the
//! `hmac`/`sha2` crates directly.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How long a minted token stays valid. Short enough that a leaked token ages
/// out on its own, long enough that a machine left asleep for a month still
/// self-heals on its next call rather than stranding the user.
pub const TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;

/// Claims Pro needs to run the relay: who, their email (Stripe binding), and
/// the device the free-credit cohort keys on.
#[derive(Debug, Clone, PartialEq)]
pub struct Claims {
    pub user_id: i64,
    pub email: String,
    pub device_id: Option<String>,
    /// Unix seconds.
    pub exp: i64,
}

/// base64url, no padding — the JWT alphabet.
fn b64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        // A 1-byte tail encodes to 2 chars, a 2-byte tail to 3 — padding is omitted.
        for (i, &d) in idx.iter().enumerate().take(chunk.len() + 1) {
            let _ = i;
            out.push(ALPHABET[d as usize] as char);
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn sign(secret: &str, signing_input: &str) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signing_input.as_bytes());
    Some(b64_encode(&mac.finalize().into_bytes()))
}

/// Mint a token for `claims`. `None` only when the secret is unusable.
pub fn mint(secret: &str, user_id: i64, email: &str, device_id: Option<&str>, now: i64) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    let header = b64_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "sub": user_id,
        "email": email,
        "device_id": device_id,
        "iat": now,
        "exp": now + TOKEN_TTL_SECS,
    });
    let payload = b64_encode(payload.to_string().as_bytes());
    let signing_input = format!("{header}.{payload}");
    let signature = sign(secret, &signing_input)?;
    Some(format!("{signing_input}.{signature}"))
}

/// Verify and decode. `Err` carries the reason for logs, never for the caller —
/// an invalid token is always answered the same way.
pub fn verify(secret: &str, token: &str, now: i64) -> Result<Claims, String> {
    if secret.is_empty() {
        return Err("no signing secret configured".into());
    }
    let mut parts = token.split('.');
    let (header, payload, signature) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err("not a three-part token".into()),
    };
    let expected = sign(secret, &format!("{header}.{payload}")).ok_or("bad secret")?;
    // Constant-time: a signature check that short-circuits leaks which prefix
    // was right, which is enough to forge one byte at a time.
    if !crate::middleware::safe_equal(expected.as_bytes(), signature.as_bytes()) {
        return Err("signature mismatch".into());
    }
    let decoded = b64_decode(payload).ok_or("payload not base64url")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|_| "payload not json".to_string())?;
    let exp = claims.get("exp").and_then(serde_json::Value::as_i64).ok_or("no exp")?;
    if exp <= now {
        return Err("expired".into());
    }
    Ok(Claims {
        user_id: claims.get("sub").and_then(serde_json::Value::as_i64).ok_or("no sub")?,
        email: claims
            .get("email")
            .and_then(serde_json::Value::as_str)
            .ok_or("no email")?
            .to_string(),
        device_id: claims
            .get("device_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        exp,
    })
}

/// Cheap shape test: three dot-separated parts. Used to tell a legacy opaque
/// token (64 hex chars, no dots) from a minted one without verifying.
pub fn looks_like_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-signing-secret";
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn round_trips_every_claim() {
        let t = mint(SECRET, 42, "a@b.c", Some("dev-1"), NOW).unwrap();
        let c = verify(SECRET, &t, NOW).unwrap();
        assert_eq!(c.user_id, 42);
        assert_eq!(c.email, "a@b.c");
        assert_eq!(c.device_id.as_deref(), Some("dev-1"));
        assert_eq!(c.exp, NOW + TOKEN_TTL_SECS);
    }

    #[test]
    fn absent_device_id_stays_absent() {
        let t = mint(SECRET, 1, "a@b.c", None, NOW).unwrap();
        assert_eq!(verify(SECRET, &t, NOW).unwrap().device_id, None);
    }

    #[test]
    fn a_different_secret_cannot_verify() {
        let t = mint(SECRET, 1, "a@b.c", None, NOW).unwrap();
        assert!(verify("other-secret", &t, NOW).is_err(), "forgery must fail");
    }

    #[test]
    fn tampering_with_the_payload_fails() {
        let t = mint(SECRET, 1, "a@b.c", None, NOW).unwrap();
        let mut parts: Vec<&str> = t.split('.').collect();
        // Re-encode a payload claiming a different user, keeping the signature.
        let forged = b64_encode(
            serde_json::json!({ "sub": 999, "email": "a@b.c", "exp": NOW + 100 })
                .to_string()
                .as_bytes(),
        );
        parts[1] = &forged;
        assert!(verify(SECRET, &parts.join("."), NOW).is_err(), "claims are signed");
    }

    #[test]
    fn expiry_is_enforced() {
        let t = mint(SECRET, 1, "a@b.c", None, NOW).unwrap();
        assert!(verify(SECRET, &t, NOW + TOKEN_TTL_SECS - 1).is_ok());
        assert!(
            verify(SECRET, &t, NOW + TOKEN_TTL_SECS).is_err(),
            "exactly at exp is expired — the client re-registers and self-heals"
        );
    }

    #[test]
    fn legacy_tokens_are_distinguishable_and_rejected() {
        let legacy = "a".repeat(64);
        assert!(!looks_like_jwt(&legacy));
        assert!(verify(SECRET, &legacy, NOW).is_err());
        assert!(looks_like_jwt(&mint(SECRET, 1, "a@b.c", None, NOW).unwrap()));
    }

    #[test]
    fn base64url_round_trips_every_tail_length() {
        for n in 0..8usize {
            let raw: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&raw)).unwrap(), raw, "len {n}");
        }
        // The JWT alphabet never emits + / or =.
        let encoded = b64_encode(&(0u8..=255).collect::<Vec<u8>>());
        assert!(!encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='));
    }

    #[test]
    fn no_secret_mints_nothing_and_verifies_nothing() {
        assert_eq!(mint("", 1, "a@b.c", None, NOW), None);
        assert!(verify("", "a.b.c", NOW).is_err());
    }

    #[test]
    fn unsigned_alg_none_is_rejected() {
        // Classic JWT bypass: declare alg=none and skip the signature.
        // We HMAC the header.payload regardless of what the header claims.
        let header = b64_encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64_encode(br#"{"sub":1,"email":"a@b.c","exp":9999999999}"#);
        assert!(verify(SECRET, &format!("{header}.{payload}."), NOW).is_err());
        assert!(verify(SECRET, &format!("{header}.{payload}.x"), NOW).is_err());
    }
}
