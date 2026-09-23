//! spaceman — reclaim disk from stale, regenerable build directories, and on
//! request from Docker (`--docker`, docker.rs) and the Chrome disk cache
//! (`--caches`, caches.rs).
//!
//! The default collector: `node_modules`, `.next`, `.turbo`, Cargo `target` and
//! Python venvs inside git checkouts. A directory is deleted only when every
//! gate holds, and every gate fails closed:
//!
//! 1. it sits inside a git checkout found under a configured root;
//! 2. nothing in that checkout (sources, git HEAD/index, the directory itself)
//!    was written for `--days` days — and the same for any checkout that
//!    symlinks to it;
//! 3. no running process has its cwd or a mapped binary inside the checkout;
//! 4. `target` carries Cargo's `CACHEDIR.TAG`, a venv carries `pyvenv.cfg`;
//! 5. git says the directory is ignored and holds no tracked files.
//!
//! `scan` (the default) only reports. `run` deletes and appends every deletion to
//! a JSONL ledger, closed by a `"kind":"run"` record once the run gets through
//! planning; `log` reads it back (history.rs). There is no scheduler subcommand and no default root: with no
//! roots configured the tool does nothing. See README.md for a launchd snippet.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod caches;
mod docker;
mod history;

/// Directory names treated as regenerable. Adding one here is a decision about
/// deleting people's files unattended — it needs a marker or a git gate that
/// makes a false positive implausible, not just a plausible name.
const GENERATED: &[&str] = &["node_modules", ".next", ".turbo", "target", ".venv", "venv"];

const DAY: u64 = 86_400;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Directory tree to search (repeatable). Default: the lines of
    /// ~/.config/spaceman/roots
    #[arg(long = "root", global = true, value_name = "DIR")]
    roots: Vec<PathBuf>,
    /// Days a checkout must sit idle before its generated dirs are eligible
    #[arg(long, global = true, default_value_t = 7, value_parser = clap::value_parser!(u64).range(1..=36_500))]
    days: u64,
    /// Refuse to delete when more directories (or, separately, more docker
    /// objects) than this are eligible
    #[arg(long, global = true, default_value_t = 200)]
    max_items: usize,
    /// Also remove long-stopped containers, long-unused images, stale build
    /// cache and dangling anonymous volumes (never named volumes)
    #[arg(long, global = true)]
    docker: bool,
    /// Also remove Chrome/Chromium disk-cache entries older than --days
    #[arg(long, global = true)]
    caches: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report what `run` would delete (default; deletes nothing)
    Scan {
        /// Also list the directories that were kept, with the reason
        #[arg(long)]
        all: bool,
    },
    /// Delete what `scan` reports and record each removal in the ledger
    Run,
    /// Show past runs from ~/.local/state/spaceman/ledger.jsonl
    Log {
        /// How many of the most recent runs to show
        #[arg(short = 'n', long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
        last: u64,
        /// Also list every removal in each run
        #[arg(short, long)]
        verbose: bool,
    },
    /// Exercise the gates against a throwaway tree (used by `brew test`)
    Selftest,
}

struct Checkout {
    root: PathBuf,
    /// Why nothing in this checkout may be deleted: its activity could not be
    /// established completely, or it opted out.
    blocked: Option<&'static str>,
    newest: SystemTime,
}

#[derive(Default)]
struct Tree {
    checkouts: Vec<Checkout>,
    /// (generated dir, index of owning checkout)
    found: Vec<(PathBuf, usize)>,
    /// (canonical target of a generated-named symlink, index of linking checkout)
    links: Vec<(PathBuf, usize)>,
    /// Linked worktrees of discovered repos. They may live outside every root and
    /// still symlink into a candidate, so they get walked too (observe-only).
    worktrees: Vec<PathBuf>,
    unreadable: usize,
}

enum Verdict {
    Eligible,
    Kept(&'static str),
}

struct Candidate {
    path: PathBuf,
    /// Owning checkout plus every checkout that symlinks to `path`.
    users: Vec<PathBuf>,
    idle_days: u64,
    bytes: u64,
    verdict: Verdict,
}

fn bump(newest: &mut SystemTime, t: std::io::Result<SystemTime>) {
    if let Ok(t) = t
        && t > *newest
    {
        *newest = t;
    }
}

fn is_generated(name: &OsStr) -> bool {
    GENERATED.iter().any(|g| name == *g)
}

/// Newest write to the files git touches on checkout/commit/add/stash/rebase:
/// all of those rewrite the index or append to the HEAD reflog. Fetches and ref
/// packing (FETCH_HEAD, packed-refs) are background noise and deliberately not
/// counted. `None` when the git dir is unreadable, which keeps the whole checkout.
fn git_activity(checkout: &Path, dotgit: &fs::Metadata) -> Option<SystemTime> {
    let gitdir = if dotgit.is_dir() {
        checkout.join(".git")
    } else {
        // Linked worktree or submodule: `.git` is a file holding `gitdir: <path>`.
        let text = fs::read_to_string(checkout.join(".git")).ok()?;
        checkout.join(text.trim().strip_prefix("gitdir:")?.trim())
    };
    let mut newest = fs::metadata(gitdir.join("HEAD")).ok()?.modified().ok()?;
    for f in ["index", "logs/HEAD"] {
        if let Ok(md) = fs::metadata(gitdir.join(f)) {
            bump(&mut newest, md.modified());
        }
    }
    Some(newest)
}

/// Roots of the linked worktrees registered in a main repo's `.git/worktrees`.
fn linked_worktrees(gitdir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(gitdir.join("worktrees")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("gitdir")).ok())
        // The file holds the path of the worktree's `.git` file.
        .filter_map(|text| Path::new(text.trim()).parent().map(Path::to_path_buf))
        .filter_map(|wt| fs::canonicalize(wt).ok())
        .collect()
}

fn block(t: &mut Tree, current: Option<usize>, why: &'static str) {
    if let Some(c) = current {
        t.checkouts[c].blocked.get_or_insert(why);
    }
}

/// `observe` walks a tree for its activity and symlinks only; nothing found in it
/// becomes a candidate.
fn walk(dir: &Path, dev: u64, mut current: Option<usize>, observe: bool, t: &mut Tree) {
    if let Ok(md) = fs::symlink_metadata(dir.join(".git")) {
        let activity = git_activity(dir, &md);
        let blocked = if activity.is_none() {
            Some("unreadable git dir")
        } else if dir.join(".spaceman-keep").exists() {
            Some("opted out (.spaceman-keep)")
        } else {
            None
        };
        t.checkouts.push(Checkout {
            root: dir.to_path_buf(),
            blocked,
            newest: activity.unwrap_or(UNIX_EPOCH),
        });
        current = Some(t.checkouts.len() - 1);
        if md.is_dir() {
            t.worktrees.extend(linked_worktrees(&dir.join(".git")));
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        // Whatever happened in there is unknown, so the checkout is not idle.
        t.unreadable += 1;
        block(t, current, "unreadable subdirectory");
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            block(t, current, "unreadable subdirectory");
            continue;
        };
        let generated = is_generated(&entry.file_name());
        if md.file_type().is_symlink() {
            let Ok(target) = fs::canonicalize(&path) else {
                continue; // dangling
            };
            if generated {
                // A worktree that symlinks node_modules to another checkout's
                // copy keeps that copy alive for as long as the worktree is active.
                if let Some(c) = current {
                    t.links.push((target, c));
                }
            } else if let Some(c) = current
                && !target.starts_with(&t.checkouts[c].root)
            {
                match fs::metadata(&target) {
                    // Sources living behind the link are never walked.
                    Ok(md) if md.is_dir() => {
                        block(t, current, "symlinked directory leaves the checkout");
                    }
                    // A linked file is a source file like any other.
                    Ok(md) => bump(&mut t.checkouts[c].newest, md.modified()),
                    Err(_) => block(t, current, "unreadable subdirectory"),
                }
            }
            continue;
        }
        if md.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            if md.dev() != dev {
                block(t, current, "spans filesystems");
                continue;
            }
            if generated {
                if let Some(c) = current
                    && !observe
                {
                    t.found.push((path, c));
                }
                continue;
            }
        }
        if let Some(c) = current {
            bump(&mut t.checkouts[c].newest, md.modified());
        }
        if md.is_dir() {
            walk(&path, dev, current, observe, t);
        }
    }
}

/// Sum of allocated bytes this directory alone accounts for, and its newest
/// mtime. Hardlinked files (pnpm links into its global store) count as zero:
/// unlinking them frees nothing. Returns false if any part was unreadable.
fn measure(dir: &Path, bytes: &mut u64, newest: &mut SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut ok = true;
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else {
            ok = false;
            continue;
        };
        bump(newest, md.modified());
        if md.is_dir() {
            ok &= measure(&entry.path(), bytes, newest);
        } else if md.nlink() == 1 {
            *bytes += md.blocks() * 512;
        }
    }
    ok
}

fn git(checkout: &Path, args: &[&str], rel: &Path) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(checkout)
        // A repo's config must not get to run a command from a nightly job, and
        // "ignored" has to be the repo's own statement, not the user's global
        // excludes file.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.excludesFile=/dev/null",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .arg("--")
        .arg(rel)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stderr(Stdio::null())
        .output()
        .ok()
}

fn git_gate(checkout: &Path, path: &Path) -> Option<&'static str> {
    let Ok(rel) = path.strip_prefix(checkout) else {
        return Some("git failed");
    };
    match git(checkout, &["ls-files", "-z"], rel) {
        Some(o) if o.status.success() && o.stdout.is_empty() => {}
        Some(o) if o.status.success() => return Some("contains tracked files"),
        _ => return Some("git failed"),
    }
    match git(checkout, &["check-ignore", "-q"], rel).and_then(|o| o.status.code()) {
        Some(0) => None,
        Some(1) => Some("not gitignored"),
        _ => Some("git failed"),
    }
}

fn marker_gate(path: &Path) -> Option<&'static str> {
    match path.file_name().and_then(OsStr::to_str) {
        Some("target") => {
            (!path.join("CACHEDIR.TAG").is_file()).then_some("target without CACHEDIR.TAG")
        }
        Some(".venv" | "venv") => {
            if !path.join("pyvenv.cfg").is_file() {
                return Some("venv without pyvenv.cfg");
            }
            // A venv is only regenerable when something says what was in it.
            const RECIPES: &[&str] = &[
                "uv.lock",
                "poetry.lock",
                "Pipfile.lock",
                "pyproject.toml",
                "requirements.txt",
                "setup.py",
            ];
            let parent = path.parent()?;
            (!RECIPES.iter().any(|r| parent.join(r).is_file()))
                .then_some("venv without a lock or requirements file")
        }
        _ => None,
    }
}

/// What running processes hold: every path lsof can name (cwd, mapped files,
/// open fds) and every command line. The command lines matter because a service
/// started as `/proj/.venv/bin/python app.py` from another cwd holds nothing
/// open under `/proj` that lsof can see.
struct Busy {
    paths: Vec<PathBuf>,
    args: Vec<Vec<u8>>,
}

impl Busy {
    /// `None` unless both lsof and ps ran to completion: a partial process list
    /// must not pass for a complete one.
    fn snapshot() -> Option<Busy> {
        let run = |cmd: &str, args: &[&str]| {
            let out = Command::new(cmd)
                .args(args)
                .stderr(Stdio::null())
                .output()
                .ok()?;
            (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
        };
        let lsof = run("lsof", &["-n", "-P", "-w", "-F", "n"])?;
        let ps = run("ps", &["-axww", "-o", "pid=,args="])?;
        let me = std::process::id().to_string();
        Some(Busy {
            paths: lsof
                .split(|b| *b == b'\n')
                .filter_map(|l| l.strip_prefix(b"n/"))
                .map(|p| Path::new("/").join(OsStr::from_bytes(p)))
                .collect(),
            // spaceman's own command line names the roots; that is not a user.
            args: ps
                .split(|b| *b == b'\n')
                .filter(|l| {
                    let line = l.trim_ascii_start();
                    !(line.starts_with(me.as_bytes())
                        && line.get(me.len()).is_some_and(u8::is_ascii_whitespace))
                })
                .map(<[u8]>::to_vec)
                .collect(),
        })
    }

    fn uses(&self, users: &[PathBuf]) -> bool {
        users.iter().any(|u| {
            let mut needle = u.as_os_str().as_bytes().to_vec();
            needle.push(b'/');
            self.paths.iter().any(|p| p.starts_with(u))
                || self.args.iter().any(|a| {
                    a.windows(needle.len()).any(|w| w == needle)
                        || a.ends_with(&needle[..needle.len() - 1])
                })
        })
    }
}

fn idle_days(now: SystemTime, newest: SystemTime) -> u64 {
    now.duration_since(newest).map_or(0, |d| d.as_secs() / DAY)
}

fn plan(
    roots: &[PathBuf],
    now: SystemTime,
    days: u64,
    busy: Option<&Busy>,
) -> (Vec<Candidate>, usize) {
    let mut tree = Tree::default();
    for root in roots {
        if let Ok(md) = fs::metadata(root) {
            walk(root, md.dev(), None, false, &mut tree);
        }
    }
    let seen: Vec<PathBuf> = tree.checkouts.iter().map(|c| c.root.clone()).collect();
    for wt in std::mem::take(&mut tree.worktrees) {
        if !seen.contains(&wt)
            && let Ok(md) = fs::metadata(&wt)
        {
            walk(&wt, md.dev(), None, true, &mut tree);
        }
    }
    let mut out = Vec::new();
    for (path, c) in &tree.found {
        let own = &tree.checkouts[*c];
        let mut users = vec![own.root.clone()];
        let mut newest = own.newest;
        let mut blocked = own.blocked;
        for (target, l) in &tree.links {
            if target == path {
                let linker = &tree.checkouts[*l];
                users.push(linker.root.clone());
                blocked = blocked.or(linker.blocked);
                newest = newest.max(linker.newest);
            }
        }
        let mut cand = Candidate {
            path: path.clone(),
            users,
            idle_days: idle_days(now, newest),
            bytes: 0,
            verdict: Verdict::Eligible,
        };
        let kept = if let Some(why) = blocked {
            Some(why)
        } else if cand.idle_days < days {
            Some("checkout active")
        } else if busy.is_none() {
            Some("in-use check unavailable (lsof/ps)")
        } else if busy.is_some_and(|b| b.uses(&cand.users)) {
            Some("process running in checkout")
        } else if let Some(why) = marker_gate(path) {
            Some(why)
        } else if let Some(why) = git_gate(&own.root, path) {
            Some(why)
        } else {
            let mut inside = newest;
            let readable = measure(path, &mut cand.bytes, &mut inside);
            cand.idle_days = idle_days(now, inside);
            if !readable {
                Some("unreadable contents")
            } else if cand.idle_days < days {
                Some("recent writes inside")
            } else {
                None
            }
        };
        if let Some(why) = kept {
            cand.verdict = Verdict::Kept(why);
        }
        out.push(cand);
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    (out, tree.unreadable)
}

/// Delete eligible candidates. Planning a big tree takes minutes, so nothing is
/// deleted on the strength of that plan alone: candidates are grouped by the
/// checkouts that use them, and right before a group goes, those checkouts are
/// planned again from scratch against a fresh process snapshot. Only what is
/// still eligible in that second plan is removed.
fn execute(
    cands: &[Candidate],
    days: u64,
    ledger: &Path,
    clock: &dyn Fn() -> SystemTime,
    snapshot: &dyn Fn() -> Option<Busy>,
) -> Result<(u64, usize)> {
    let mut log = open_ledger(ledger)?;
    let (mut freed, mut failed) = (0, 0);
    // Ordered on purpose: a checkout sorts before the checkouts nested in it, and
    // deleting inside a nested checkout touches a directory the outer one counts
    // as its own activity. Outer first, or the outer group would skip itself.
    let mut groups: BTreeMap<&[PathBuf], Vec<&Candidate>> = BTreeMap::new();
    for c in cands
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Eligible))
    {
        groups.entry(&c.users).or_default().push(c);
    }
    for (users, group) in groups {
        let busy = snapshot();
        if busy.is_none() {
            eprintln!(
                "skip  {} (process list unavailable: lsof/ps failed)",
                users[0].display()
            );
            failed += 1;
            continue;
        }
        let (fresh, _) = plan(users, clock(), days, busy.as_ref());
        for c in group {
            let still = fresh
                .iter()
                .any(|f| f.path == c.path && matches!(f.verdict, Verdict::Eligible));
            let is_dir = fs::symlink_metadata(&c.path).is_ok_and(|md| md.is_dir());
            if !still || !is_dir || !c.path.file_name().is_some_and(is_generated) {
                println!("skip  {} (no longer eligible)", c.path.display());
                continue;
            }
            let result = fs::remove_dir_all(&c.path);
            let mut entry = serde_json::json!({
                "ts": rfc3339(SystemTime::now()),
                "kind": "generated-dir",
                "path": c.path.to_string_lossy(),
                "bytes": c.bytes,
                "idle_days": c.idle_days,
            });
            match result {
                Ok(()) => {
                    freed += c.bytes;
                    println!("rm    {:>9}  {}", human(c.bytes), c.path.display());
                }
                Err(e) => {
                    failed += 1;
                    entry["error"] = e.to_string().into();
                    eprintln!("FAIL  {}: {e}", c.path.display());
                }
            }
            writeln!(log, "{entry}").context("write ledger")?;
        }
    }
    Ok((freed, failed))
}

/// The last ledger line of a `run` that got through planning, written even when
/// nothing was deleted, so `log` can tell a quiet night from a night that never
/// ran. Runs that fail earlier (no roots, lock held) leave no line.
fn record_run(
    log: &mut File,
    started: SystemTime,
    failed: usize,
    refused: Option<&str>,
) -> Result<()> {
    let mut entry = serde_json::json!({
        "ts": rfc3339(SystemTime::now()),
        "kind": "run",
        "started": rfc3339(started),
        "failed": failed,
    });
    if let Some(why) = refused {
        entry["refused"] = why.into();
    }
    writeln!(log, "{entry}").context("write ledger")
}

fn open_ledger(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open ledger {}", path.display()))
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// UTC timestamp without a date crate (days-to-civil from Howard Hinnant).
fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / DAY) as i64, secs % DAY);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    let base = match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home()?.join(fallback),
    };
    Ok(base.join("spaceman"))
}

fn resolve_roots(cli: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let file = xdg("XDG_CONFIG_HOME", ".config")?.join("roots");
    let mut raw: Vec<PathBuf> = cli.to_vec();
    if raw.is_empty()
        && let Ok(text) = fs::read_to_string(&file)
    {
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            raw.push(match line.strip_prefix("~/") {
                Some(rest) => home()?.join(rest),
                None => PathBuf::from(line),
            });
        }
    }
    if raw.is_empty() {
        bail!(
            "no roots configured, nothing to do.\n\
             Pass --root DIR, or list one directory per line in {}",
            file.display()
        );
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for r in raw {
        // Canonical roots make every discovered path comparable with lsof output
        // and with canonicalized symlink targets.
        let canon = match fs::canonicalize(&r) {
            Ok(c) if c.is_dir() && c != Path::new("/") => c,
            // One dead line must not stop the other roots from being cleaned.
            _ => {
                eprintln!("warning: skipping root {}", r.display());
                continue;
            }
        };
        if canon.ancestors().any(|a| a.join(".git").exists()) && !canon.join(".git").exists() {
            eprintln!(
                "warning: root {} is inside a git checkout; checkouts are only \
                 recognised from their top directory, so nothing will be found there",
                canon.display()
            );
        }
        roots.push(canon);
    }
    // A root nested in another root would report everything twice.
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        bail!("none of the configured roots is a usable directory");
    }
    Ok(roots)
}

fn report(cands: &[Candidate], unreadable: usize, all: bool) {
    let mut total = 0;
    let mut kept: BTreeMap<&str, usize> = BTreeMap::new();
    for c in cands {
        match c.verdict {
            Verdict::Eligible => {
                total += c.bytes;
                println!(
                    "{:>9}  idle {:>4}d  {}",
                    human(c.bytes),
                    c.idle_days,
                    c.path.display()
                );
            }
            Verdict::Kept(why) => *kept.entry(why).or_default() += 1,
        }
    }
    let eligible = cands.len() - kept.values().sum::<usize>();
    println!("\n{eligible} eligible, {} reclaimable", human(total));
    for (why, n) in &kept {
        println!("kept {n:>4}  {why}");
    }
    if unreadable > 0 {
        println!("{unreadable} unreadable directories skipped");
    }
    if all {
        println!();
        for c in cands {
            if let Verdict::Kept(why) = c.verdict {
                println!("kept  {}  ({why})", c.path.display());
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let all = match cli.cmd {
        Some(Cmd::Selftest) => return selftest(),
        Some(Cmd::Log { last, verbose }) => {
            let ledger = xdg("XDG_STATE_HOME", ".local/state")?.join("ledger.jsonl");
            let last = usize::try_from(last).unwrap_or(usize::MAX);
            return history::show(&ledger, last, verbose);
        }
        Some(Cmd::Scan { all }) => all,
        _ => false,
    };
    // The directory collector needs roots; --docker / --caches can run without.
    let roots = match resolve_roots(&cli.roots) {
        Ok(roots) => roots,
        Err(_) if cli.docker || cli.caches => Vec::new(),
        Err(e) => return Err(e),
    };
    let deleting = matches!(cli.cmd, Some(Cmd::Run));
    let now = SystemTime::now();
    let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());

    let state = xdg("XDG_STATE_HOME", ".local/state")?;
    let seen_path = state.join("docker-state.json");
    let _lock = if deleting {
        fs::create_dir_all(&state).with_context(|| format!("create {}", state.display()))?;
        let lock = File::create(state.join("lock")).context("create lock file")?;
        if lock.try_lock().is_err() {
            bail!(
                "another spaceman run holds {}",
                state.join("lock").display()
            );
        }
        Some(lock)
    } else {
        None
    };

    let busy = Busy::snapshot();
    let (cands, unreadable) = plan(&roots, now, cli.days, busy.as_ref());
    // None: not asked for, or no daemon (normal at night). Some(None): the daemon
    // answered but the answers made no sense, which is a failure.
    let docker_plan = (cli.docker && docker::reachable()).then(|| {
        let plan = docker::query()
            .and_then(|r| docker::decide(&r, now_secs, cli.days, docker::load_seen(&seen_path)))?;
        // Bookkeeping, not deletion, so `scan` does it too: otherwise the clocks
        // never start and a scan could never preview anything.
        let saved = fs::create_dir_all(&state).is_ok() && docker::save_seen(&seen_path, &plan.seen);
        saved.then_some(plan)
    });
    if cli.docker && docker_plan.is_none() {
        println!("docker: daemon not reachable, skipped");
    }
    let cache_plan = if cli.caches {
        caches::plan(&home()?, now, cli.days)
    } else {
        Vec::new()
    };

    if !deleting {
        if !roots.is_empty() {
            report(&cands, unreadable, all);
        }
        match &docker_plan {
            Some(Some(p)) => docker::report(p),
            Some(None) => println!("docker: could not read or record docker state"),
            None => {}
        }
        if cli.caches {
            let (files, bytes) = cache_plan
                .iter()
                .fold((0, 0), |(f, b), s| (f + s.files.len(), b + s.bytes));
            println!("browser cache: {files} stale files, {}", human(bytes));
        }
        return Ok(());
    }

    let ledger = state.join("ledger.jsonl");
    let (mut freed, mut failed) = (0, 0);
    let eligible = cands
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Eligible))
        .count();
    let docker_items = match &docker_plan {
        Some(Some(p)) => p.items.len(),
        _ => 0,
    };
    if eligible > cli.max_items || docker_items > cli.max_items {
        let why = format!(
            "{eligible} directories and {docker_items} docker objects are eligible, more \
             than --max-items {}",
            cli.max_items
        );
        record_run(&mut open_ledger(&ledger)?, now, 0, Some(&why))?;
        bail!(
            "{why}. Nothing was deleted. Read `spaceman scan`, then rerun with a higher \
             --max-items."
        );
    }
    if !roots.is_empty() {
        (freed, failed) = execute(&cands, cli.days, &ledger, &SystemTime::now, &Busy::snapshot)?;
    }
    let mut log = open_ledger(&ledger)?;
    for stale in &cache_plan {
        let (bytes, errors) = caches::execute(stale);
        freed += bytes;
        failed += errors;
        println!(
            "rm    {:>9}  {} ({} files)",
            human(bytes),
            stale.dir.display(),
            stale.files.len()
        );
        writeln!(
            log,
            "{}",
            serde_json::json!({
                "ts": rfc3339(SystemTime::now()),
                "kind": "browser-cache",
                "path": stale.dir.to_string_lossy(),
                "files": stale.files.len(),
                "bytes": bytes,
            })
        )
        .context("write ledger")?;
    }
    println!("freed {}", human(freed));
    match &docker_plan {
        Some(Some(p)) => failed += docker::execute(p, cli.days, &mut log),
        Some(None) => {
            eprintln!("docker: could not read or record docker state");
            failed += 1;
        }
        None => {}
    }
    record_run(&mut log, now, failed, None)?;
    if failed > 0 {
        bail!("{failed} problems, see the lines above");
    }
    Ok(())
}

fn selftest() -> Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let base = fs::canonicalize(std::env::temp_dir())?
        .join(format!("spaceman-selftest-{}-{nanos}", std::process::id()));
    // Exclusive create: never build the fixture inside something that was
    // already sitting at that name.
    fs::create_dir(&base).with_context(|| format!("create {}", base.display()))?;
    let result = selftest_in(&base);
    // The fixture contains a mode-000 directory.
    let locked = base.join("roots/locked/private");
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
    let _ = fs::remove_dir_all(&base);
    result?;
    println!("spaceman selftest: OK");
    Ok(())
}

fn selftest_in(base: &Path) -> Result<()> {
    let put = |rel: &str, body: &str| -> Result<()> {
        let p = base.join(rel);
        fs::create_dir_all(p.parent().context("parent")?)?;
        fs::write(p, body)?;
        Ok(())
    };
    let sh = |dir: &str, args: &[&str]| -> Result<()> {
        let ok = Command::new("git")
            .arg("-C")
            .arg(base.join(dir))
            .args([
                "-c",
                "user.name=selftest",
                "-c",
                "user.email=selftest@invalid",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("selftest needs git on PATH")?
            .success();
        if !ok {
            bail!("selftest: git {args:?} failed in {dir}");
        }
        Ok(())
    };

    // good: ignored node_modules, a marked target, an unmarked venv.
    put("roots/good/.gitignore", "node_modules/\ntarget/\n.venv/\n")?;
    put("roots/good/src/a.js", "x")?;
    put("roots/good/node_modules/pkg/index.js", "x")?;
    put(
        "roots/good/target/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55",
    )?;
    put("roots/good/.venv/lib/site.py", "x")?;
    sh("roots/good", &["init", "-q"])?;
    // py: a real-looking venv with and without a recipe to rebuild it from.
    put("roots/py/.gitignore", ".venv/\n")?;
    put("roots/py/.venv/pyvenv.cfg", "home = /usr/bin")?;
    put("roots/py/requirements.txt", "requests")?;
    sh("roots/py", &["init", "-q"])?;
    put("roots/pybare/.gitignore", ".venv/\n")?;
    put("roots/pybare/.venv/pyvenv.cfg", "home = /usr/bin")?;
    sh("roots/pybare", &["init", "-q"])?;
    // unignored: node_modules git does not ignore.
    put("roots/unignored/node_modules/x.js", "x")?;
    sh("roots/unignored", &["init", "-q"])?;
    // tracked: ignored node_modules that still holds a tracked file.
    put("roots/tracked/.gitignore", "node_modules/\n")?;
    put("roots/tracked/node_modules/x.js", "x")?;
    sh("roots/tracked", &["init", "-q"])?;
    sh("roots/tracked", &["add", "-f", "node_modules/x.js"])?;
    // loose: not in a checkout at all.
    put("roots/loose/node_modules/x.js", "x")?;
    // locked: a subdirectory the walk cannot read.
    put("roots/locked/.gitignore", "node_modules/\n")?;
    put("roots/locked/node_modules/x.js", "x")?;
    put("roots/locked/private/secret", "x")?;
    sh("roots/locked", &["init", "-q"])?;
    fs::set_permissions(
        base.join("roots/locked/private"),
        fs::Permissions::from_mode(0o000),
    )?;
    // keep: opted out.
    put("roots/keep/.gitignore", "node_modules/\n")?;
    put("roots/keep/node_modules/x.js", "x")?;
    put("roots/keep/.spaceman-keep", "")?;
    sh("roots/keep", &["init", "-q"])?;
    // shared: its node_modules is symlinked from a linked worktree that lives
    // OUTSIDE the roots.
    put("roots/shared/.gitignore", "node_modules\n")?;
    put("roots/shared/node_modules/x.js", "x")?;
    sh("roots/shared", &["init", "-q"])?;
    sh("roots/shared", &["add", ".gitignore"])?;
    sh("roots/shared", &["commit", "-q", "-m", "init"])?;
    fs::create_dir_all(base.join("outside"))?;
    let wt = base.join("outside/wt");
    sh(
        "roots/shared",
        &["worktree", "add", "-q", "-b", "wt", &wt.to_string_lossy()],
    )?;
    std::os::unix::fs::symlink(
        base.join("roots/shared/node_modules"),
        wt.join("node_modules"),
    )?;

    let roots = [base.join("roots")];
    let expect = |cands: &[Candidate], rel: &str, want: Option<&'static str>| -> Result<()> {
        let c = cands
            .iter()
            .find(|c| c.path == base.join("roots").join(rel))
            .with_context(|| format!("selftest: {rel} was not discovered"))?;
        let got = match c.verdict {
            Verdict::Eligible => None,
            Verdict::Kept(why) => Some(why),
        };
        if got != want {
            bail!("selftest: {rel}: expected {want:?}, got {got:?}");
        }
        Ok(())
    };
    let idle = || Busy {
        paths: Vec::new(),
        args: Vec::new(),
    };

    let now = SystemTime::now();
    let later = now + Duration::from_secs(30 * DAY);

    let (fresh, _) = plan(&roots, now, 7, Some(&idle()));
    expect(&fresh, "good/node_modules", Some("checkout active"))?;

    let (blind, _) = plan(&roots, later, 7, None);
    expect(
        &blind,
        "good/node_modules",
        Some("in-use check unavailable (lsof/ps)"),
    )?;

    // A process whose cwd is in the out-of-roots worktree keeps the shared copy.
    let in_wt = Busy {
        paths: vec![wt.join("src")],
        args: Vec::new(),
    };
    let (linked, _) = plan(&roots, later, 7, Some(&in_wt));
    expect(
        &linked,
        "shared/node_modules",
        Some("process running in checkout"),
    )?;
    expect(&linked, "good/node_modules", None)?;

    // So does a command line that merely mentions the checkout.
    let by_args = Busy {
        paths: Vec::new(),
        args: vec![
            format!(
                "{}/.venv/bin/python app.py",
                base.join("roots/py").display()
            )
            .into_bytes(),
        ],
    };
    let (argv, _) = plan(&roots, later, 7, Some(&by_args));
    expect(&argv, "py/.venv", Some("process running in checkout"))?;

    let (stale, _) = plan(&roots, later, 7, Some(&idle()));
    expect(&stale, "good/node_modules", None)?;
    expect(&stale, "good/target", None)?;
    expect(&stale, "good/.venv", Some("venv without pyvenv.cfg"))?;
    expect(&stale, "py/.venv", None)?;
    expect(
        &stale,
        "pybare/.venv",
        Some("venv without a lock or requirements file"),
    )?;
    expect(&stale, "unignored/node_modules", Some("not gitignored"))?;
    expect(
        &stale,
        "tracked/node_modules",
        Some("contains tracked files"),
    )?;
    expect(
        &stale,
        "locked/node_modules",
        Some("unreadable subdirectory"),
    )?;
    expect(
        &stale,
        "keep/node_modules",
        Some("opted out (.spaceman-keep)"),
    )?;
    expect(&stale, "shared/node_modules", None)?;
    if stale
        .iter()
        .any(|c| !c.path.starts_with(base.join("roots")))
    {
        bail!("selftest: something outside the roots became a candidate");
    }
    if stale
        .iter()
        .any(|c| c.path.starts_with(base.join("roots/loose")))
    {
        bail!("selftest: a directory outside any checkout became a candidate");
    }

    // A plan that went stale must not be executed: by the time of the delete the
    // clock says "fresh", so nothing may go.
    let ledger = base.join("ledger.jsonl");
    execute(&stale, 7, &ledger, &|| now, &|| Some(idle()))?;
    if !base.join("roots/good/node_modules").exists() {
        bail!("selftest: execute trusted an outdated plan");
    }
    // Neither may anything go when the process snapshot fails at delete time.
    execute(&stale, 7, &ledger, &|| later, &|| None)?;
    if !base.join("roots/good/node_modules").exists() {
        bail!("selftest: execute deleted without an in-use check");
    }

    let (_, failed) = execute(&stale, 7, &ledger, &|| later, &|| Some(idle()))?;
    let gone = [
        "good/node_modules",
        "good/target",
        "py/.venv",
        "shared/node_modules",
    ];
    let stay = [
        "good/src/a.js",
        "good/.venv",
        "pybare/.venv",
        "tracked/node_modules/x.js",
        "unignored/node_modules/x.js",
        "locked/node_modules/x.js",
        "keep/node_modules/x.js",
        "loose/node_modules/x.js",
    ];
    if failed != 0
        || gone.iter().any(|p| base.join("roots").join(p).exists())
        || stay.iter().any(|p| !base.join("roots").join(p).exists())
    {
        bail!("selftest: execute deleted the wrong set");
    }
    if fs::read_to_string(&ledger)?.lines().count() != gone.len() {
        bail!(
            "selftest: ledger should hold exactly {} deletions",
            gone.len()
        );
    }
    if rfc3339(UNIX_EPOCH + Duration::from_secs(1_790_000_000)) != "2026-09-21T14:13:20Z"
        || rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800)) != "2024-02-29T00:00:00Z"
    {
        bail!("selftest: rfc3339 mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The selftest injects a synthetic process list; this one checks that the
    /// real lsof/ps parsing still finds a real process both ways.
    #[test]
    fn snapshot_sees_cwd_and_argv() {
        use std::process::{Command, Stdio};
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("spaceman-busy-{}", std::process::id()));
        let (by_cwd, by_argv) = (dir.join("cwd"), dir.join("argv"));
        std::fs::create_dir_all(&by_cwd).unwrap();
        std::fs::create_dir_all(&by_argv).unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read line", "sh"])
            .arg(by_argv.join("app.py"))
            .current_dir(&by_cwd)
            // Blocks on the pipe and forks nothing, so kill() leaves no orphan.
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let busy = super::Busy::snapshot();
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        // Production keeps everything when lsof/ps are degraded; that is not a
        // parsing bug, so it is not a test failure either.
        let Some(busy) = busy else {
            eprintln!("skipped: lsof/ps unavailable here");
            return;
        };
        assert!(busy.uses(std::slice::from_ref(&by_cwd)), "cwd not seen");
        assert!(busy.uses(std::slice::from_ref(&by_argv)), "argv not seen");
        assert!(!busy.uses(&[dir.join("nobody")]));
    }

    #[test]
    fn selftest_passes() {
        super::selftest().unwrap();
    }
}
