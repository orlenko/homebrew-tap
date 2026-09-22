//! Docker collector, enabled with `--docker`.
//!
//! Docker's own `--filter until=` on images and containers compares the *build*
//! or *create* date, so a base image built upstream last year looks ancient the
//! minute it is pulled, and `docker volume prune` has no age filter at all. This
//! collector applies `--days` as "unused for that long" to everything:
//!
//! - a stopped container goes once it has been stopped for `days`, together with
//!   its anonymous volumes (`docker rm -v`). That is more than `docker system
//!   prune` does by default; it is `system prune --volumes` for that container;
//! - an image goes once no container, running or stopped, has referenced it for
//!   `days`;
//! - a dangling anonymous volume goes once it has been dangling for `days`;
//! - build cache unused for `days` goes through `docker builder prune`, whose
//!   `until` filter does mean "unused".
//!
//! Docker records neither when an image was last used nor when a volume became
//! dangling, so spaceman keeps `docker-state.json`: the time each was last seen
//! in use. `scan` and `run` both update it; something new counts as used the
//! first time it is seen, so nothing is removed during the first `days`.
//!
//! Named volumes are never touched and nothing is forced. `docker rmi <tag>`
//! happily untags an image that is in use, so right before an image goes its
//! containers are looked up once more and any hit leaves every tag in place.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::{DAY, human, rfc3339};

/// Label the engine (23+) puts on volumes it created without a name.
const ANONYMOUS: &str = "com.docker.volume.anonymous";

pub struct Item {
    pub kind: &'static str,
    pub id: String,
    /// What `docker rm`/`rmi`/`volume rm` is called with, one call per entry.
    pub targets: Vec<String>,
    pub label: String,
    pub bytes: u64,
    pub idle_days: u64,
}

/// id -> epoch seconds the image or volume was last seen in use.
#[derive(Default, Clone, PartialEq, Debug)]
pub struct Seen {
    pub images: BTreeMap<String, u64>,
    pub volumes: BTreeMap<String, u64>,
}

pub struct Plan {
    pub items: Vec<Item>,
    /// Unused images and volumes that have not been unused for long enough yet.
    pub waiting: usize,
    pub seen: Seen,
}

/// Raw `docker inspect` rows, tab separated; see `query` for the columns.
#[derive(Default)]
pub struct Rows {
    pub containers: String,
    pub images: String,
    pub volumes: String,
}

fn docker(args: &[&str]) -> Option<String> {
    let out = Command::new("docker")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn reachable() -> bool {
    docker(&["version", "--format", "{{.Server.Version}}"]).is_some()
}

fn inspect(kind: &str, list: &[&str], format: &str) -> Option<String> {
    let ids = docker(list)?;
    let ids: BTreeSet<&str> = ids.lines().filter(|l| !l.is_empty()).collect();
    if ids.is_empty() {
        return Some(String::new());
    }
    let mut args = vec![kind, "inspect", "--format", format];
    args.extend(ids);
    docker(&args)
}

/// `None` when any query fails, e.g. because a container vanished between the
/// listing and the inspect: no partial plans.
pub fn query() -> Option<Rows> {
    Some(Rows {
        containers: inspect(
            "container",
            &["ps", "-aq", "--no-trunc"],
            "{{.Id}}\t{{.Image}}\t{{.State.Status}}\t{{.State.FinishedAt}}\t{{.Created}}\t{{.Name}}",
        )?,
        images: inspect(
            "image",
            &["image", "ls", "-q", "--no-trunc"],
            "{{.Id}}\t{{.Size}}\t{{json .RepoTags}}",
        )?,
        // Dangling = referenced by no container, running or stopped.
        volumes: inspect(
            "volume",
            &["volume", "ls", "-q", "--filter", "dangling=true"],
            "{{.Name}}\t{{json .Labels}}",
        )?,
    })
}

/// Seconds since the epoch for docker's `2026-09-13T04:12:33.123456789Z`.
/// Anything else (offsets, the zero date) is `None`, which keeps the container.
pub fn parse_utc(s: &str) -> Option<u64> {
    let s = s.trim();
    if !s.ends_with('Z') || s.len() < 20 || !s.is_char_boundary(19) {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if y < 1970 || !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60
    {
        return None;
    }
    // days-from-civil (Howard Hinnant), the inverse of `rfc3339`.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * DAY as i64 + hh * 3600 + mm * 60 + ss).ok()
}

/// A missing, truncated or foreign state file is an empty one: every clock
/// restarts, which only ever delays removals.
pub fn load_seen(path: &Path) -> Seen {
    let parsed: Option<Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let map = |key: &str| {
        parsed
            .as_ref()
            .and_then(|v| serde_json::from_value(v.get(key)?.clone()).ok())
            .unwrap_or_default()
    };
    Seen {
        images: map("images"),
        volumes: map("volumes"),
    }
}

/// Atomic, so a `scan` and a `run` writing at once cannot leave half a file.
pub fn save_seen(path: &Path, seen: &Seen) -> bool {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let body = json!({ "images": seen.images, "volumes": seen.volumes }).to_string();
    let saved = std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_ok();
    if !saved {
        let _ = std::fs::remove_file(&tmp);
    }
    saved
}

/// Age of `id` on a last-seen-in-use clock; starts the clock when `id` is new.
fn unused_days(clock: &mut BTreeMap<String, u64>, id: &str, now: u64) -> u64 {
    now.saturating_sub(*clock.entry(id.to_string()).or_insert(now)) / DAY
}

fn fields(rows: &str) -> Vec<Vec<&str>> {
    rows.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').collect())
        .collect()
}

/// The pure half of planning. `None` when the rows do not look as expected.
pub fn decide(rows: &Rows, now: u64, days: u64, mut seen: Seen) -> Option<Plan> {
    let mut items = Vec::new();
    let mut waiting = 0;

    let mut used = BTreeSet::new();
    for f in fields(&rows.containers) {
        let [id, image, status, finished, created, name] = f[..] else {
            return None;
        };
        used.insert(image);
        if !matches!(status, "exited" | "dead" | "created") {
            continue;
        }
        // A container that never ran has a zero FinishedAt; fall back to Created.
        let Some(since) = parse_utc(finished).or_else(|| parse_utc(created)) else {
            continue;
        };
        let idle_days = now.saturating_sub(since) / DAY;
        if idle_days >= days {
            items.push(Item {
                kind: "docker-container",
                id: id.to_string(),
                targets: vec![id.to_string()],
                label: format!("{} ({status})", name.trim_start_matches('/')),
                bytes: 0,
                idle_days,
            });
        }
    }

    let images = fields(&rows.images);
    let listed: BTreeSet<&str> = images.iter().filter_map(|f| f.first().copied()).collect();
    // Under the containerd image store a container's .Image is not an id that
    // `image ls` prints. Then "unreferenced" means nothing, so stop.
    if !used.is_empty() && used.is_disjoint(&listed) {
        return None;
    }
    for f in &images {
        let [id, size, tags] = f[..] else {
            return None;
        };
        if used.contains(id) {
            seen.images.insert(id.to_string(), now);
            continue;
        }
        let idle_days = unused_days(&mut seen.images, id, now);
        if idle_days < days {
            waiting += 1;
            continue;
        }
        let tags: Vec<String> = serde_json::from_str(tags).unwrap_or_default();
        items.push(Item {
            kind: "docker-image",
            id: id.to_string(),
            label: if tags.is_empty() {
                "<untagged>".into()
            } else {
                tags.join(" ")
            },
            // By tag when tagged: `rmi <id>` refuses an image with several tags.
            targets: if tags.is_empty() {
                vec![id.to_string()]
            } else {
                tags
            },
            bytes: size.parse().unwrap_or(0),
            idle_days,
        });
    }
    seen.images.retain(|id, _| listed.contains(id.as_str()));

    let mut dangling = BTreeSet::new();
    for f in fields(&rows.volumes) {
        let [name, labels] = f[..] else {
            return None;
        };
        // No label, no proof it is anonymous: treat it as named and leave it.
        let anonymous = serde_json::from_str::<Value>(labels)
            .ok()
            .is_some_and(|l| l.get(ANONYMOUS).is_some());
        if !anonymous {
            continue;
        }
        dangling.insert(name);
        let idle_days = unused_days(&mut seen.volumes, name, now);
        if idle_days < days {
            waiting += 1;
            continue;
        }
        items.push(Item {
            kind: "docker-volume",
            id: name.to_string(),
            targets: vec![name.to_string()],
            label: format!("anonymous {}", name.get(..12).unwrap_or(name)),
            bytes: 0,
            idle_days,
        });
    }
    // A volume that is in use again is no longer listed; its clock restarts.
    seen.volumes
        .retain(|name, _| dangling.contains(name.as_str()));

    Some(Plan {
        items,
        waiting,
        seen,
    })
}

pub fn report(p: &Plan) {
    for i in &p.items {
        let size = if i.bytes > 0 {
            human(i.bytes)
        } else {
            "-".into()
        };
        println!(
            "{size:>9}  unused {:>4}d  {} {}",
            i.idle_days, i.kind, i.label
        );
    }
    let total: u64 = p.items.iter().map(|i| i.bytes).sum();
    println!(
        "docker: {} removable (images up to {}; shared layers make that an upper bound), \
         {} unused but inside the window",
        p.items.len(),
        human(total),
        p.waiting
    );
    // `run` also prunes build cache; docker only tells how much exists in total.
    let cache = docker(&["system", "df", "--format", "{{.Type}}\t{{.Size}}"]);
    if let Some(size) = cache
        .iter()
        .flat_map(|out| out.lines())
        .find_map(|l| l.strip_prefix("Build Cache\t"))
    {
        println!("docker: build cache {size} in total; the part unused for --days goes too");
    }
}

/// Returns the number of failed removals.
pub fn execute(p: &Plan, days: u64, ledger: &mut File) -> usize {
    execute_with(p, days, ledger, &docker)
}

fn execute_with(
    p: &Plan,
    days: u64,
    ledger: &mut File,
    docker: &dyn Fn(&[&str]) -> Option<String>,
) -> usize {
    let mut failed = 0;
    for i in &p.items {
        let verb: &[&str] = match i.kind {
            // -v takes the container's anonymous volumes with it. No -f: a
            // container that was started since planning makes docker refuse.
            "docker-container" => &["rm", "-v"],
            "docker-image" => &["rmi"],
            _ => &["volume", "rm"],
        };
        if i.kind == "docker-image" {
            // `rmi <tag>` untags without complaint even under a running
            // container, so "docker will refuse" is not a gate for images.
            let ancestor = format!("ancestor={}", i.id);
            let users = docker(&["ps", "-aq", "--filter", &ancestor]);
            if users.is_none_or(|out| !out.trim().is_empty()) {
                println!("skip  docker-image {} (in use again)", i.label);
                continue;
            }
        }
        let ok = i.targets.iter().all(|t| {
            let mut args = verb.to_vec();
            args.push(t);
            docker(&args).is_some()
        });
        println!(
            "{}  {} {}",
            if ok { "rm  " } else { "FAIL" },
            i.kind,
            i.label
        );
        failed += usize::from(!ok);
        let _ = writeln!(
            ledger,
            "{}",
            json!({
                "ts": rfc3339(std::time::SystemTime::now()),
                "kind": i.kind,
                "id": i.id,
                "label": i.label,
                "bytes": i.bytes,
                "idle_days": i.idle_days,
                "ok": ok,
            })
        );
    }
    let until = format!("until={}h", days.saturating_mul(24));
    match docker(&["builder", "prune", "-f", "--filter", &until]) {
        Some(out) => {
            let total = out.lines().rev().find(|l| l.starts_with("Total"));
            println!("docker builder prune: {}", total.unwrap_or("nothing"));
        }
        None => failed += 1,
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_timestamps() {
        assert_eq!(
            parse_utc("2026-09-21T14:13:20.123456789Z"),
            Some(1_790_000_000)
        );
        assert_eq!(parse_utc("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        // Never-started containers, offsets and junk keep the container.
        for bad in [
            "0001-01-01T00:00:00Z",
            "2026-09-21T14:13:20+02:00",
            "",
            "yesterday",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }

    const NOW: u64 = 1_790_000_000;

    fn rows() -> Rows {
        Rows {
            containers: [
                "c-run\tsha256:used\trunning\t0001-01-01T00:00:00Z\t2026-01-01T00:00:00Z\t/web",
                "c-old\tsha256:old\texited\t2026-09-01T00:00:00Z\t2026-01-01T00:00:00Z\t/db",
                "c-new\tsha256:used\texited\t2026-09-21T10:00:00Z\t2026-01-01T00:00:00Z\t/job",
                "c-never\tsha256:used\tcreated\t0001-01-01T00:00:00Z\t2026-08-01T00:00:00Z\t/tmp",
                "c-paused\tsha256:used\tpaused\t2026-01-01T00:00:00Z\t2026-01-01T00:00:00Z\t/p",
            ]
            .join("\n"),
            images: [
                "sha256:used\t100\t[\"app:latest\"]",
                "sha256:old\t200\t[\"db:1\"]",
                "sha256:idle\t300\t[\"a:1\",\"a:2\"]",
                "sha256:bare\t400\tnull",
            ]
            .join("\n"),
            volumes: [
                "anon1\t{\"com.docker.volume.anonymous\":\"\"}",
                "composed\t{\"com.docker.compose.project\":\"ops\"}",
                "plain\tnull",
            ]
            .join("\n"),
        }
    }

    fn ids(p: &Plan) -> Vec<&str> {
        p.items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn first_sight_removes_no_image_or_volume() {
        let p = decide(&rows(), NOW, 7, Seen::default()).unwrap();
        // Only containers carry their own date.
        assert_eq!(ids(&p), ["c-old", "c-never"]);
        assert_eq!(p.waiting, 3);
        assert_eq!(p.seen.images.len(), 4);
        assert_eq!(p.seen.volumes.keys().collect::<Vec<_>>(), ["anon1"]);
    }

    #[test]
    fn unused_long_enough_goes_and_in_use_never_does() {
        let first = decide(&rows(), NOW, 7, Seen::default()).unwrap();
        let p = decide(&rows(), NOW + 8 * DAY, 7, first.seen).unwrap();
        for gone in ["sha256:idle", "sha256:bare", "anon1"] {
            assert!(ids(&p).contains(&gone), "{gone}");
        }
        // Referenced by a container, stopped or not, or a named volume: kept.
        for kept in ["sha256:used", "sha256:old", "composed", "plain"] {
            assert!(!ids(&p).contains(&kept), "{kept}");
        }
        assert_eq!(p.seen.images["sha256:old"], NOW + 8 * DAY);
        let idle = p.items.iter().find(|i| i.id == "sha256:idle").unwrap();
        assert_eq!(idle.targets, ["a:1", "a:2"]);
        let bare = p.items.iter().find(|i| i.id == "sha256:bare").unwrap();
        assert_eq!(bare.targets, ["sha256:bare"]);
    }

    #[test]
    fn image_in_use_again_keeps_every_tag() {
        use std::cell::RefCell;
        let first = decide(&rows(), NOW, 7, Seen::default()).unwrap();
        let p = decide(&rows(), NOW + 8 * DAY, 7, first.seen).unwrap();
        let dir = std::env::temp_dir().join(format!("spaceman-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ledger = File::create(dir.join("ledger")).unwrap();
        let calls = RefCell::new(Vec::new());
        // sha256:idle (tags a:1, a:2) got a container after planning.
        let fake = |args: &[&str]| {
            calls.borrow_mut().push(args.join(" "));
            let busy = args == ["ps", "-aq", "--filter", "ancestor=sha256:idle"];
            Some(if busy { "c123\n" } else { "" }.to_string())
        };
        assert_eq!(execute_with(&p, 7, &mut ledger, &fake), 0);
        let calls = calls.into_inner();
        assert!(
            !calls.iter().any(|c| c == "rmi a:1" || c == "rmi a:2"),
            "{calls:?}"
        );
        assert!(calls.contains(&"rmi sha256:bare".to_string()), "{calls:?}");
        assert!(calls.contains(&"volume rm anon1".to_string()), "{calls:?}");
        assert!(calls.contains(&"rm -v c-old".to_string()), "{calls:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_forgets_what_is_gone() {
        let mut seen = Seen::default();
        seen.images.insert("sha256:deleted".into(), 1);
        seen.volumes.insert("reattached".into(), 1);
        let p = decide(&rows(), NOW, 7, seen).unwrap();
        assert!(!p.seen.images.contains_key("sha256:deleted"));
        assert!(!p.seen.volumes.contains_key("reattached"));
    }

    #[test]
    fn unexpected_rows_stop_the_plan() {
        let mut r = rows();
        r.containers.push_str("\nonly\ttwo");
        assert!(decide(&r, NOW, 7, Seen::default()).is_none());
        // Containers exist but none of their images is listed: wrong id space.
        let mut r = rows();
        r.images = "sha256:other\t1\tnull".into();
        assert!(decide(&r, NOW, 7, Seen::default()).is_none());
    }

    #[test]
    fn state_file_round_trip_and_garbage() {
        let dir = std::env::temp_dir().join(format!("spaceman-seen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("docker-state.json");
        assert_eq!(load_seen(&path), Seen::default());
        std::fs::write(&path, "{\"images\": {\"a\": 5}, \"volu").unwrap();
        assert_eq!(load_seen(&path), Seen::default());
        let p = decide(&rows(), NOW, 7, Seen::default()).unwrap();
        assert!(save_seen(&path, &p.seen));
        assert_eq!(load_seen(&path), p.seen);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
