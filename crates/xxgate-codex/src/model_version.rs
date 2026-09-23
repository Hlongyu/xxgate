//! Model discovery follows stable Codex releases independently of the wire baseline.
use std::sync::RwLock;

static VERSION: RwLock<Option<String>> = RwLock::new(None);

pub fn parse(version: &str) -> Option<[u32; 3]> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut result = [0; 3];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|c| c.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        result[i] = part.parse().ok()?;
    }
    Some(result)
}

pub fn current() -> String {
    VERSION
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| crate::CODEX_VERSION.into())
}

/// Reject malformed releases and never downgrade a successfully observed version.
pub fn update(version: &str) -> bool {
    advance(&mut VERSION.write().unwrap(), version)
}

fn advance(guard: &mut Option<String>, version: &str) -> bool {
    let Some(next) = parse(version) else {
        return false;
    };
    let previous = guard.as_deref().unwrap_or(crate::CODEX_VERSION);
    if next <= parse(previous).unwrap() {
        return false;
    }
    *guard = Some(version.into());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_or_older_versions_and_retains_last_success() {
        let mut version = None;
        assert!(!advance(&mut version, "0.99.0"));
        assert!(advance(&mut version, "0.155.1"));
        for candidate in ["0.154.0", "0.155.1", "invalid", "0.156.0-alpha.1"] {
            assert!(!advance(&mut version, candidate));
            assert_eq!(version.as_deref(), Some("0.155.1"));
        }
    }
    #[test]
    fn stable_versions_are_strict_and_ordered_numerically() {
        assert!(parse("0.155.1") > parse("0.99.0"));
        for invalid in [
            "0.155.1-alpha.1",
            "rust-v0.155.1",
            "0.155",
            "0.01.0",
            "0.1.2\r\n",
            "+0.1.2",
        ] {
            assert_eq!(parse(invalid), None);
        }
    }
}
