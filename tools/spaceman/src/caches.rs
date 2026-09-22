//! Browser cache collector, enabled with `--caches`.
//!
//! Chrome and Chromium on macOS and Linux use the "simple" disk cache: one file
//! per entry named `<16 hex>_<0|1|s>`, plus an index. A missing entry file is a
//! cache miss, so old entries can go while the browser runs. Only files with
//! exactly that name shape, older than `days`, inside a profile's `Cache` or
//! `Code Cache` are touched; the index and everything else stay.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::DAY;

/// Cache roots relative to $HOME; each holds one directory per browser profile.
const BASES: &[&str] = &[
    "Library/Caches/Google/Chrome",
    "Library/Caches/Chromium",
    ".cache/google-chrome",
    ".cache/chromium",
];

const ENTRY_DIRS: &[&str] = &["Cache/Cache_Data", "Code Cache/js", "Code Cache/wasm"];

pub struct Stale {
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub bytes: u64,
}

fn is_entry(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 18
        && b[..16].iter().all(u8::is_ascii_hexdigit)
        && b[16] == b'_'
        && matches!(b[17], b'0' | b'1' | b's')
}

fn scan_dir(dir: &Path, now: SystemTime, days: u64) -> Option<Stale> {
    // An entry is up to three files sharing a 16-hex key. It goes as a whole or
    // not at all: half an entry is dead weight the browser will not reclaim.
    let mut entries: BTreeMap<String, (bool, Vec<(PathBuf, u64)>)> = BTreeMap::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| md.is_file() && is_entry(n)) else {
            continue;
        };
        let old = md
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() / DAY >= days);
        let slot = entries
            .entry(name[..16].to_string())
            .or_insert((true, Vec::new()));
        slot.0 &= old;
        slot.1.push((entry.path(), md.blocks() * 512));
    }
    let mut stale = Stale {
        dir: dir.to_path_buf(),
        files: Vec::new(),
        bytes: 0,
    };
    for (path, bytes) in entries
        .into_values()
        .filter(|(all_old, _)| *all_old)
        .flat_map(|(_, files)| files)
    {
        stale.bytes += bytes;
        stale.files.push(path);
    }
    (!stale.files.is_empty()).then_some(stale)
}

pub fn plan(home: &Path, now: SystemTime, days: u64) -> Vec<Stale> {
    let mut out = Vec::new();
    for base in BASES {
        let Ok(profiles) = fs::read_dir(home.join(base)) else {
            continue;
        };
        for profile in profiles.flatten() {
            // DirEntry::metadata does not follow symlinks; a symlinked profile is skipped.
            if !profile.metadata().is_ok_and(|md| md.is_dir()) {
                continue;
            }
            for sub in ENTRY_DIRS {
                out.extend(scan_dir(&profile.path().join(sub), now, days));
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    out
}

/// Returns (bytes freed, files that could not be removed) for one directory.
pub fn execute(stale: &Stale) -> (u64, usize) {
    let (mut freed, mut failed) = (0, 0);
    for f in &stale.files {
        let size = fs::symlink_metadata(f).map_or(0, |md| md.blocks() * 512);
        match fs::remove_file(f) {
            Ok(()) => freed += size,
            // The browser evicted it first: nothing to do.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed += 1,
        }
    }
    (freed, failed)
}

#[cfg(test)]
mod tests {
    use super::is_entry;

    #[test]
    fn entry_names() {
        assert!(is_entry("0001618b23d1f842_0"));
        assert!(is_entry("0001618b23d1f842_s"));
        for bad in [
            "index",
            "the-real-index",
            "0001618b23d1f842_2",
            "0001618b23d1f84_0",
            "data_0",
        ] {
            assert!(!is_entry(bad), "{bad}");
        }
    }
}
