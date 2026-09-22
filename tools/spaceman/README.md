# spaceman

Deletes stale, regenerable build directories (`node_modules`, `.next`, `.turbo`,
Cargo `target`, Python venvs) from git checkouts nobody has touched for a week.
Meant to run unattended once a night.

Install:

```bash
brew install orlenko/tap/spaceman
```

## Usage

```bash
spaceman                      # same as `spaceman scan`: report only
spaceman scan --all           # also list what was kept, and why
spaceman run                  # delete, and append each deletion to the ledger
spaceman run --days 14        # idle threshold (default 7)
spaceman scan --root ~/code --root ~/work
spaceman run --docker --caches   # also clean Docker and the Chrome disk cache
```

Roots come from `--root`, or from `~/.config/spaceman/roots` (one directory per
line, `~/` and `#` comments allowed). **With no roots configured spaceman does
nothing.** There is no default that walks your home directory.

Every deletion is appended to `~/.local/state/spaceman/ledger.jsonl`
(`ts`, `kind`, `path`, `bytes`, `idle_days`). The ledger is a record, not an undo.

## What has to be true before a directory is deleted

All of it. A check that cannot be completed keeps the directory.

1. It is inside a git checkout (main repo, linked worktree or submodule) whose
   top directory is under a root.
2. Nothing in that checkout was written for `--days` days: no source file, no
   git `HEAD`/`index`/reflog, nothing inside the directory itself. If any part of
   the checkout cannot be walked (unreadable directory, a symlinked directory
   that leaves the checkout, another filesystem), the checkout counts as active.
3. If another checkout symlinks to the directory (a worktree sharing the main
   checkout's `node_modules`), that checkout has to pass 2 and 4 as well. Linkers
   are found under the roots and among the repo's linked worktrees, wherever
   those live. A symlink from an unrelated directory outside the roots is not
   seen.
4. No running process holds a path inside the checkout (`lsof`: cwd, mapped
   files, open fds) and no command line mentions it (`ps`; this is what catches
   `/proj/.venv/bin/python app.py` started from elsewhere). Both tools must exit
   cleanly, otherwise nothing is deleted. The command-line match is a plain
   substring, so a long-lived shell or agent whose command line names a
   checkout keeps it for as long as that process lives; `scan --all` says
   "process running in checkout".
5. `target` contains Cargo's `CACHEDIR.TAG`. A venv contains `pyvenv.cfg` and
   sits next to a lock, `pyproject.toml`, `requirements.txt` or `setup.py`.
6. `git check-ignore` says the repo itself ignores the directory (your global
   excludes file does not count) and `git ls-files` finds no tracked file in it.
7. There is no `.spaceman-keep` file in the checkout's top directory.

Scanning a large tree takes minutes, so `run` does not trust its first pass:
right before deleting a checkout's directories it evaluates that checkout again
from scratch against a fresh process list, and removes only what is still
eligible. `run` also refuses to delete anything when more than `--max-items`
(default 200) directories are eligible; read `spaceman scan` and raise it.

The idle check reads mtimes, so a project you only *read* for a week looks idle.
That is why the list is limited to directories one command rebuilds.

Reported sizes count allocated blocks and skip hardlinked files (pnpm links into
its global store; unlinking those frees nothing). APFS clones can still make the
number optimistic.

## Scheduling

spaceman does not install a scheduler. macOS, `~/Library/LaunchAgents/ca.orlenko.spaceman.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>ca.orlenko.spaceman</string>
  <key>ProgramArguments</key>
  <array><string>/opt/homebrew/bin/spaceman</string><string>run</string><string>--docker</string><string>--caches</string></array>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>3</integer><key>Minute</key><integer>30</integer></dict>
  <key>StandardOutPath</key><string>/Users/YOU/.local/state/spaceman/launchd.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/.local/state/spaceman/launchd.log</string>
</dict></plist>
```

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ca.orlenko.spaceman.plist
```

A sleeping Mac runs the job when it wakes, not at 03:30; the process check and
the second evaluation before each delete exist for that case. launchd does not
expand `~`, so write your home directory out, and `mkdir -p
~/.local/state/spaceman` first. The log lists project paths, which is why it
does not go to `/tmp`.

Linux: `30 3 * * * spaceman run >> ~/.local/state/spaceman/cron.log 2>&1`
(needs `git`, `lsof` and `ps`). The tap ships an arm64 macOS binary only; build from
source elsewhere.

## `--docker`

Off unless asked for. Docker's own `--filter until=` on images and containers
compares the *build* or *create* date, so an upstream base image looks a year
old the minute you pull it, and `docker volume prune` has no age filter at all.
spaceman applies `--days` as "unused for that long" to each kind:

- a **stopped container** goes once it has been stopped for `--days`, together
  with its anonymous volumes (`docker rm -v`). This is more than `docker system
  prune` does by default: it is `system prune --volumes` for that container.
  Whatever lived only in its writable layer or its anonymous volumes is gone;
- an **image** goes once no container, running or stopped, has referenced it for
  `--days`. Expect a re-pull if only a Compose file or a script referenced it;
- a **dangling anonymous volume** goes once it has been dangling for `--days`;
- **build cache** unused for `--days` goes through `docker builder prune`.

Docker records neither when an image was last used nor when a volume became
dangling, so spaceman keeps `~/.local/state/spaceman/docker-state.json`. Both
`scan --docker` and `run --docker` update it (it is the only thing `scan` ever
writes). Something new counts as used the first time it is seen, so images and
volumes are never removed during the first `--days` days, and `scan --docker`
shows what the next `run` would take.

Named volumes are never touched; a volume without the engine's anonymous label
(Engine 23+) counts as named. Nothing is forced. A container that was started
after planning makes `docker rm` refuse. `docker rmi <tag>` does not refuse (it
untags an image even under a running container), so spaceman looks up an
image's containers once more right before removing it and leaves every tag
alone on a hit. `scan --docker` prints the total build cache size; docker does
not say how much of it `--days` will take. No daemon, no docker
step. If container image ids do not match the image list (containerd image
store), the step stops rather than guess. `--max-items` caps docker objects too.

## `--caches`

Off unless asked for. Deletes Chrome/Chromium disk-cache entries (`Cache` and
`Code Cache` of every profile) older than `--days`. Only files named like a
cache entry (`<16 hex>_0`, `_1`, `_s`) are removed; the index stays. The browser
treats a missing entry as a cache miss, so this is safe while it runs.

An entry is up to three files sharing a key; it goes as a whole or not at all.

Playwright's browser builds (`~/Library/Caches/ms-playwright`) are not handled.
`npx playwright install` drops builds that no installed Playwright references,
but only when you run it.

## Not in scope, on purpose

- **Whole worktrees.** Once the generated directories inside a stale worktree are
  gone, the worktree itself is a few MB of source plus whatever gitignored files
  (`.env`) someone copied in, which are not recoverable.
- **Package-manager caches.** `brew` cleans itself every 30 days; `npm cache
  verify` frees next to nothing.
