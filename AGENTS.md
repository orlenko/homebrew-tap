# Agent helpers in orlenko/tap

Small, self-contained CLI helpers for the patterns AI coding agents keep
hand-assembling across sessions. Each installs from this tap:

```sh
brew install orlenko/tap/<tool>
```

This file is the **canonical** "what exists and when to reach for it" reference —
point your agent's global instructions here instead of pasting a copy (a pasted
copy drifts; a link doesn't).

## Retrieval verbs

| verb         | what it does                                                         | runtime dep            |
|--------------|---------------------------------------------------------------------|------------------------|
| `gpeek`      | `git show`/`git diff` at a ref, with optional line slicing           | git (system)           |
| `ghcat`      | print one file from an un-checked-out GitHub repo (raw, no base64)   | `gh`                   |
| `fetch-text` | curl → plain text for WebFetch-blocked domains                      | curl (system); w3m opt |
| `gh-recon`   | one-screen dossier (meta / releases / open PRs) for an uncloned repo | `gh`                   |
| `mgw`        | `mgrep` web search, web-only + line-capped output                   | mgrep (npm, see below) |
| `eml2txt`    | read saved `.eml`: print body, save attachments to `_attachments/`  | — (single binary)      |
| `pr-family`  | tree of your open PRs stacked around a given PR (ASCII / Mermaid)    | `gh`, python3 (system) |

### When to reach for each

- **`gpeek`** — you're *in* a repo and want a file (or a slice) at some ref, or a
  single file's diff against a baseline. Replaces
  `git show <ref>:<path> | sed -n 'A,Bp'` and `git diff <range> -- <path>`. Set
  `GPEEK_BASE` once (a merge-base SHA) so `gpeek diff <path>` needs no range.
- **`ghcat`** — one file from a GitHub repo you have **not** cloned. File-level.
  Replaces `gh api .../contents/... --jq .content | base64 -d`.
- **`fetch-text`** — the readable text of a page `WebFetch` refuses (reddit,
  stackoverflow, superuser, law.stackexchange, …). Pipe to `head` to cap it.
- **`gh-recon`** — sizing up a GitHub repo you have **not** cloned: stars,
  default branch, last push, latest releases, recently-updated open PRs.
  Repo-level, vs `ghcat`'s file-level.
- **`mgw`** — web results from `mgrep` without the local-file noise or a manual
  `| head`. Web-only by default (`-a` to include local `./` hits), capped (`-n`,
  default 40).
- **`eml2txt`** — you have saved `.eml` file(s) and want the body as text plus
  the attachments on disk. macOS `textutil` can't parse `.eml`; this prints the
  body (HTML-stripped if there's no plaintext) and drops attachments in a
  sibling `_attachments/` dir.
- **`pr-family`** — understand a stack of PRs: walks the base/head relationships
  among your open PRs and prints the whole family as a tree
  (`pr-family --output md` for Markdown + a Mermaid graph).

## Human-facing helpers (also in the tap)

- **`show-claude-images`** — browse the images pasted into the current Claude
  Code session (opens `yazi`). Needs `yazi`. Interactive TUI; not for agents.
- **`move-pr-diff`** — re-base a PR's diff onto a new target branch as a fresh
  PR. Needs `gh` + `jq`.

## Notes

- Every tool has `--help`, exits non-zero on failure, and prints errors to stderr.
- `mgw` needs **`mgrep`**, which isn't in Homebrew: `npm install -g @mixedbread/mgrep`.
- `fetch-text` uses `w3m` for nicer HTML→text when present, else a built-in
  stripper — so `w3m` is optional, not required.
- `pr-family` runs on the system `python3` (Xcode Command Line Tools).
