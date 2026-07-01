//! gootodoo — write Google Tasks (and, later, Calendar) idempotently from a stream
//! of NDJSON "publish intents". One intent per stdin line; one result JSON per
//! line to stdout, in order; every line processed; non-zero exit if any failed.
//!
//! It owns two things the emitter deliberately does not: OAuth (see `oauth`) and
//! the idempotency map (see `idmap`). The emitter keys each item with an opaque
//! string; gootodoo maps that key to a Google object and thereafter patches rather
//! than duplicates. MVP is tasks-only; the event fields in the frozen contract
//! are carried through but not yet acted on.

mod idmap;
mod model;
mod oauth;
mod tasks;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use idmap::{Entry, IdMap};
use model::{Action, Intent, Outcome, PullRow, Target};
use oauth::Auth;
use tasks::{PatchOutcome, Tasks};

#[derive(Parser)]
#[command(
    name = "gootodoo",
    version,
    about = "Write Google Tasks/Calendar idempotently from NDJSON intents on stdin.",
    long_about = "Reads one publish-intent JSON per stdin line and writes it to Google Tasks, \
keyed by an opaque idempotency string so re-sending patches instead of duplicating. Emits one \
result JSON per line to stdout in input order; processes every line; exits non-zero if any failed.\n\n\
Config lives in ~/.config/gootodoo: credentials.json (your Desktop OAuth client) and a cached token. \
Run `gootodoo auth` once before the first live drain.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    drain: DrainArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Args)]
struct DrainArgs {
    /// Validate + report intended actions without touching the network or the idmap.
    #[arg(long)]
    dry_run: bool,
    /// Skip the reconcile-before-create scan (faster; only safe when no prior batch was interrupted).
    #[arg(long)]
    no_reconcile: bool,
}

#[derive(Subcommand)]
enum Command {
    /// One-time Google consent (opens a browser). Reads ~/.config/gootodoo/credentials.json.
    Auth,
    /// Rebuild the idmap from a list's gootodoo notes-tags (recovery / drift-repair).
    Reconcile {
        /// Task-list name to scan (default: the Google default list).
        list: Option<String>,
    },
    /// Read back the state of managed tasks (what the human completed / deleted in
    /// Google) as NDJSON, one row per key in the idmap. Read-only.
    Pull {
        /// Only tasks in this list (default: every list gootodoo manages).
        #[arg(long)]
        list: Option<String>,
        /// Only emit tasks the human has completed.
        #[arg(long = "completed-only")]
        completed_only: bool,
    },
    /// Offline self-check (no network) — backs the formula `test do`.
    Selftest,
}

fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(x).join("gootodoo")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/gootodoo")
    }
}

/// Pre-rename config location (the tool was `gpush` through v0.1.0).
fn legacy_config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(x).join("gpush")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/gpush")
    }
}

/// One-time migration of the pre-rename config dir. If the new dir doesn't exist
/// yet but the legacy `gpush` one does, move it wholesale so the cached token +
/// idmap + credentials carry over and the user needn't re-auth. A rename failure
/// is a hard error, not a shrug: continuing with a fresh empty config would make
/// the next drain treat every existing key as new and duplicate every task, so we
/// surface it and let the caller abort. (Stop any running `gpush` before the first
/// `gootodoo` run — the move is not coordinated with a live v0.1 lock.)
fn migrate_legacy_config(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let legacy = legacy_config_dir();
    if !legacy.exists() || legacy == *dir {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(&legacy, dir).with_context(|| {
        format!(
            "migrating config {} -> {} failed; move it by hand (`mv {} {}`) and retry",
            legacy.display(),
            dir.display(),
            legacy.display(),
            dir.display()
        )
    })?;
    eprintln!(
        "gootodoo: migrated config {} -> {}",
        legacy.display(),
        dir.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = config_dir();

    // Migrate a pre-rename ~/.config/gpush into place for any command that touches
    // config — not the offline selftest. A FAILED migration aborts loudly: silently
    // proceeding with an empty config would treat every existing key as new and
    // duplicate every task.
    if !matches!(cli.command, Some(Command::Selftest))
        && let Err(e) = migrate_legacy_config(&dir)
    {
        eprintln!("gootodoo: {e:#}");
        return ExitCode::FAILURE;
    }

    // Commands that process many items (drain, pull) own their exit code: success
    // unless some item failed — its error is already on stdout, so we don't also
    // print to stderr. The rest are plain ok/err, mapped onto the same `bool`.
    let ran: Result<bool> = match cli.command {
        None => drain(&dir, cli.drain.dry_run, cli.drain.no_reconcile),
        Some(Command::Pull {
            list,
            completed_only,
        }) => pull(&dir, list.as_deref(), completed_only),
        Some(Command::Selftest) => selftest().map(|()| true),
        Some(Command::Auth) => Auth::new(&dir).authorize(oauth::TASKS_SCOPE).map(|()| true),
        Some(Command::Reconcile { list }) => reconcile(&dir, list.as_deref()).map(|()| true),
    };
    match ran {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("gootodoo: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Live backend for a drain, or a read-only idmap snapshot for `--dry-run`.
enum Backend {
    DryRun(HashMap<String, Entry>),
    Live(Box<Live>),
}

struct Live {
    tasks: Tasks,
    idmap: IdMap,
    /// Emitter list-name → resolved Google list id, populated only *after* that
    /// list has been resolved AND reconcile-scanned successfully. Doubles as the
    /// "already reconciled" marker: a transient scan failure leaves it absent, so
    /// the next unknown key for that list retries the scan instead of skipping it.
    list_cache: HashMap<Option<String>, String>,
    no_reconcile: bool,
}

fn drain(dir: &Path, dry_run: bool, no_reconcile: bool) -> Result<bool> {
    let mut backend = if dry_run {
        Backend::DryRun(IdMap::snapshot(dir))
    } else {
        let idmap = IdMap::open(dir)?;
        // Validate credentials eagerly so a bad/expired token fails loudly before
        // we start mutating Google. The Auth is then owned by Tasks, which refreshes
        // through it transparently — a batch outliving one token lifetime keeps going.
        let auth = Auth::new(dir);
        auth.access_token()?;
        Backend::Live(Box::new(Live {
            tasks: Tasks::new(auth),
            idmap,
            list_cache: HashMap::new(),
            no_reconcile,
        }))
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut all_ok = true;
    for (i, line) in stdin.lock().lines().enumerate() {
        let lineno = i + 1;
        let line = line.with_context(|| format!("reading stdin line {lineno}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let outcome = process_line(&mut backend, &line, lineno);
        if !outcome.ok {
            all_ok = false;
        }
        writeln!(out, "{}", serde_json::to_string(&outcome)?)?;
        out.flush()?;
    }
    Ok(all_ok)
}

fn process_line(backend: &mut Backend, line: &str, lineno: usize) -> Outcome {
    let intent: Intent = match serde_json::from_str(line) {
        Ok(i) => i,
        // Distinguish "not JSON at all" from "valid JSON, wrong shape" (e.g. a
        // missing `key`) so the error — and the echoed op/target — are useful.
        Err(e) => {
            return match serde_json::from_str::<serde_json::Value>(line) {
                Ok(raw) => Outcome::malformed(&raw, format!("invalid intent: {e}")),
                Err(_) => Outcome::unparseable(lineno),
            };
        }
    };
    let action = match intent.validate() {
        Ok(a) => a,
        Err(msg) => return Outcome::invalid(&intent, msg),
    };
    match backend {
        Backend::DryRun(map) => dry_outcome(map, &intent, &action),
        Backend::Live(live) => live_apply(live, &intent, &action)
            .unwrap_or_else(|e| Outcome::invalid(&intent, format!("{e:#}"))),
    }
}

/// Offline decision: created vs updated vs closed vs noop, read from the idmap
/// snapshot only — no network, no writes.
fn dry_outcome(map: &HashMap<String, Entry>, intent: &Intent, action: &Action) -> Outcome {
    match action {
        Action::Upsert {
            target: Target::Event,
            ..
        } => Outcome::invalid(intent, "event target not implemented (calendar is phase-2)"),
        Action::Upsert {
            target: Target::Task,
            ..
        } => {
            let known = map.get(&intent.key);
            let status = if known.is_some() {
                "updated"
            } else {
                "created"
            };
            Outcome::ok(intent, status, known.map(|e| e.google_id.clone()))
        }
        Action::Close => match map.get(&intent.key) {
            Some(e) => Outcome::ok(intent, "closed", Some(e.google_id.clone())),
            None => Outcome::ok(intent, "noop", None),
        },
    }
}

fn live_apply(live: &mut Live, intent: &Intent, action: &Action) -> Result<Outcome> {
    match action {
        Action::Upsert {
            target: Target::Event,
            ..
        } => Ok(Outcome::invalid(
            intent,
            "event target not implemented (calendar is phase-2)",
        )),
        Action::Upsert {
            target: Target::Task,
            title,
        } => {
            let notes = model::stamp_notes(intent.notes.as_deref(), &intent.key);
            let due = intent.due.as_deref().map(model::due_to_rfc3339);

            // Known key → patch in place.
            if let Some(entry) = live.idmap.get(&intent.key).cloned() {
                match live.tasks.patch_task(
                    &entry.list,
                    &entry.google_id,
                    title,
                    &notes,
                    due.as_deref(),
                )? {
                    PatchOutcome::Patched => {
                        return Ok(Outcome::ok(intent, "updated", Some(entry.google_id)));
                    }
                    // Mapped task is gone in Google. Drop the stale pointer so the
                    // reconcile scan below can repair it (or a fresh create can),
                    // instead of us patching a dead id forever.
                    PatchOutcome::NotFound => live.idmap.remove(&intent.key)?,
                }
            }

            // Unknown (or the mapped task vanished): resolve + reconcile the list,
            // which may recover a mapping a crash dropped — patch it if so, else create.
            let list_id = ensure_list_ready(live, intent.list.as_deref())?;
            if let Some(entry) = live.idmap.get(&intent.key).cloned()
                && let PatchOutcome::Patched = live.tasks.patch_task(
                    &entry.list,
                    &entry.google_id,
                    title,
                    &notes,
                    due.as_deref(),
                )?
            {
                return Ok(Outcome::ok(intent, "updated", Some(entry.google_id)));
            }
            let id = live
                .tasks
                .create_task(&list_id, title, &notes, due.as_deref())?;
            live.idmap.put(
                intent.key.clone(),
                Entry {
                    google_id: id.clone(),
                    target: "task".to_string(),
                    list: list_id,
                },
            )?;
            Ok(Outcome::ok(intent, "created", Some(id)))
        }
        Action::Close => match live.idmap.get(&intent.key).cloned() {
            // Unknown key → noop, never an error. Close carries no list, so there is
            // nothing to scan; an item we never recorded is, from here, a no-op.
            None => Ok(Outcome::ok(intent, "noop", None)),
            Some(entry) => match live.tasks.complete_task(&entry.list, &entry.google_id)? {
                PatchOutcome::Patched => Ok(Outcome::ok(intent, "closed", Some(entry.google_id))),
                PatchOutcome::NotFound => Ok(Outcome::ok(intent, "noop", None)),
            },
        },
    }
}

/// Resolve a list name to its id and, the first time each list is touched, scan
/// it to recover any mappings the idmap is missing (the create-gap safety net).
/// Both the resolution and the scan are cached under `list_cache` — but only once
/// they have *succeeded*, so a transient failure is retried rather than silently
/// disabling the net for the rest of the batch.
fn ensure_list_ready(live: &mut Live, list_name: Option<&str>) -> Result<String> {
    let cache_key = list_name.map(str::to_string);
    if let Some(id) = live.list_cache.get(&cache_key) {
        return Ok(id.clone());
    }
    let list_id = live.tasks.resolve_or_create_list(list_name)?;
    if !live.no_reconcile {
        let discovered = live.tasks.scan_keys(&list_id)?;
        let list_for_entries = list_id.clone();
        live.idmap
            .merge_missing(discovered.into_iter().map(move |(k, id)| {
                (
                    k,
                    Entry {
                        google_id: id,
                        target: "task".to_string(),
                        list: list_for_entries.clone(),
                    },
                )
            }))?;
    }
    live.list_cache.insert(cache_key, list_id.clone());
    Ok(list_id)
}

fn reconcile(dir: &Path, list_name: Option<&str>) -> Result<()> {
    let mut idmap = IdMap::open(dir)?;
    let auth = Auth::new(dir);
    auth.access_token()?;
    let tasks = Tasks::new(auth);
    let list_id = tasks.resolve_or_create_list(list_name)?;
    let discovered = tasks.scan_keys(&list_id)?;
    let n = discovered.len();
    let list_for_entries = list_id.clone();
    idmap.merge_missing(discovered.into_iter().map(move |(k, id)| {
        (
            k,
            Entry {
                google_id: id,
                target: "task".to_string(),
                list: list_for_entries.clone(),
            },
        )
    }))?;
    eprintln!("gootodoo: reconciled {n} tagged task(s) from list {list_id} into the idmap");
    Ok(())
}

/// Read-back: emit the current state of every managed task (one row per idmap key)
/// as NDJSON. Idmap-driven so it's exactly one row per key — a mid-move orphan can
/// never surface as a duplicate. Never writes to Google and never mutates the idmap
/// (it may refresh the cached OAuth token). Like the drain, it processes every key:
/// a task that fails to read becomes a `status:"error"` row and pull exits non-zero,
/// rather than truncating the stream. Returns whether every row succeeded.
fn pull(dir: &Path, list_name: Option<&str>, completed_only: bool) -> Result<bool> {
    let map = IdMap::snapshot(dir);
    let auth = Auth::new(dir);
    auth.access_token()?;
    let tasks = Tasks::new(auth);

    // id <-> name, used for the output `list` field and the optional --list filter.
    // Read-only: we look lists up here, never create one.
    let lists = tasks.list_tasklists()?;
    let name_of = |id: &str| -> String {
        lists
            .iter()
            .find(|(lid, _)| lid == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let filter_id: Option<String> = match list_name {
        Some(n) => Some(
            lists
                .iter()
                .find(|(_, t)| t == n)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| anyhow::anyhow!("no such task-list: {n:?}"))?,
        ),
        None => None,
    };

    // Deterministic output order (the HashMap isn't ordered): sort by key.
    let mut entries: Vec<(&String, &Entry)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut all_ok = true;
    for (key, entry) in entries {
        if entry.target != "task" {
            continue; // events aren't managed yet
        }
        if let Some(fid) = &filter_id
            && &entry.list != fid
        {
            continue;
        }
        // A read failure becomes an error row (not an abort), so one transient
        // hiccup can't make every task past it look absent to the reconciler.
        let (status, completed_at, error) = match tasks.get_task(&entry.list, &entry.google_id) {
            // 404 → the task is no longer at its managed (list, id): deleted, or
            // (rarely, off-model) manually moved to another list. A single GET
            // can't distinguish those — see PullRow docs.
            Ok(None) => ("deleted".to_string(), None, None),
            Ok(Some((st, c))) => (st, c, None),
            Err(e) => {
                all_ok = false;
                ("error".to_string(), None, Some(format!("{e:#}")))
            }
        };
        // --completed-only keeps only completed rows; error rows always pass so a
        // failure is never silently swallowed.
        if completed_only && status != "completed" && status != "error" {
            continue;
        }
        let row = PullRow {
            key: key.clone(),
            google_id: entry.google_id.clone(),
            status,
            completed_at,
            list: name_of(&entry.list),
            error,
        };
        writeln!(out, "{}", serde_json::to_string(&row)?)?;
    }
    out.flush()?;
    Ok(all_ok)
}

/// Deterministic, offline self-check exercised by the formula `test do` and CI.
fn selftest() -> Result<()> {
    use model::*;

    let intent: Intent = serde_json::from_str(
        r#"{"key":"k1","op":"upsert","target":"task","title":"hello","due":"2026-07-04"}"#,
    )
    .context("selftest: parse")?;
    match intent.validate() {
        Ok(Action::Upsert {
            target: Target::Task,
            ref title,
        }) if title == "hello" => {}
        other => anyhow::bail!("selftest: unexpected validate result: {other:?}"),
    }

    if due_to_rfc3339("2026-07-04") != "2026-07-04T00:00:00.000Z" {
        anyhow::bail!("selftest: due conversion wrong");
    }

    let stamped = stamp_notes(Some("body"), "osavul:tax:1");
    if extract_key(&stamped).as_deref() != Some("osavul:tax:1") {
        anyhow::bail!("selftest: notes-tag round-trip failed");
    }

    // Dry-run decisions against a known map.
    let mut map = HashMap::new();
    map.insert(
        "known".to_string(),
        Entry {
            google_id: "gid1".to_string(),
            target: "task".to_string(),
            list: "@default".to_string(),
        },
    );
    let known: Intent =
        serde_json::from_str(r#"{"key":"known","op":"upsert","target":"task","title":"x"}"#)?;
    let a = known.validate().map_err(|e| anyhow::anyhow!(e))?;
    let o = dry_outcome(&map, &known, &a);
    if o.status != "updated" || o.google_id.as_deref() != Some("gid1") {
        anyhow::bail!("selftest: known key should be 'updated'");
    }

    let close_unknown: Intent = serde_json::from_str(r#"{"key":"nope","op":"close"}"#)?;
    let a = close_unknown.validate().map_err(|e| anyhow::anyhow!(e))?;
    if dry_outcome(&map, &close_unknown, &a).status != "noop" {
        anyhow::bail!("selftest: close of unknown key should be 'noop'");
    }

    let u = Outcome::unparseable(3);
    if u.ok || u.key.is_some() {
        anyhow::bail!("selftest: unparseable outcome shape wrong");
    }

    println!("gootodoo selftest: OK");
    Ok(())
}
