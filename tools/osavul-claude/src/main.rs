//! osavul-claude — safe-claude plus the osavul spool.
//!
//! Grants `$HOME/.local/share/osavul` (read+write) on top of whatever profile
//! safe-claude selects, through safe-claude's `SAFE_CLAUDE_EXTRA_ALLOW`
//! extension point (safe-claude >= 0.5.28, urban-sky/tap). Everything else
//! is inherited: aiq account pooling, per-project transcript isolation, and
//! every grant safe-claude ships.
//!
//! Why not a nono profile that `extends: safe-claude`: `extends` resolves the
//! PROMOTED profile by name, while safe-claude runs its BUNDLED template
//! whenever the promoted copy is behind. An extending profile therefore runs
//! whatever was promoted last, with no staleness notice. The env var rides
//! the command line on top of the profile the wrapper actually picked, so it
//! is current by construction and never needs `nono profile promote`.
//!
//! Why a binary and not a five-line shell script: this tap's release spine is
//! one Rust lane (cargo build -> tarball -> forge formula). A shell lane would
//! fork forge, release.yml, ci.yml and Cargo.toml for two files. ~330 KB of
//! binary is cheaper than that.
//!
//! `safe-claude` is resolved through PATH. Under an aiq launcher (aiq drops
//! its shim dir from PATH before launching) that is the real wrapper. Invoked
//! directly, whatever `safe-claude` is first on PATH runs, an aiq shim
//! included; the grant is an exported env var, so it survives either route.
//!
//! The spool directory is created if missing: safe-claude skips a grant whose
//! path does not exist (Landlock silently drops such rules), so an absent
//! spool would mean a session with no grant at all.
//!
//! There is no `depends_on` in the formula: urban-sky/tap is private, and
//! Homebrew will not tap it on your behalf. A missing safe-claude is reported
//! at run time with the install command, exit 127.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, exit};

const NAME: &str = "osavul-claude";
const WRAPPED: &str = "safe-claude";
const EXTRA_ALLOW: &str = "SAFE_CLAUDE_EXTRA_ALLOW";
const INSTALL_HINT: &str = "brew install urban-sky/tap/safe-claude";
/// Spool override, the same variable osavul's own scripts honour (a frozen
/// contract there): `OSAVUL_SPOOL`.
const SPOOL_OVERRIDE: &str = "OSAVUL_SPOOL";

/// The osavul spool directory: `$OSAVUL_SPOOL`, else `$HOME/.local/share/osavul`.
fn spool_dir(override_: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(p) = override_.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let home = home.filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/osavul"))
}

/// Compose the colon-separated grant list: whatever the caller already set,
/// then the spool, unless it is already listed (a wrapper re-invoked from
/// inside its own session inherits the value). Keeping the caller's entries
/// matters for a launcher that stacks grants (aiq passes env through
/// untouched). The format is safe-claude's, colon-separated like PATH, so a
/// spool path containing a colon cannot be expressed; nothing here rescues
/// that.
fn extra_allow(existing: Option<OsString>, spool: &OsStr) -> OsString {
    match existing.filter(|e| !e.is_empty()) {
        Some(e)
            if e.as_encoded_bytes()
                .split(|b| *b == b':')
                .any(|p| p == spool.as_encoded_bytes()) =>
        {
            e
        }
        Some(mut e) => {
            e.push(":");
            e.push(spool);
            e
        }
        None => spool.to_os_string(),
    }
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    let Some(spool) = spool_dir(env::var_os(SPOOL_OVERRIDE), env::var_os("HOME")) else {
        eprintln!("{NAME}: neither {SPOOL_OVERRIDE} nor HOME is set; cannot locate the spool");
        exit(2);
    };
    let value = extra_allow(env::var_os(EXTRA_ALLOW), spool.as_os_str());

    // Deterministic self-check for the formula's `test do` and the release
    // smoke test. Never launches anything and touches nothing. Only the exact
    // invocation `osavul-claude selftest` is intercepted; `selftest` followed
    // by anything else passes through to the agent.
    if args.len() == 1 && args[0] == "selftest" {
        println!(
            "{NAME}: would exec {WRAPPED} with {EXTRA_ALLOW}={}",
            value.to_string_lossy()
        );
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&spool) {
        // Not fatal: safe-claude will report the skipped grant on its own.
        eprintln!(
            "{NAME}: cannot create spool {} ({e}); the grant will be skipped",
            spool.display()
        );
    }

    // exec only returns on failure.
    let err = Command::new(WRAPPED)
        .args(&args)
        .env(EXTRA_ALLOW, &value)
        .exec();
    if err.kind() == ErrorKind::NotFound {
        eprintln!("{NAME}: `{WRAPPED}` is not on PATH. Install it with: {INSTALL_HINT}");
        exit(127);
    }
    eprintln!("{NAME}: failed to exec {WRAPPED}: {err}");
    exit(126);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn spool_defaults_under_home() {
        let p = spool_dir(None, os("/Users/x")).unwrap();
        assert_eq!(p, PathBuf::from("/Users/x/.local/share/osavul"));
    }

    #[test]
    fn spool_override_wins_and_empty_is_unset() {
        assert_eq!(
            spool_dir(os("/spool"), os("/Users/x")).unwrap(),
            PathBuf::from("/spool")
        );
        assert_eq!(
            spool_dir(os(""), os("/Users/x")).unwrap(),
            PathBuf::from("/Users/x/.local/share/osavul")
        );
        assert!(spool_dir(None, None).is_none());
        assert!(spool_dir(os(""), os("")).is_none());
    }

    #[test]
    fn extra_allow_keeps_existing_entries_first() {
        assert_eq!(extra_allow(None, OsStr::new("/s")), OsString::from("/s"));
        assert_eq!(extra_allow(os(""), OsStr::new("/s")), OsString::from("/s"));
        assert_eq!(
            extra_allow(os("/a:/b"), OsStr::new("/s")),
            OsString::from("/a:/b:/s")
        );
    }

    #[test]
    fn extra_allow_does_not_duplicate_the_spool() {
        assert_eq!(
            extra_allow(os("/s"), OsStr::new("/s")),
            OsString::from("/s")
        );
        assert_eq!(
            extra_allow(os("/a:/s:/b"), OsStr::new("/s")),
            OsString::from("/a:/s:/b")
        );
        // A prefix match is not a match.
        assert_eq!(
            extra_allow(os("/s2"), OsStr::new("/s")),
            OsString::from("/s2:/s")
        );
    }
}
