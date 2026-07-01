//! The idempotency map: `key → {google_id, target, list}`, gootodoo's own private
//! state (the emitter never reads it). Google Tasks forbid client-supplied ids,
//! so this file is the *only* authoritative record of which key maps to which
//! task — losing it means a rebuild scan (see `tasks::scan_keys`) is the only
//! recovery. Writes are atomic (tmp + rename) and guarded by an advisory lock so
//! a manual run can't race the scheduled drainer.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub google_id: String,
    /// "task" | "event" — always "task" until events land, but recorded now so a
    /// mixed map stays unambiguous.
    pub target: String,
    /// The *resolved* Google list id (not the emitter's list name), so close/patch
    /// can address the object without re-resolving.
    pub list: String,
}

pub struct IdMap {
    path: PathBuf,
    // Held for the process lifetime; the flock is released when this fd closes.
    #[allow(dead_code)]
    lock: File,
    map: HashMap<String, Entry>,
}

impl IdMap {
    /// Open (creating the config dir), take an exclusive advisory lock, and load
    /// the map. Fails loudly if another gootodoo holds the lock.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock_path = dir.join("gootodoo.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening lock {}", lock_path.display()))?;
        // std advisory file lock (stable since 1.89); released when `lock`'s fd is
        // dropped at the end of the run.
        lock.try_lock().map_err(|_| {
            anyhow!(
                "another gootodoo is already running (lock held on {}); wait for it to finish",
                lock_path.display()
            )
        })?;

        let path = dir.join("idmap.json");
        let map = match fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, lock, map })
    }

    /// Read the map without locking or creating anything — for `--dry-run`, which
    /// must not touch disk state. Missing/corrupt file → empty map.
    pub fn snapshot(dir: &Path) -> HashMap<String, Entry> {
        fs::read_to_string(dir.join("idmap.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.map.get(key)
    }

    /// Insert/overwrite one mapping and persist immediately, so a crash after the
    /// next line can't lose a just-created task's id.
    pub fn put(&mut self, key: String, entry: Entry) -> Result<()> {
        self.map.insert(key, entry);
        self.persist()
    }

    /// Drop a mapping (used when a patch proves the mapped task is gone in Google),
    /// so a subsequent reconcile/create can re-establish the correct id.
    pub fn remove(&mut self, key: &str) -> Result<()> {
        if self.map.remove(key).is_some() {
            self.persist()?;
        }
        Ok(())
    }

    /// Merge mappings discovered by a rebuild scan. Additive only — never delete,
    /// because a completed/hidden task filtered out of some scan must not drop a
    /// mapping that is still valid.
    pub fn merge_missing(
        &mut self,
        discovered: impl IntoIterator<Item = (String, Entry)>,
    ) -> Result<()> {
        use std::collections::hash_map::Entry;
        let mut added = false;
        for (k, e) in discovered {
            if let Entry::Vacant(slot) = self.map.entry(k) {
                slot.insert(e);
                added = true;
            }
        }
        if added {
            self.persist()?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(&self.map)?;
        {
            let mut f = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(data.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}
