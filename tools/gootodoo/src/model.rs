//! NDJSON intent/result types, validation, and the pure helpers (notes-tag
//! stamping, due-date conversion) that back the offline `selftest`. Nothing here
//! touches the network or disk, so it is cheap to unit-test exhaustively.

use serde::{Deserialize, Serialize};

/// One input line: a publish intent from the emitter. Only `key`/`op` are always
/// present; the rest depend on `op`/`target` and are validated in [`Intent::validate`].
#[derive(Debug, Deserialize)]
pub struct Intent {
    pub key: String,
    pub op: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
    #[serde(default)]
    pub list: Option<String>,
    // Accepted-but-ignored today: the event fields are phase-2 (calendar), and
    // `priority` is advisory with no Google Tasks equivalent. Parsed so a
    // contract-valid line never fails on them; deliberately not read yet.
    #[serde(default)]
    #[allow(dead_code)]
    pub all_day: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub start: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub end: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub priority: Option<String>,
}

/// One output line, faithful to the frozen result schema. Every field is emitted
/// (null where not applicable) so the reader can join on `key` positionally.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub key: Option<String>,
    pub op: Option<String>,
    pub target: Option<String>,
    pub google_id: Option<String>,
    pub status: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// One `pull` output row: the read-back state of a managed task. `status` is
/// `needsAction` | `completed` | `deleted` | `error`. `deleted` = the task is no
/// longer at its managed (list, id) — the human removed it, OR (rarely) moved it
/// out of its list; a single GET can't tell those apart. `error` = gootodoo
/// couldn't read it (transient/auth/permission); `error` carries the message and
/// the row is emitted so the stream stays one-per-key and never truncates.
/// `completed_at` is the RFC3339 completion stamp, null unless completed.
#[derive(Debug, Serialize)]
pub struct PullRow {
    pub key: String,
    pub google_id: String,
    pub status: String,
    pub completed_at: Option<String>,
    pub list: String,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Task,
    Event,
}

#[derive(Debug)]
pub enum Action {
    Upsert { target: Target, title: String },
    Close,
}

impl Intent {
    /// Shape-check the intent against the frozen contract. Returns the concrete
    /// action to perform, or a human-readable reason it is malformed.
    pub fn validate(&self) -> Result<Action, String> {
        if self.key.is_empty() {
            return Err("key is required and must be non-empty".into());
        }
        // The notes-tag marker is delimited by ']', so a key containing one would
        // make the rebuild scan ambiguous. Reject it rather than corrupt recovery.
        if self.key.contains(']') {
            return Err("key must not contain ']'".into());
        }
        match self.op.as_str() {
            "upsert" => {
                let target = match self.target.as_deref() {
                    Some("task") => Target::Task,
                    Some("event") => Target::Event,
                    Some(other) => return Err(format!("unknown target {other:?}")),
                    None => return Err("upsert requires a target (\"task\" or \"event\")".into()),
                };
                let title = self
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .ok_or("upsert requires a non-empty title")?;
                Ok(Action::Upsert { target, title })
            }
            "close" => Ok(Action::Close),
            other => Err(format!(
                "unknown op {other:?} (expected \"upsert\" or \"close\")"
            )),
        }
    }
}

impl Outcome {
    fn base(intent: &Intent) -> Outcome {
        Outcome {
            key: Some(intent.key.clone()),
            op: Some(intent.op.clone()),
            target: intent.target.clone(),
            google_id: None,
            status: String::new(),
            ok: false,
            error: None,
        }
    }

    /// A successful outcome with the given status and (optional) Google id.
    pub fn ok(intent: &Intent, status: &str, google_id: Option<String>) -> Outcome {
        Outcome {
            google_id,
            status: status.to_string(),
            ok: true,
            error: None,
            ..Self::base(intent)
        }
    }

    /// A per-line failure that still echoes the intent's key/op for correlation.
    pub fn invalid(intent: &Intent, msg: impl Into<String>) -> Outcome {
        Outcome {
            status: "error".to_string(),
            ok: false,
            error: Some(msg.into()),
            ..Self::base(intent)
        }
    }

    /// A line that could not be parsed as JSON at all — no key to echo, but order
    /// and count are preserved so the reader's positional join still lines up.
    pub fn unparseable(lineno: usize) -> Outcome {
        Outcome {
            key: None,
            op: None,
            target: None,
            google_id: None,
            status: "error".to_string(),
            ok: false,
            error: Some(format!("unparseable line {lineno}")),
        }
    }

    /// Valid JSON that isn't a well-formed intent (e.g. a missing `key`). Echoes
    /// whatever key/op/target the object *did* carry so it's debuggable — unlike
    /// [`unparseable`], this line was structurally understood, just not usable.
    pub fn malformed(raw: &serde_json::Value, msg: impl Into<String>) -> Outcome {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_str()).map(str::to_string);
        Outcome {
            key: field("key"),
            op: field("op"),
            target: field("target"),
            google_id: None,
            status: "error".to_string(),
            ok: false,
            error: Some(msg.into()),
        }
    }
}

/// The marker gootodoo appends to every task's notes. It is gootodoo's, not the
/// emitter's: re-appended on *every* write because a later patch (which carries
/// the emitter's notes, sans marker) would otherwise strip it and orphan the
/// rebuild anchor.
pub const KEY_TAG_PREFIX: &str = "[gootodoo:key=";

/// The pre-rename marker (the tool was `gpush` through v0.1.0). Still *read* for
/// recovery so tasks created before the rename aren't orphaned; never written.
const LEGACY_KEY_TAG_PREFIX: &str = "[gpush:key=";

/// Append the key marker to the emitter's notes (which may be empty).
pub fn stamp_notes(user_notes: Option<&str>, key: &str) -> String {
    let marker = format!("{KEY_TAG_PREFIX}{key}]");
    match user_notes {
        Some(n) if !n.is_empty() => format!("{n}\n\n{marker}"),
        _ => marker,
    }
}

/// Recover the key from a task's notes, if gootodoo stamped it. Best-effort: the
/// notes field is user-editable, so a missing/edited marker just means that task
/// won't be recovered by a rebuild (the idmap stays authoritative). We take the
/// *last* marker via `rfind` — gootodoo always appends its own at the very end, so
/// a marker an emitter forged inside the notes body can't shadow the real one. The
/// current marker wins over the legacy one when both are present.
pub fn extract_key(notes: &str) -> Option<String> {
    for prefix in [KEY_TAG_PREFIX, LEGACY_KEY_TAG_PREFIX] {
        if let Some(idx) = notes.rfind(prefix) {
            let rest = &notes[idx + prefix.len()..];
            if let Some(end) = rest.find(']') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Google Tasks stores `due` as a date only — the time-of-day is dropped and the
/// day itself can shift across timezones (this is lossy, not merely cosmetic). We
/// send midnight UTC for a bare `YYYY-MM-DD`; anything already timestamp-shaped is
/// passed through untouched.
pub fn due_to_rfc3339(due: &str) -> String {
    let b = due.as_bytes();
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' {
        format!("{due}T00:00:00.000Z")
    } else {
        due.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_upsert_task() {
        let i: Intent =
            serde_json::from_str(r#"{"key":"k1","op":"upsert","target":"task","title":"hi"}"#)
                .unwrap();
        match i.validate() {
            Ok(Action::Upsert {
                target: Target::Task,
                title,
            }) => assert_eq!(title, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_bad_shapes() {
        let cases = [
            r#"{"key":"","op":"upsert","target":"task","title":"x"}"#,
            r#"{"key":"k]bad","op":"close"}"#,
            r#"{"key":"k","op":"upsert","title":"x"}"#,
            r#"{"key":"k","op":"upsert","target":"task"}"#,
            r#"{"key":"k","op":"frobnicate"}"#,
        ];
        for c in cases {
            let i: Intent = serde_json::from_str(c).unwrap();
            assert!(i.validate().is_err(), "should reject: {c}");
        }
    }

    #[test]
    fn close_needs_no_target_or_title() {
        let i: Intent = serde_json::from_str(r#"{"key":"k","op":"close"}"#).unwrap();
        assert!(matches!(i.validate(), Ok(Action::Close)));
    }

    #[test]
    fn notes_tag_round_trips() {
        let s = stamp_notes(Some("body text"), "osavul:tax-2025:005");
        assert!(s.starts_with("body text"));
        assert_eq!(extract_key(&s).as_deref(), Some("osavul:tax-2025:005"));

        let s2 = stamp_notes(None, "k2");
        assert_eq!(extract_key(&s2).as_deref(), Some("k2"));

        assert_eq!(extract_key("no marker here"), None);

        // A forged marker inside emitter notes must not shadow gootodoo's real one,
        // which is always appended last.
        let spoofed = stamp_notes(Some("sneaky [gootodoo:key=EVIL] text"), "real-key");
        assert_eq!(extract_key(&spoofed).as_deref(), Some("real-key"));

        // Pre-rename tasks carry the legacy `[gpush:key=]` marker — still recovered.
        assert_eq!(
            extract_key("body\n\n[gpush:key=old-key]").as_deref(),
            Some("old-key")
        );
        // When both are present, the current marker wins.
        let both = format!("{}\n\n{}", "[gpush:key=old]", stamp_notes(None, "new"));
        assert_eq!(extract_key(&both).as_deref(), Some("new"));
    }

    #[test]
    fn due_conversion() {
        assert_eq!(due_to_rfc3339("2026-07-04"), "2026-07-04T00:00:00.000Z");
        // Already a timestamp → passthrough.
        assert_eq!(
            due_to_rfc3339("2026-07-04T09:00:00Z"),
            "2026-07-04T09:00:00Z"
        );
        // Not a bare date → passthrough (Google will reject if truly bogus).
        assert_eq!(due_to_rfc3339("garbage"), "garbage");
    }

    #[test]
    fn pull_row_serializes_to_contract_shape() {
        let row = PullRow {
            key: "osavul:tax-2025:005".to_string(),
            google_id: "b3k9".to_string(),
            status: "completed".to_string(),
            completed_at: Some("2026-07-05T14:02:00.000Z".to_string()),
            list: "Tax 2025".to_string(),
            error: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(v["key"], "osavul:tax-2025:005");
        assert_eq!(v["google_id"], "b3k9");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["completed_at"], "2026-07-05T14:02:00.000Z");
        assert_eq!(v["list"], "Tax 2025");
        assert!(v.get("error").is_some_and(|e| e.is_null()));

        // A deleted task: completed_at is null, not omitted.
        let deleted = PullRow {
            key: "k".to_string(),
            google_id: "g".to_string(),
            status: "deleted".to_string(),
            completed_at: None,
            list: "Personal".to_string(),
            error: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&deleted).unwrap()).unwrap();
        assert!(v.get("completed_at").is_some_and(|c| c.is_null()));
    }

    #[test]
    fn unparseable_has_null_key() {
        let o = Outcome::unparseable(7);
        assert!(!o.ok);
        assert!(o.key.is_none());
        assert_eq!(o.error.as_deref(), Some("unparseable line 7"));
        assert_eq!(o.status, "error");
    }
}
