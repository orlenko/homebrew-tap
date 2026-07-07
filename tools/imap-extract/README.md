# imap-extract

A background watcher that connects to an IMAP server, monitors a folder/label in
real time via `IMAP IDLE`, and exports new messages as Markdown files (plus
attachment subdirectories).

Install:

```bash
brew install orlenko/tap/imap-extract
```

**Configuration is per-directory.** When you run it, it reads a `.env` from the
*current* directory and keeps its sync pointer in a local `state.json` there. So
each folder you run from is self-contained — drop a `.env` pointing at whichever
IMAP server you want and run `imap-extract`. Run one per terminal/tmux session to
watch several mailboxes at once; they never interfere.

```bash
cd ~/intake/gmail   && imap-extract            # reads ./.env, dumps here
cd ~/intake/proton  && imap-extract Labels/FSR # override the folder for this run
```

## Configuration

Copy `.env.example` to `.env` in the folder you want to watch and fill it in.
Real shell environment variables override the file (handy for one-offs:
`IMAP_PASSWORD=… imap-extract`). Proton Mail Bridge uses STARTTLS with a
self-signed cert: set `IMAP_STARTTLS=true`, `IMAP_SECURE=false`,
`IMAP_TLS_REJECT_UNAUTHORIZED=false`.

## Usage

```bash
imap-extract [folder] [target-dir] [options]
```

| Option | Effect |
| --- | --- |
| `--once` | Sync once and exit (no live IDLE loop). Good for cron. |
| `--mark-read` | Mark messages read on the server (`RFC822`, sets `\Seen`). By default mail is left unread (`BODY.PEEK`) so a human still notices it. Also enabled by `IMAP_MARK_READ=true`. |
| `--env-file <path>` | Read config from a specific file instead of `./.env`. |
| `--print-config` | Print the resolved server/folder/target/state paths and exit. |
| `selftest` | Run a no-network self-check and exit. |

State (`state.json`) and config (`.env`) are local to the directory you run from;
the dumped Markdown goes to `TARGET_DIR` (or the current dir), normally a
*different* directory, so a downstream process can consume the `.md` files without
touching the watcher's state. On first run the watcher baselines to the current
highest UID and exports only mail that arrives afterward.

## Always-on (optional)

The intended workflow is one foreground `imap-extract` per terminal/tmux session.
For a single always-on watcher, wrap it in a `launchd` agent pointing its
`WorkingDirectory` at the folder whose `.env` it should use.
