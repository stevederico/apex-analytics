//! Experiment arm per install, plus the first-success milestone name.
//!
//! WHY A HASH, NOT INSTALL-SEQUENCE PARITY. The desktop has to render its arm
//! on the very first launch, often before it has reached this server (offline
//! first run, register still in flight). A sequence number only exists after a
//! server round trip, races when two installs ping at once, and shifts when
//! smoke/excluded installs are dropped. A salted SHA-256 of the install id is
//! stable, needs no table, splits ~50/50, and the client and every report
//! compute the same answer — including over history recorded before the
//! client knew about variants.
//!
//! Rule (client MUST match): `sha256("<SALT>:<install_id>")[0]` even →
//! `control`, odd → `test`. `install_id` is the exact string the client sends.
//! Change `SALT` to re-randomize for the next experiment.

use sha2::{Digest, Sha256};

/// Salt for the current experiment (Exp 1: gateway-ready state + starters).
pub const SALT: &str = "exp1";

pub const CONTROL: &str = "control";
pub const TEST: &str = "test";

/// First chat or action that actually completed (not blocked, no error).
/// Fired once per install; the server keeps only the first one.
pub const FIRST_SUCCESS: &str = "funnel.first_success";

/// Arm for an install. Empty id (unlinked signup) has no arm.
pub fn variant_for(install_id: &str) -> Option<&'static str> {
    if install_id.is_empty() {
        return None;
    }
    let digest = Sha256::digest(format!("{SALT}:{install_id}").as_bytes());
    Some(if digest[0] % 2 == 0 { CONTROL } else { TEST })
}

/// `?variant=` filter. `None` = no filter; `Err` = unknown value (400).
pub fn parse_filter(raw: Option<&str>) -> Result<Option<&'static str>, ()> {
    match raw.map(str::trim) {
        None | Some("") | Some("all") => Ok(None),
        Some(CONTROL) => Ok(Some(CONTROL)),
        Some(TEST) => Ok(Some(TEST)),
        Some(_) => Err(()),
    }
}

/// True when `install_id` belongs to `filter` (or there is no filter).
pub fn matches(filter: Option<&str>, install_id: &str) -> bool {
    match filter {
        None => true,
        Some(want) => variant_for(install_id) == Some(want),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_split() {
        assert_eq!(variant_for("abc"), variant_for("abc"));
        assert_eq!(variant_for(""), None);
        let test = (0..2000)
            .filter(|i| variant_for(&format!("install-{i}")) == Some(TEST))
            .count();
        assert!((900..1100).contains(&test), "split was {test}/2000");
    }

    #[test]
    fn filter_parsing() {
        assert_eq!(parse_filter(None), Ok(None));
        assert_eq!(parse_filter(Some("all")), Ok(None));
        assert_eq!(parse_filter(Some("test")), Ok(Some(TEST)));
        assert_eq!(parse_filter(Some("control")), Ok(Some(CONTROL)));
        assert!(parse_filter(Some("b")).is_err());
    }
}
