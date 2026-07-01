//! Minimal Google Tasks REST client: resolve/create lists, create/patch/complete
//! tasks, and the rebuild scan that recovers `key → id` mappings from notes-tags.
//! Only the handful of calls the drainer needs — no generated-client bulk.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::oauth::Auth;

const BASE: &str = "https://tasks.googleapis.com/tasks/v1";
/// Transient statuses worth a bounded retry (rate-limit / server hiccup).
const MAX_RETRIES: u32 = 2;

pub struct Tasks {
    agent: ureq::Agent,
    auth: Auth,
}

/// Outcome of a patch/complete against a task that may have been deleted in Google
/// out from under us.
pub enum PatchOutcome {
    Patched,
    NotFound,
}

impl Tasks {
    pub fn new(auth: Auth) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent, auth }
    }

    /// A fresh bearer header, refreshing the token through `Auth` if it expired
    /// mid-batch — so a drain that outlives one token lifetime keeps working.
    fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.auth.access_token()?))
    }

    /// Run one request, retrying a handful of times on a network error or a
    /// transient (429 / 5xx) status with linear backoff. Idempotency makes retry
    /// safe; a transient blip should not silently poison the create-gap safety net.
    fn with_retry<F>(&self, mut attempt: F) -> Result<(u16, Value)>
    where
        F: FnMut() -> Result<(u16, Value)>,
    {
        let mut tries = 0;
        loop {
            match attempt() {
                Ok((status, _)) if is_transient(status) && tries < MAX_RETRIES => {}
                Err(_) if tries < MAX_RETRIES => {}
                other => return other,
            }
            tries += 1;
            std::thread::sleep(Duration::from_millis(500 * u64::from(tries)));
        }
    }

    fn get(&self, url: &str) -> Result<(u16, Value)> {
        self.with_retry(|| {
            let bearer = self.bearer()?;
            let mut resp = self
                .agent
                .get(url)
                .header("Authorization", &bearer)
                .call()
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status().as_u16();
            let val = resp.body_mut().read_json().unwrap_or(Value::Null);
            Ok((status, val))
        })
    }

    fn send_json(&self, method: &str, url: &str, body: Value) -> Result<(u16, Value)> {
        self.with_retry(|| {
            let req = match method {
                "POST" => self.agent.post(url),
                "PATCH" => self.agent.patch(url),
                _ => bail!("unsupported method {method}"),
            };
            let bearer = self.bearer()?;
            let mut resp = req
                .header("Authorization", &bearer)
                .send_json(&body)
                .with_context(|| format!("{method} {url}"))?;
            let status = resp.status().as_u16();
            let val = resp.body_mut().read_json().unwrap_or(Value::Null);
            Ok((status, val))
        })
    }

    /// Resolve a task-list *name* to its id, creating the list if absent. `None`
    /// selects the user's default list (`@default`).
    pub fn resolve_or_create_list(&self, name: Option<&str>) -> Result<String> {
        let Some(name) = name else {
            return Ok("@default".to_string());
        };
        let mut page: Option<String> = None;
        loop {
            let url = match &page {
                Some(t) => format!("{BASE}/users/@me/lists?maxResults=100&pageToken={t}"),
                None => format!("{BASE}/users/@me/lists?maxResults=100"),
            };
            let (status, v) = self.get(&url)?;
            if !(200..300).contains(&status) {
                bail!("listing task-lists failed ({status}): {}", gerr(&v));
            }
            if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
                for it in items {
                    if it.get("title").and_then(|t| t.as_str()) == Some(name)
                        && let Some(id) = it.get("id").and_then(|i| i.as_str())
                    {
                        return Ok(id.to_string());
                    }
                }
            }
            match v.get("nextPageToken").and_then(|t| t.as_str()) {
                Some(t) => page = Some(t.to_string()),
                None => break,
            }
        }
        // Not found → create it.
        let (status, v) = self.send_json(
            "POST",
            &format!("{BASE}/users/@me/lists"),
            json!({ "title": name }),
        )?;
        if !(200..300).contains(&status) {
            bail!(
                "creating task-list {name:?} failed ({status}): {}",
                gerr(&v)
            );
        }
        v.get("id")
            .and_then(|i| i.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("created task-list but no id was returned"))
    }

    /// Scan a list — completed *and* hidden, fully paginated — and return every
    /// `(key, task_id)` recoverable from a gpush notes-tag. This closes the
    /// crash-between-create-and-idmap-write gap: a task Google has but the idmap
    /// lost is rediscovered here before the next create would duplicate it.
    pub fn scan_keys(&self, list_id: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let base = format!(
                "{BASE}/lists/{list_id}/tasks?showCompleted=true&showHidden=true&maxResults=100"
            );
            let url = match &page {
                Some(t) => format!("{base}&pageToken={t}"),
                None => base,
            };
            let (status, v) = self.get(&url)?;
            if !(200..300).contains(&status) {
                bail!("scanning list {list_id} failed ({status}): {}", gerr(&v));
            }
            if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
                for it in items {
                    let (Some(id), Some(notes)) = (
                        it.get("id").and_then(|x| x.as_str()),
                        it.get("notes").and_then(|x| x.as_str()),
                    ) else {
                        continue;
                    };
                    if let Some(key) = crate::model::extract_key(notes) {
                        out.push((key, id.to_string()));
                    }
                }
            }
            match v.get("nextPageToken").and_then(|t| t.as_str()) {
                Some(t) => page = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    pub fn create_task(
        &self,
        list_id: &str,
        title: &str,
        notes: &str,
        due: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({ "title": title, "notes": notes, "status": "needsAction" });
        if let Some(d) = due {
            body["due"] = json!(d);
        }
        let (status, v) = self.send_json("POST", &format!("{BASE}/lists/{list_id}/tasks"), body)?;
        if !(200..300).contains(&status) {
            bail!("creating task failed ({status}): {}", gerr(&v));
        }
        v.get("id")
            .and_then(|i| i.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("created task but no id was returned"))
    }

    pub fn patch_task(
        &self,
        list_id: &str,
        task_id: &str,
        title: &str,
        notes: &str,
        due: Option<&str>,
    ) -> Result<PatchOutcome> {
        let mut body = json!({ "title": title, "notes": notes });
        if let Some(d) = due {
            body["due"] = json!(d);
        }
        let (status, v) = self.send_json(
            "PATCH",
            &format!("{BASE}/lists/{list_id}/tasks/{task_id}"),
            body,
        )?;
        match status {
            s if (200..300).contains(&s) => Ok(PatchOutcome::Patched),
            404 => Ok(PatchOutcome::NotFound),
            s => bail!("patching task failed ({s}): {}", gerr(&v)),
        }
    }

    pub fn complete_task(&self, list_id: &str, task_id: &str) -> Result<PatchOutcome> {
        let (status, v) = self.send_json(
            "PATCH",
            &format!("{BASE}/lists/{list_id}/tasks/{task_id}"),
            json!({ "status": "completed" }),
        )?;
        match status {
            s if (200..300).contains(&s) => Ok(PatchOutcome::Patched),
            404 => Ok(PatchOutcome::NotFound),
            s => bail!("completing task failed ({s}): {}", gerr(&v)),
        }
    }
}

/// Rate-limit or transient server error — worth a bounded retry.
fn is_transient(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Pull the human-readable message out of a Google API error body.
fn gerr(v: &Value) -> String {
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.to_string())
}
