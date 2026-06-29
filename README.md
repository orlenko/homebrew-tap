# orlenko/homebrew-tap

A Homebrew tap that doubles as a **factory for small, AI-authored CLI tools** —
each compiled to a single self-contained binary, installable with one command.

## Install

```bash
brew install orlenko/tap/<tool>     # e.g. imap-extract
```

That auto-taps `orlenko/tap` and installs. (Or `brew tap orlenko/tap` first.)

## Tools

Compiled (Rust) — single self-contained binaries:

| Tool | What it does |
| --- | --- |
| [`imap-extract`](tools/imap-extract) | Watch an IMAP folder over IDLE and export new mail as Markdown |
| [`eml2txt`](tools/eml2txt) | Read saved .eml messages: print the body, save attachments |

Scripts (bash/python) — interpreted helpers that wrap existing CLIs:

| Tool | What it does |
| --- | --- |
| [`gpeek`](scripts/gpeek) | Show git file content or a diff at a ref, with line slicing |
| [`ghcat`](scripts/ghcat) | Print one file from an un-checked-out GitHub repo |
| [`fetch-text`](scripts/fetch-text) | Fetch a web page as plain text for WebFetch-blocked domains |
| [`gh-recon`](scripts/gh-recon) | Dossier for an un-cloned GitHub repo: meta, releases, open PRs |
| [`mgw`](scripts/mgw) | Web search via mgrep, web-only with capped output |
| [`pr-family`](scripts/pr-family) | Show a tree of your open PRs stacked around a given PR |
| [`move-pr-diff`](scripts/move-pr-diff) | Re-base a PR's diff onto a new target branch as a fresh PR |
| [`show-claude-images`](scripts/show-claude-images) | Browse images pasted into the current Claude Code session |

Many of these are **agent helpers** — see [AGENTS.md](AGENTS.md) for what each is
for and when to reach for it.

## How it works

- **Rust by default.** Most tools live in `tools/<name>/` as Cargo workspace
  members, compile to small binaries (imap-extract is ~2.3 MB), and are auto
  ad-hoc-signed by Apple's `ld64` on the native macOS CI runner — they just run on
  Apple Silicon. Bun/TypeScript is a narrow, gated escape hatch (browser
  automation, npm-only SaaS SDKs); standalone Python apps live on `uv`, elsewhere.
- **Scripts lane.** Rust isn't dogma — a 30-line `gh`/`git`/`curl` wrapper
  doesn't earn a compile step. Interpreted helpers (`lang: bash|python`) live in
  `scripts/<name>/` with the *same* `tool.json` + generated formula, and ship as a
  `noarch` tarball (no build, no signing). They declare only genuinely-absent
  deps (`gh`, `jq`, `yazi`); system tools (git/curl/bash/python3) aren't declared.
- **Declare once.** Each tool's `tool.json` is the source of truth. `forge`
  generates the Homebrew formula from it. Formulas in `Formula/` are
  **generated — never hand-edited** (CI fails on drift).
- **Release = a git tag.** Push `<tool>-vX.Y.Z`; GitHub Actions builds (compiled)
  or tarballs as-is (scripts), smoke-tests that it actually runs, publishes a
  Release with the tarball + sha256, regenerates the formula, and commits it back.

## Adding a tool

**Compiled (Rust):**

1. Create `tools/<name>/` with a `tool.json` (`lang: "rust"`), `Cargo.toml`, `src/`.
2. Implement a deterministic `selftest` subcommand (it backs the formula's test).
3. `forge lint <name>` to validate the manifest against `brew audit` rules.
4. Tag `<name>-v0.1.0` and push — CI builds, releases, and writes the formula.

**Script (bash/python):**

1. Create `scripts/<name>/<name>` — the executable itself, with a portable
   `#!/usr/bin/env …` shebang and `chmod +x`.
2. Add `scripts/<name>/tool.json` (`lang: "bash"|"python"`, `test: "--help"`, and
   `depends_on` / `caveats` only for genuinely-absent deps).
3. `forge lint <name>`; tag `<name>-v0.1.0` and push — CI tarballs and releases it.

## Development

```bash
cargo build --workspace                 # build everything
cargo test --workspace                  # run the unit tests
cargo run -p imap-extract -- selftest   # run a tool's self-check
cargo run -p forge -- lint imap-extract # validate a manifest
```

CI (GitHub Actions, macOS arm64 — no other infrastructure) rebuilds all targets,
runs the tests, lints manifests, enforces that formulas aren't hand-edited, and
`brew audit`s them.

## License

MIT.
