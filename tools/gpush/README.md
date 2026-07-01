# gpush

Write **Google Tasks** (and, later, Calendar) idempotently from a stream of
newline-delimited JSON "publish intents". One intent per stdin line; one result
JSON per line to stdout, in input order; every line is processed; the process
exits non-zero if any line failed.

gpush owns the two things an emitter shouldn't: **OAuth** and the
**idempotency map**. You hand it intents keyed by an opaque string; it maps each
key to a Google object and thereafter *patches* rather than duplicates. That
split (generic writer here, which-items-and-when elsewhere) is the whole point —
gpush never needs to know what your keys mean.

> **MVP scope:** tasks only. The `event`/`start`/`end`/`all_day` fields are part
> of the frozen contract and are parsed, but writing to Calendar is phase-2 — an
> `event` intent returns a clear "not implemented" error today.

## Install

```
brew install orlenko/tap/gpush
```

## One-time setup

gpush talks to Google as an **installed ("Desktop") OAuth app** using credentials
*you* supply — nothing secret is committed or shipped.

1. In [Google Cloud Console](https://console.cloud.google.com/): create (or pick)
   a project, enable the **Google Tasks API**, and create an **OAuth client ID**
   of type **Desktop app**.
2. Download its JSON and save it as `~/.config/gpush/credentials.json`.
3. Authorize once:

   ```
   gpush auth
   ```

   This opens a browser for consent (loopback + PKCE — no client secret leaves
   your machine in a URL), then caches a token at `~/.config/gpush/token.json`
   (mode `0600`).

The token is scoped to **exactly** `https://www.googleapis.com/auth/tasks` —
nothing else in your account is reachable.

> ⚠️ **7-day token expiry.** If your OAuth **consent screen is in "Testing"
> status** (the default for a personal project), Google expires the refresh token
> after **7 days**, and unattended drains will start failing with a loud
> `run gpush auth` error. For a set-and-forget scheduled drain, **publish the
> consent screen to "Production"** so the refresh token is long-lived.

## Usage

```
# The batch path the runner uses:
cat pending.ndjson | gpush > results.ndjson

# Validate + report intended actions with no network and no state writes:
cat pending.ndjson | gpush --dry-run

# Recover / repair the idmap by scanning a list's gpush notes-tags:
gpush reconcile "Osavul"
```

### Intent (one per stdin line)

```json
{
  "key":    "osavul:tax-2025:005",   // REQUIRED — opaque idempotency key, stable across runs
  "op":     "upsert",                // REQUIRED — "upsert" | "close"
  "target": "task",                  // REQUIRED for upsert — "task" (| "event", phase-2)
  "title":  "[tax-2025] Pay CPP",    // REQUIRED for upsert
  "notes":  "context / link",        // optional
  "due":    "2026-07-04",            // optional, date-only (see caveat below)
  "list":   "Osavul",                // optional list name; auto-created if absent. Omit → default list
  "priority": "high"                 // optional, advisory — ignored for tasks (no priority field)
}
```

### Result (one per intent line)

```json
{ "key":"osavul:tax-2025:005", "op":"upsert", "target":"task",
  "google_id":"b3k9…", "status":"created", "ok":true, "error":null }
```

`status` ∈ `created` | `updated` | `closed` | `noop` | `error`. On failure,
`ok:false` and `error` carries the message. An **unparseable** input line yields a
`{"key":null,…,"status":"error","error":"unparseable line N"}` row (so order and
count still line up) and forces a non-zero exit — but does **not** abort the batch.

### Behavior

- `upsert`, key unknown → create the task, record `key → id`. `created`.
- `upsert`, key known → patch title/notes/due. `updated`. (Re-sending an unchanged
  item just patches again — harmless. There is no change-detection; that lives
  upstream.)
- `close` → mark the task completed. Unknown key → `noop`, **never** an error.

## How idempotency survives crashes

Google Tasks **forbids client-supplied ids**, so the `key → id` map in
`~/.config/gpush/idmap.json` is the *only* authoritative record. Two things guard
it:

- **The map is written atomically after each create.** So a torn file can't
  happen, and a crash loses at most the line in flight.
- **gpush stamps a `[gpush:key=…]` marker into every task's notes** and, before
  creating a task for an unknown key, **scans the target list** (completed +
  hidden, fully paginated) to recover any mapping the map is missing. This closes
  the gap where a create succeeds but the process dies before recording the id —
  the next run rediscovers the task instead of duplicating it. (`gpush reconcile`
  runs the same scan on demand.)

Caveats on the recovery scan:

- The notes-marker is best-effort — the notes field is user-editable, so if you
  hand-edit or clear a task's notes you remove its recovery anchor. gpush always
  appends its own marker last, so a marker forged inside emitter-supplied notes
  can't shadow the real one; the idmap stays authoritative and the scan only
  *adds* mappings, never deletes.
- Recovery is **per-list, keyed on a stable list name.** The scan looks in the
  list the current intent names, so if an interrupted create landed in list "A"
  and the re-sent intent for that key names a different list (or omits it),
  recovery won't find the orphan and a duplicate is created. Keep each key's
  `list` stable across runs.

A single advisory lock (`~/.config/gpush/gpush.lock`) prevents a manual `gpush`
from racing a scheduled drain and clobbering the map.

## The `due` caveat

Google Tasks stores `due` as a **date only** — the time-of-day is dropped, and
the stored day can shift by one across timezones. This is **lossy, not cosmetic**:
a `2026-07-04` due date may display as the 3rd or 5th for users far from UTC.
gpush sends `YYYY-MM-DDT00:00:00.000Z` for a bare date and passes through anything
already timestamp-shaped.

## License

MIT
