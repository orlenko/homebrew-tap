# orlenko/homebrew-tap

A Homebrew tap that doubles as a **factory for small, AI-authored CLI tools** —
each compiled to a single self-contained binary, installable with one command.

## Install

```bash
brew install orlenko/tap/<tool>     # e.g. imap-extract
```

That auto-taps `orlenko/tap` and installs. (Or `brew tap orlenko/tap` first.)

## Tools

| Tool | What it does |
| --- | --- |
| [`imap-extract`](tools/imap-extract) | Watch an IMAP folder over IDLE and export new mail as Markdown |

## How it works

- **Rust by default.** Tools live in `tools/<name>/` as Cargo workspace members,
  compile to small binaries (imap-extract is ~2.3 MB), and are auto ad-hoc-signed
  by Apple's `ld64` on the native macOS CI runner — they just run on Apple
  Silicon. Bun/TypeScript is a narrow, gated escape hatch (browser automation,
  npm-only SaaS SDKs); Python lives on `uv`, elsewhere.
- **Declare once.** Each tool's `tool.json` is the source of truth. `forge`
  generates the Homebrew formula from it. Formulas in `Formula/` are
  **generated — never hand-edited** (CI fails on drift).
- **Release = a git tag.** Push `<tool>-vX.Y.Z`; GitHub Actions builds on a
  native macOS arm64 runner, smoke-tests that the binary actually runs, publishes
  a Release with the signed tarball + sha256, regenerates the formula, and commits
  it back.

## Adding a tool

1. Create `tools/<name>/` with a `tool.json`, `Cargo.toml`, and `src/`.
2. Implement a deterministic `selftest` subcommand (it backs the formula's test).
3. `forge lint <name>` to validate the manifest against `brew audit` rules.
4. Tag `<name>-v0.1.0` and push — CI builds, releases, and writes the formula.

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
