# gootodoo

Write **Google Tasks** (and, later, Calendar) idempotently from a stream of
newline-delimited JSON "publish intents". One intent per stdin line; one result
JSON per line to stdout, in input order; every line is processed; the process
exits non-zero if any line failed.

gootodoo owns the two things an emitter shouldn't: **OAuth** and the
**idempotency map**. You hand it intents keyed by an opaque string; it maps each
key to a Google object and thereafter *patches* rather than duplicates. That
split (generic writer here, which-items-and-when elsewhere) is the whole point —
gootodoo never needs to know what your keys mean.

> **MVP scope:** tasks only. The `event`/`start`/`end`/`all_day` fields are part
> of the frozen contract and are parsed, but writing to Calendar is phase-2 — an
> `event` intent returns a clear "not implemented" error today.

## Install

```
brew install orlenko/tap/gootodoo
```

## One-time setup

gootodoo talks to Google as an **installed ("Desktop") OAuth app** using credentials
*you* supply — nothing secret is committed or shipped.

1. In [Google Cloud Console](https://console.cloud.google.com/): create (or pick)
   a project, enable the **Google Tasks API**, and create an **OAuth client ID**
   of type **Desktop app**.
2. Download its JSON and save it as `~/.config/gootodoo/credentials.json`.
3. Authorize once:

   ```
   gootodoo auth
   ```

   This opens a browser for consent (loopback + PKCE — no client secret leaves
   your machine in a URL), then caches a token at `~/.config/gootodoo/token.json`
   (mode `0600`).

The token is scoped to **exactly** `https://www.googleapis.com/auth/tasks` —
nothing else in your account is reachable.

> ⚠️ **7-day token expiry.** If your OAuth **consent screen is in "Testing"
> status** (the default for a personal project), Google expires the refresh token
> after **7 days**, and unattended drains will start failing with a loud
> `run gootodoo auth` error. For a set-and-forget scheduled drain, **publish the
> consent screen to "Production"** so the refresh token is long-lived.

## Usage

```
# The batch path the runner uses:
cat pending.ndjson | gootodoo > results.ndjson

# Validate + report intended actions with no network and no state writes:
cat pending.ndjson | gootodoo --dry-run

# Recover / repair the idmap by scanning a list's gootodoo notes-tags:
gootodoo reconcile "Osavul"
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

## Reading state back (`pull`)

`gootodoo` is a writer, but the human also acts on tasks directly — checking them
off (or deleting them) on their phone. `pull` reads that back so the upstream
emitter can reconcile:

```
gootodoo pull [--list <name>] [--completed-only]   # NDJSON to stdout, read-only
```

One row per task in the idmap (idmap-driven, so exactly one row per key — a task
that's mid-move or duplicated can never produce two rows):

```json
{ "key":"osavul:tax-2025:005", "google_id":"b3k9…",
  "status":"completed", "completed_at":"2026-07-05T14:02:00.000Z", "list":"Tax 2025" }
```

- `status` ∈ `needsAction` | `completed` | `deleted` | `error`. `completed_at` is
  null unless completed.
  - **`deleted`** = the task is no longer at the `(list, id)` gootodoo recorded.
    Almost always the human deleted it — but a single GET *can't* distinguish that
    from the human manually moving it to a different list (off-model in the
    per-teka layout, where gootodoo owns placement). Treat it as "left gootodoo's
    management."
  - **`error`** = gootodoo couldn't read that task (transient/auth/permission); the
    message is in `error`. The row is still emitted (so the stream stays one-per-key
    and never truncates) and pull exits **non-zero**.
- `--list <name>` restricts to one list; bare `pull` covers every list gootodoo
  manages. `--completed-only` emits the completed ones (and any `error` rows, so a
  failure is never silently dropped).
- **Read-only** with respect to your tasks and the idmap — it only GETs from Google
  and never mutates the map. (It may still refresh the cached OAuth token, rewriting
  `~/.config/gootodoo/token.json`.) Bounded blind spot: a task created but lost from
  the idmap to a crash, then completed before the next `upsert` re-adopts it, isn't
  reported until that `upsert` — it self-heals on the next drain of that key.

## How idempotency survives crashes

Google Tasks **forbids client-supplied ids**, so the `key → id` map in
`~/.config/gootodoo/idmap.json` is the *only* authoritative record. Two things guard
it:

- **The map is written atomically after each create.** So a torn file can't
  happen, and a crash loses at most the line in flight.
- **gootodoo stamps a `[gootodoo:key=…]` marker into every task's notes** and, before
  creating a task for an unknown key, **scans the target list** (completed +
  hidden, fully paginated) to recover any mapping the map is missing. This closes
  the gap where a create succeeds but the process dies before recording the id —
  the next run rediscovers the task instead of duplicating it. (`gootodoo reconcile`
  runs the same scan on demand.)

Caveats on the recovery scan:

- The notes-marker is best-effort — the notes field is user-editable, so if you
  hand-edit or clear a task's notes you remove its recovery anchor. gootodoo always
  appends its own marker last, so a marker forged inside emitter-supplied notes
  can't shadow the real one; the idmap stays authoritative and the scan only
  *adds* mappings, never deletes.
- Recovery is **per-list, keyed on a stable list name.** The scan looks in the
  list the current intent names, so if an interrupted create landed in list "A"
  and the re-sent intent for that key names a different list (or omits it),
  recovery won't find the orphan and a duplicate is created. Keep each key's
  `list` stable across runs.

A single advisory lock (`~/.config/gootodoo/gootodoo.lock`) prevents a manual `gootodoo`
from racing a scheduled drain and clobbering the map.

## The `due` caveat

Google Tasks stores `due` as a **date only** — the time-of-day is dropped, and
the stored day can shift by one across timezones. This is **lossy, not cosmetic**:
a `2026-07-04` due date may display as the 3rd or 5th for users far from UTC.
gootodoo sends `YYYY-MM-DDT00:00:00.000Z` for a bare date and passes through anything
already timestamp-shaped.

## License

MIT
