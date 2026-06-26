//! Per-directory configuration: load a `.env` into the process environment
//! **without** overriding variables already set in the real shell. Real env wins,
//! so a one-off override works inline: `IMAP_PASSWORD=… imap-extract`.

use std::path::Path;

/// Load `.env` at `path` into the process environment, skipping any key already
/// set. Returns `Ok(true)` if the file existed, `Ok(false)` if it was absent.
pub fn load_env(path: impl AsRef<Path>) -> std::io::Result<bool> {
    // dotenvy::from_path does not override already-set vars (matches the real-env-wins rule).
    match dotenvy::from_path(path.as_ref()) {
        Ok(()) => Ok(true),
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(dotenvy::Error::Io(e)) => Err(e),
        Err(e) => Err(std::io::Error::other(e)),
    }
}

/// Get a string var, or `None` if unset/empty.
pub fn get(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Get a string var or a default.
pub fn get_or(key: &str, default: &str) -> String {
    get(key).unwrap_or_else(|| default.to_string())
}

/// Parse a boolean var. Anything other than the falsey set is treated as the
/// default when unset; an explicit "false"/"0"/"no"/"off" is false.
fn is_truthy(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "no" | "off" | ""
    )
}

pub fn get_bool(key: &str, default: bool) -> bool {
    match get(key) {
        Some(v) => is_truthy(&v),
        None => default,
    }
}

/// Parse an integer var, falling back to `default` when unset or unparseable.
pub fn get_int(key: &str, default: i64) -> i64 {
    get(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::is_truthy;

    #[test]
    fn truthy_values() {
        for v in ["true", "1", "yes", "on", "TRUE", "anything"] {
            assert!(is_truthy(v), "{v} should be truthy");
        }
    }

    #[test]
    fn falsey_values() {
        for v in ["false", "0", "no", "off", "FALSE", " Off ", ""] {
            assert!(!is_truthy(v), "{v} should be falsey");
        }
    }
}
