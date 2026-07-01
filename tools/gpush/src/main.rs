//! gpush — write Google Tasks (and, later, Calendar) idempotently from a stream
//! of NDJSON "publish intents". One intent per stdin line; one result JSON per
//! line to stdout, in order; every line processed; non-zero exit if any failed.
//!
//! It owns two things the emitter deliberately does not: OAuth (see `oauth`) and
//! the idempotency map (see `idmap`). The emitter keys each item with an opaque
//! string; gpush maps that key to a Google object and thereafter patches rather
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
use model::{Action, Intent, Outcome, Target};
use oauth::Auth;
use tasks::{PatchOutcome, Tasks};

#[derive(Parser)]
#[command(
    name = "gpush",
    version,
    about = "Write Google Tasks/Calendar idempotently from NDJSON intents on stdin.",
    long_about = "Reads one publish-intent JSON per stdin line and writes it to Google Tasks, \
keyed by an opaque idempotency string so re-sending patches instead of duplicating. Emits one \
result JSON per line to stdout in input order; processes every line; exits non-zero if any failed.\n\n\
Config lives in ~/.config/gpush: credentials.json (your Desktop OAuth client) and a cached token. \
Run `gpush auth` once before the first live drain.",
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
    /// One-time Google consent (opens a browser). Reads ~/.config/gpush/credentials.json.
    Auth,
    /// Rebuild the idmap from a list's gpush notes-tags (recovery / drift-repair).
    Reconcile {
        /// Task-list name to scan (default: the Google default list).
        list: Option<String>,
    },
    /// Offline self-check (no network) — backs the formula `test do`.
    Selftest,
}

fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(x).join("gpush")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/gpush")
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = config_dir();

    // The drain path owns its own exit code (non-zero if any line failed, even
    // though the run itself "succeeded"); the others are plain success/failure.
    if cli.command.is_none() {
        return match drain(&dir, cli.drain.dry_run, cli.drain.no_reconcile) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE, // per-line errors are already on stdout
            Err(e) => {
                eprintln!("gpush: {e:#}");
                ExitCode::FAILURE
            }
        };
    }

    let res = match cli.command.unwrap() {
        Command::Selftest => selftest(),
        Command::Auth => Auth::new(&dir).authorize(oauth::TASKS_SCOPE),
        Command::Reconcile { list } => reconcile(&dir, list.as_deref()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gpush: {e:#}");
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
    eprintln!("gpush: reconciled {n} tagged task(s) from list {list_id} into the idmap");
    Ok(())
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

    println!("gpush selftest: OK");
    Ok(())
}
