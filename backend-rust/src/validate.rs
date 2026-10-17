//! Registration input validation — a faithful port of the validation helpers in
//! backend/server.js (`EMAIL_RE`, `TLD_TYPO_BLOCKLIST`, `KEYBOARD_MASH_RE`,
//! `isLowEntropyLocal`, `nameRejectionReason`, `emailRejectionReason`) plus the
//! query/string helpers (`clampStr`, `parseIntQuery`).
//!
//! NO regex crate — every JS regex is reimplemented as a hand-rolled string
//! scan. The rejection-reason strings returned here are written verbatim to the
//! `rejected_registrations.reason` column, so they MUST match server.js exactly
//! (they're queried by name in the test suite and in audits).
//!
//! THREADING: pure functions, no shared state — trivially `Sync`. The
//! `/api/register` route (A4) calls these before touching `Db`.

#![allow(dead_code)]

/// Known typo-TLDs that are almost always a fat-fingered `.com`/`.net`/`.org`.
/// Copied verbatim from server.js `TLD_TYPO_BLOCKLIST`.
const TLD_TYPO_BLOCKLIST: &[&str] = &[
    "cm", "om", "con", "cmo", "ocm", "cim", "vom", "comm", "coom", "xom", "dom",
    "nett", "nte", "ent", "orgg", "rog", "og",
];

/// Keyboard-mash substrings — the exact alternation from server.js
/// `KEYBOARD_MASH_RE`, matched case-insensitively anywhere in the token.
const KEYBOARD_MASH_TOKENS: &[&str] = &[
    "asdf", "fdsa", "sdfg", "qwer", "wert", "rewq", "tyui", "zxcv", "cxzv",
    "hjkl", "lkjh", "jklh", "nbvc", "poiu", "uytr", "qwerty", "asdfg",
    "asdfasdf", "qweqwe", "qweasd", "aaaaa", "bbbbb",
];

/// Case-insensitive substring scan over the keyboard-mash token list. Equivalent
/// to `KEYBOARD_MASH_RE.test(s)` in server.js. (The longer tokens are subsumed by
/// shorter ones, e.g. "asdfasdf" contains "asdf"; kept for a 1:1 port.)
fn is_keyboard_mash(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    KEYBOARD_MASH_TOKENS.iter().any(|t| lower.contains(t))
}

/// True if `s` contains a run of the SAME char repeated `min_run` or more times
/// total — i.e. `(min_run - 1)` consecutive repeats after the first. Equivalent
/// to the JS `(.)\1{n,}` backreference: `(.)\1{3,}` → a run of length ≥ 4 →
/// `has_repeat_run(s, 4)`; `(.)\1{4,}` → run ≥ 5 → `has_repeat_run(s, 5)`.
/// Operates on Unicode scalar values, matching JS `.` (per code unit; close
/// enough for these ASCII-dominated inputs and never panics).
fn has_repeat_run(s: &str, min_run: usize) -> bool {
    let mut prev: Option<char> = None;
    let mut run = 0usize;
    for c in s.chars() {
        if Some(c) == prev {
            run += 1;
        } else {
            prev = Some(c);
            run = 1;
        }
        if run >= min_run {
            return true;
        }
    }
    false
}

/// Hand-rolled `EMAIL_RE` = `^[^\s@]+@[^\s@]+\.[^\s@]+$`: at least one char
/// before `@`, at least one between `@` and the LAST `.`, at least one after the
/// last `.`, and NO whitespace or `@` in any of the three segments. JS `\s`
/// covers space/tab/CR/LF/FF/VT (plus a few Unicode spaces); we treat ASCII
/// whitespace, which is what real submissions ever carry.
fn email_re_ok(email: &str) -> bool {
    // Exactly one '@' is implied by "[^@]+@[^@]+" — but the regex anchors the
    // domain on the FIRST '@' (the local part is `[^\s@]+`, greedy but can't
    // span an '@'), so split on the first '@'.
    let at = match email.find('@') {
        Some(i) => i,
        None => return false,
    };
    let local = &email[..at];
    let domain = &email[at + 1..];
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    // No '@' or whitespace allowed in either segment.
    if !no_ws_at(local) || !no_ws_at(domain) {
        return false;
    }
    // Domain must contain a '.' with a non-empty label after it: `[^\s@]+\.[^\s@]+`.
    let dot = match domain.rfind('.') {
        Some(i) => i,
        None => return false,
    };
    let host = &domain[..dot];
    let tld = &domain[dot + 1..];
    !host.is_empty() && !tld.is_empty()
}

/// `[^\s@]+` predicate: no ASCII whitespace and no `@`.
fn no_ws_at(s: &str) -> bool {
    s.chars().all(|c| c != '@' && !c.is_ascii_whitespace())
}

/// Port of server.js `isLowEntropyLocal`: ≥4 of the same char in a row
/// (`(.)\1{3,}` → run ≥ 4), OR length ≥ 5 with zero vowels/digits (the JS
/// charclass is `[aeiouy0-9]`, case-insensitive).
pub fn is_low_entropy_local(local: &str) -> bool {
    if has_repeat_run(local, 4) {
        return true;
    }
    if local.chars().count() >= 5 && !local.chars().any(is_vowel_or_digit) {
        return true;
    }
    false
}

fn is_vowel_or_digit(c: char) -> bool {
    matches!(
        c.to_ascii_lowercase(),
        'a' | 'e' | 'i' | 'o' | 'u' | 'y' | '0'..='9'
    )
}

/// Port of server.js `emailRejectionReason`: the specific reason or `None` if
/// the email is plausibly real. `None` argument models a non-string JSON value
/// (`typeof email !== "string"` → "email_not_string").
pub fn email_rejection_reason(email: Option<&str>) -> Option<String> {
    let email = match email {
        Some(e) => e,
        None => return Some("email_not_string".into()),
    };
    // JS `.length` is UTF-16 code units; for the ASCII-dominated emails here,
    // char count matches in practice and never over-rejects legit addresses.
    let len = email.chars().count();
    if len == 0 {
        return Some("email_empty".into());
    }
    if len > 320 {
        return Some("email_too_long".into());
    }
    if !email_re_ok(email) {
        return Some("email_bad_format".into());
    }
    let lower = email.to_lowercase();
    let at = lower.rfind('@').unwrap_or(0);
    let local = &lower[..at];
    let domain = &lower[at + 1..];
    // `domain.split(".").pop()` → the substring after the last '.' (the regex
    // guaranteed a '.' is present).
    let tld = domain.rsplit('.').next().unwrap_or("");
    if TLD_TYPO_BLOCKLIST.contains(&tld) {
        return Some(format!("email_typo_tld_{tld}"));
    }
    if is_keyboard_mash(local) {
        return Some("email_keyboard_mash_local".into());
    }
    // `domain.split(".")[0]` → the first label of the domain.
    let domain_first = domain.split('.').next().unwrap_or("");
    if is_keyboard_mash(domain_first) {
        return Some("email_keyboard_mash_domain".into());
    }
    if is_low_entropy_local(local) {
        return Some("email_low_entropy_local".into());
    }
    None
}

/// Port of server.js `nameRejectionReason`. `None` models a non-string value.
pub fn name_rejection_reason(name: Option<&str>) -> Option<String> {
    let name = match name {
        Some(n) => n,
        None => return Some("name_not_string".into()),
    };
    let trimmed = name.trim();
    let len = trimmed.chars().count();
    if len == 0 {
        return Some("name_empty".into());
    }
    if len > 200 {
        return Some("name_too_long".into());
    }
    if len < 2 {
        return Some("name_too_short".into());
    }
    if is_keyboard_mash(trimmed) {
        return Some("name_keyboard_mash".into());
    }
    if has_repeat_run(trimmed, 5) {
        return Some("name_repeating_chars".into());
    }
    None
}

/// Port of server.js `clampStr(v, n)`: a non-empty string clamped to `n` chars,
/// else `None`. `None` input (non-string JSON) → `None`.
pub fn clamp_str(v: Option<&str>, n: usize) -> Option<String> {
    match v {
        Some(s) if !s.is_empty() => Some(s.chars().take(n).collect()),
        _ => None,
    }
}

/// Port of JS `Number(s)` for query-param coercion — the operations the dottie
/// query params actually exercise. JS `Number()` is more permissive than Rust's
/// `f64::FromStr`, so this is a deliberate (close) reimplementation rather than a
/// bare `parse`:
///   - leading/trailing ASCII whitespace is trimmed (JS trims its own whitespace
///     set; ASCII whitespace covers every real query value);
///   - the empty string (after trim) is `0.0` (`Number("")` === 0);
///   - decimal + exponent forms parse via `f64` (`"48.5"`, `"1e3"`, `"12"`, `"0"`);
///   - radix-prefixed integer literals `0x`/`0X`, `0o`/`0O`, `0b`/`0B` parse via
///     `from_str_radix` then widen to `f64` (`Number("0x10")` === 16);
///   - `"Infinity"` / `"+Infinity"` / `"-Infinity"` → ±∞ (JS accepts these);
///   - anything else → `NaN`.
/// The result feeds `parse_num_query` / the report clamps, which fold NaN to the
/// fallback exactly like `Number.isFinite` does in db.js.
pub fn js_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    if t.is_empty() {
        return 0.0;
    }
    // Signed Infinity literals (JS `Number("Infinity")`/`"-Infinity"`).
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Radix-prefixed integer literals: 0x/0o/0b (case-insensitive, unsigned only,
    // matching JS — a sign in front of a radix literal yields NaN in JS).
    if t.len() > 2 && t.as_bytes()[0] == b'0' {
        let radix = match t.as_bytes()[1] {
            b'x' | b'X' => Some(16u32),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            return match i128::from_str_radix(&t[2..], radix) {
                Ok(n) => n as f64,
                Err(_) => f64::NAN,
            };
        }
    }
    // Decimal / float / exponent. Rust's f64 parser accepts the JS decimal forms
    // we care about; it ALSO accepts "inf"/"nan"/"infinity" (lowercase) which JS
    // would reject, but query params never carry those, and the only ones we must
    // honor ("Infinity") are handled above.
    match t.parse::<f64>() {
        Ok(n) => n,
        Err(_) => f64::NAN,
    }
}

/// Port of server.js `parseIntQuery(raw, fallback)`: use `fallback` only when the
/// param is missing/empty; otherwise `Number(raw)` if finite, else `fallback`.
/// NOTE: despite the JS name, NO integer truncation happens — the value stays a
/// (possibly fractional) `f64` and is carried into the db-layer clamp + echoed
/// `window_hours`, exactly like JS Number coercion. `Number("0")` is 0, NOT the
/// fallback — the bug the JS comment guards against.
pub fn parse_num_query(raw: Option<&str>, fallback: f64) -> f64 {
    match raw {
        None => fallback,
        Some("") => fallback,
        Some(s) => {
            let n = js_number(s);
            if n.is_finite() {
                n
            } else {
                fallback
            }
        }
    }
}

/// Emit a JSON Number that byte-matches JS `JSON.stringify(n)` for the integral
/// case: JS prints an integral Number WITHOUT a decimal (`24`, not `24.0`) while
/// serde prints an `f64` as `24.0`. So when `n` is finite, integral, and fits in
/// `i64`, emit a JSON integer; otherwise emit the `f64` (serde's float formatting
/// matches JS for the fractional window values we echo, e.g. `48.5`). Non-finite
/// `n` (can't occur after clamping) falls back to `Null`.
pub fn json_num(n: f64) -> serde_json::Value {
    if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        serde_json::json!(n as i64)
    } else if n.is_finite() {
        serde_json::json!(n)
    } else {
        serde_json::Value::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- email_rejection_reason -----------------------------------------

    #[test]
    fn typo_tld_cm_is_rejected_with_exact_reason() {
        // The .cm TLD blocklist is the FIRST rule that fires (matches the JS
        // test "logs rejection reason ... email_typo_tld_cm").
        assert_eq!(
            email_rejection_reason(Some("real.person@gmail.cm")).as_deref(),
            Some("email_typo_tld_cm")
        );
        assert_eq!(
            email_rejection_reason(Some("asdfadsf@gmail.cm")).as_deref(),
            Some("email_typo_tld_cm"),
            "typo TLD checked before keyboard-mash"
        );
    }

    #[test]
    fn keyboard_mash_local_is_rejected() {
        assert_eq!(
            email_rejection_reason(Some("asdfasdf@gmail.com")).as_deref(),
            Some("email_keyboard_mash_local")
        );
    }

    #[test]
    fn low_entropy_local_is_rejected() {
        // 5+ chars, no vowels/digits -> low entropy.
        assert_eq!(
            email_rejection_reason(Some("bcdfg@example.com")).as_deref(),
            Some("email_low_entropy_local")
        );
        // 4+ repeated chars -> low entropy (and not caught earlier).
        assert_eq!(
            email_rejection_reason(Some("hmmmm@example.com")).as_deref(),
            Some("email_low_entropy_local")
        );
    }

    #[test]
    fn non_string_empty_long_badformat() {
        assert_eq!(
            email_rejection_reason(None).as_deref(),
            Some("email_not_string")
        );
        assert_eq!(
            email_rejection_reason(Some("")).as_deref(),
            Some("email_empty")
        );
        let long = format!("{}@example.com", "a".repeat(320));
        assert_eq!(
            email_rejection_reason(Some(&long)).as_deref(),
            Some("email_too_long")
        );
        assert_eq!(
            email_rejection_reason(Some("no-at-sign")).as_deref(),
            Some("email_bad_format")
        );
        assert_eq!(
            email_rejection_reason(Some("a b@example.com")).as_deref(),
            Some("email_bad_format"),
            "whitespace in local part is bad format"
        );
        assert_eq!(
            email_rejection_reason(Some("a@b")).as_deref(),
            Some("email_bad_format"),
            "no dot in domain is bad format"
        );
    }

    #[test]
    fn legit_emails_pass() {
        // The JS "accepts legitimate edge cases" test: plus addressing + numbers.
        assert_eq!(email_rejection_reason(Some("jo+test123@example.com")), None);
        assert_eq!(email_rejection_reason(Some("alice@test.local")), None);
        assert_eq!(email_rejection_reason(Some("bob@test.local")), None);
        assert_eq!(email_rejection_reason(Some("carol@test.local")), None);
        assert_eq!(email_rejection_reason(Some("real@example.com")), None);
    }

    // ---- name_rejection_reason ------------------------------------------

    #[test]
    fn keyboard_mash_name_is_rejected() {
        assert_eq!(
            name_rejection_reason(Some("asdfasdf")).as_deref(),
            Some("name_keyboard_mash")
        );
    }

    #[test]
    fn single_letter_name_too_short() {
        assert_eq!(
            name_rejection_reason(Some("a")).as_deref(),
            Some("name_too_short")
        );
    }

    #[test]
    fn two_char_name_ok() {
        // "Jo" (2 chars) must pass — boundary of the >= 2 check.
        assert_eq!(name_rejection_reason(Some("Jo")), None);
    }

    #[test]
    fn name_edge_cases() {
        assert_eq!(
            name_rejection_reason(None).as_deref(),
            Some("name_not_string")
        );
        assert_eq!(
            name_rejection_reason(Some("   ")).as_deref(),
            Some("name_empty"),
            "all-whitespace trims to empty"
        );
        assert_eq!(
            name_rejection_reason(Some(&"x".repeat(201))).as_deref(),
            Some("name_too_long")
        );
        // 5+ repeated chars -> repeating (but "aaaaa" is also a mash token; mash
        // is checked first, so that's the reason).
        assert_eq!(
            name_rejection_reason(Some("aaaaa")).as_deref(),
            Some("name_keyboard_mash")
        );
        // A run of 5 of a char that ISN'T a mash token -> repeating_chars.
        assert_eq!(
            name_rejection_reason(Some("Greggggg")).as_deref(),
            Some("name_repeating_chars")
        );
        // Real names pass.
        assert_eq!(name_rejection_reason(Some("Alice")), None);
        assert_eq!(name_rejection_reason(Some("Real Name")), None);
    }

    // ---- has_repeat_run / is_keyboard_mash ------------------------------

    #[test]
    fn repeat_run_boundaries() {
        // (.)\1{3,} -> run of 4.
        assert!(!has_repeat_run("aaa", 4));
        assert!(has_repeat_run("aaaa", 4));
        // (.)\1{4,} -> run of 5.
        assert!(!has_repeat_run("bbbb", 5));
        assert!(has_repeat_run("bbbbb", 5));
        // Run can be anywhere in the string.
        assert!(has_repeat_run("xyaaaa", 4));
    }

    #[test]
    fn keyboard_mash_is_case_insensitive_substring() {
        assert!(is_keyboard_mash("ASDF"));
        assert!(is_keyboard_mash("xxqwerxx"));
        assert!(!is_keyboard_mash("alice"));
    }

    // ---- clamp_str / parse_int_query ------------------------------------

    #[test]
    fn clamp_str_semantics() {
        assert_eq!(clamp_str(Some("hello"), 3).as_deref(), Some("hel"));
        assert_eq!(clamp_str(Some("hi"), 10).as_deref(), Some("hi"));
        assert_eq!(clamp_str(Some(""), 5), None, "empty -> None");
        assert_eq!(clamp_str(None, 5), None, "missing -> None");
    }

    #[test]
    fn js_number_semantics() {
        assert_eq!(js_number(""), 0.0, "empty -> 0 like Number(\"\")");
        assert_eq!(js_number("   "), 0.0, "whitespace-only -> 0");
        assert_eq!(js_number("12"), 12.0);
        assert_eq!(js_number("0"), 0.0);
        assert_eq!(js_number("48.5"), 48.5, "fractional preserved");
        assert_eq!(js_number(" 24 "), 24.0, "trims ascii whitespace");
        assert_eq!(js_number("1e3"), 1000.0, "exponent form");
        assert_eq!(js_number("0x10"), 16.0, "hex literal");
        assert_eq!(js_number("0o17"), 15.0, "octal literal");
        assert_eq!(js_number("0b101"), 5.0, "binary literal");
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert_eq!(js_number("-Infinity"), f64::NEG_INFINITY);
        assert!(js_number("x").is_nan(), "non-numeric -> NaN");
        assert!(js_number("nope").is_nan());
        assert!(js_number("12.5x").is_nan(), "trailing junk -> NaN");
    }

    #[test]
    fn parse_num_query_semantics() {
        assert_eq!(parse_num_query(None, 8.0), 8.0, "missing -> fallback");
        assert_eq!(parse_num_query(Some(""), 8.0), 8.0, "empty -> fallback");
        assert_eq!(parse_num_query(Some("24"), 8.0), 24.0);
        // The bug the JS comment guards: "0" must NOT become the fallback.
        assert_eq!(parse_num_query(Some("0"), 8.0), 0.0, "zero is a valid value");
        assert_eq!(parse_num_query(Some("48.5"), 24.0), 48.5, "fractional kept");
        assert_eq!(
            parse_num_query(Some("notanumber"), 8.0),
            8.0,
            "NaN -> fallback"
        );
        assert_eq!(
            parse_num_query(Some("x"), 24.0),
            24.0,
            "NaN fallback (activation window 'x' -> 24)"
        );
    }

    #[test]
    fn json_num_byte_matches_js() {
        // Integral -> integer token (no decimal), matching JS JSON.stringify.
        assert_eq!(json_num(24.0).to_string(), "24");
        assert_eq!(json_num(1.0).to_string(), "1");
        assert_eq!(json_num(720.0).to_string(), "720");
        // Fractional -> float token.
        assert_eq!(json_num(48.5).to_string(), "48.5");
        assert_eq!(json_num(1.5).to_string(), "1.5");
    }
}
