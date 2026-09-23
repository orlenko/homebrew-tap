//! `spaceman log`: the ledger read back, one line per run.
//!
//! A `run` that gets through planning ends with a `"kind":"run"` record (when
//! it started, how many problems it hit, whether --max-items refused it), so a
//! night that deleted nothing still shows up. Entries with no run record —
//! ledgers from 0.1, or a run that died before its last line — are grouped by
//! the gaps between them.
//!
//! "freed" counts only what spaceman measured itself: generated dirs and
//! browser-cache entries. Docker reports image sizes including layers other
//! images still share, so docker removals are counted, not summed.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

use crate::human;

/// Deletions further apart than this belong to different runs when no run
/// record ties them together. Runs are nightly; one run's deletions land within
/// a few minutes unless the machine sleeps mid-run.
const GAP_SECS: i64 = 10 * 60;

struct Run {
    record: Option<Value>,
    items: Vec<Value>,
}

impl Run {
    fn start(&self) -> Option<i64> {
        self.record
            .as_ref()
            .and_then(|r| ts(r, "started"))
            .or_else(|| self.items.first().and_then(|i| ts(i, "ts")))
    }
}

fn ts(v: &Value, key: &str) -> Option<i64> {
    v[key].as_str().and_then(parse_rfc3339)
}

fn failed(item: &Value) -> bool {
    item.get("error").is_some() || item["ok"] == false
}

/// Inverse of `crate::rfc3339`: only the exact `YYYY-MM-DDTHH:MM:SSZ` shape it
/// writes.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || [b[4], b[7], b[10], b[13], b[16], b[19]] != *b"--T::Z" {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Local wall-clock time, so the log reads in the same clock as the schedule.
fn local(secs: i64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: `tm` is plain old data; localtime_r writes only through the two
    // pointers it is given, both valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return format!("@{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn by_gap(items: Vec<Value>, out: &mut Vec<Run>) {
    let mut last: Option<i64> = None;
    for item in items {
        let t = ts(&item, "ts");
        let split = match (last, t) {
            (Some(a), Some(b)) => b - a > GAP_SECS,
            _ => last.is_none(),
        };
        if split {
            out.push(Run {
                record: None,
                items: Vec::new(),
            });
        }
        out.last_mut().unwrap().items.push(item);
        last = t.or(last);
    }
}

fn group(entries: Vec<Value>) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    for e in entries {
        if e["kind"] != "run" {
            pending.push(e);
            continue;
        }
        // What was logged well before this run started belongs to an earlier
        // run that left no record. The slack absorbs a clock stepped back
        // during planning (NTP right after a wake).
        let started = ts(&e, "started");
        let (mine, older): (Vec<_>, Vec<_>) = pending
            .drain(..)
            .partition(|p| started.is_none_or(|s| ts(p, "ts").is_none_or(|t| t >= s - GAP_SECS)));
        by_gap(older, &mut runs);
        runs.push(Run {
            record: Some(e),
            items: mine,
        });
    }
    by_gap(pending, &mut runs);
    runs
}

/// Display order of the per-kind counts, singular and plural.
const NOUNS: [(&str, &str); 4] = [
    ("dir", "dirs"),
    ("cache dir", "cache dirs"),
    ("docker object", "docker objects"),
    ("other", "other"),
];

fn noun(kind: &str) -> usize {
    match kind {
        "generated-dir" => 0,
        "browser-cache" => 1,
        k if k.starts_with("docker-") => 2,
        _ => 3,
    }
}

fn summary(run: &Run) -> String {
    let mut freed = 0;
    let mut problems = 0;
    let mut kinds = [0usize; NOUNS.len()];
    for i in &run.items {
        if failed(i) {
            problems += 1;
        } else {
            let k = noun(i["kind"].as_str().unwrap_or(""));
            if k < 2 {
                freed += i["bytes"].as_u64().unwrap_or(0);
            }
            kinds[k] += 1;
        }
    }
    let when = run.start().map_or_else(|| "????-??-?? ??:??".into(), local);
    if let Some(r) = &run.record {
        if let Some(why) = r["refused"].as_str() {
            return format!("{when}  REFUSED  {why}");
        }
        // The record also counts problems that removed nothing (lsof/ps down,
        // docker state unreadable), which the items cannot show.
        problems = r["failed"].as_u64().map_or(problems, |n| n as usize);
    }
    let what = NOUNS
        .iter()
        .zip(kinds)
        .filter(|(_, n)| *n > 0)
        .map(|((one, many), n)| format!("{n} {}", if n == 1 { one } else { many }))
        .collect::<Vec<_>>()
        .join(", ");
    let what = if what.is_empty() {
        "nothing removed".into()
    } else {
        what
    };
    let mut line = format!("{when}  freed {:>9}  {what}", human(freed));
    if problems > 0 {
        line += &format!("  {problems} FAILED");
    }
    if run.record.is_none() {
        line += "  (no run record)";
    }
    line
}

fn detail(item: &Value) -> String {
    let kind = item["kind"].as_str().unwrap_or("?");
    let what = item["path"]
        .as_str()
        .or(item["label"].as_str())
        .unwrap_or("?");
    let size = human(item["bytes"].as_u64().unwrap_or(0));
    let extra = match item["files"].as_u64() {
        Some(n) => format!(" ({n} files)"),
        None if kind.starts_with("docker-") => format!(" [{kind}]"),
        None => String::new(),
    };
    match item["error"].as_str() {
        Some(e) => format!("  FAIL  {what}{extra}: {e}"),
        None if failed(item) => format!("  FAIL  {what}{extra}"),
        None => format!("  rm    {size:>9}  {what}{extra}"),
    }
}

pub fn show(ledger: &Path, last: usize, verbose: bool) -> Result<()> {
    // Lossy: a line torn mid-character (killed, or ENOSPC, which is when this
    // tool runs) must cost that line, not the whole history.
    let text = match std::fs::read(ledger) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no runs recorded yet ({} does not exist)", ledger.display());
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", ledger.display())),
    };
    let mut bad = 0;
    let entries: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).map_err(|_| bad += 1).ok())
        .collect();
    let runs = group(entries);
    for run in &runs[runs.len().saturating_sub(last)..] {
        println!("{}", summary(run));
        if verbose {
            for i in &run.items {
                println!("{}", detail(i));
            }
        }
    }
    if runs.is_empty() {
        println!("no runs recorded yet");
    }
    if bad > 0 {
        eprintln!("warning: {bad} unreadable lines in {}", ledger.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> String {
        crate::rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn parse_inverts_rfc3339() {
        for secs in [0, 951_782_400, 1_790_149_018, 4_107_542_400] {
            assert_eq!(parse_rfc3339(&at(secs)), Some(secs as i64), "{}", at(secs));
        }
        for bad in [
            "",
            "2026-09-23 11:43:46Z",
            "2026-09-23T11:43:46",
            "garbage-in-20-chars!",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn groups_by_record_then_by_gap() {
        let t0 = 1_790_000_000;
        let dir = |t: u64| json!({"ts": at(t), "kind": "generated-dir", "bytes": 1});
        let entries = vec![
            // 0.1 ledger: two runs, told apart by the gap only.
            dir(t0),
            dir(t0 + 60),
            dir(t0 + 3 * 3600),
            // A run whose deletions landed hours after it started (slept mid-run).
            dir(t0 + 30 * 3600),
            json!({"ts": at(t0 + 30 * 3600 + 5), "kind": "run", "started": at(t0 + 26 * 3600), "failed": 0}),
            // A run that deleted nothing still counts.
            json!({"ts": at(t0 + 50 * 3600), "kind": "run", "started": at(t0 + 50 * 3600), "failed": 0}),
            // Died before writing its record.
            dir(t0 + 74 * 3600),
        ];
        let runs = group(entries);
        let shape: Vec<(bool, usize)> = runs
            .iter()
            .map(|r| (r.record.is_some(), r.items.len()))
            .collect();
        assert_eq!(
            shape,
            [(false, 2), (false, 1), (true, 1), (true, 0), (false, 1)]
        );
        assert_eq!(runs[2].start(), Some(t0 as i64 + 26 * 3600));
        assert!(summary(&runs[3]).contains("nothing removed"));
    }

    #[test]
    fn record_overrides_item_failures_and_shows_refusals() {
        let run = Run {
            record: Some(json!({"kind": "run", "started": at(0), "failed": 3})),
            items: vec![
                json!({"ts": at(0), "kind": "generated-dir", "bytes": 2048, "error": "EPERM"}),
            ],
        };
        assert!(summary(&run).contains("3 FAILED"));
        let refused = Run {
            record: Some(
                json!({"kind": "run", "started": at(0), "failed": 0, "refused": "too many"}),
            ),
            items: vec![],
        };
        assert!(summary(&refused).contains("REFUSED  too many"));
    }
}
