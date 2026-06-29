//! forge — the tool factory's scaffolding + Homebrew-formula generator.
//!
//! Source of truth is each tool's `tool.json`, found under `tools/<name>/`
//! (compiled Rust/Bun crates) or `scripts/<name>/` (interpreted bash/python
//! helpers). forge derives the Homebrew formula from it (CI calls `forge
//! formula` on release with the built asset's URL + sha256) and lints the
//! manifest against the rules `brew audit --strict` enforces — so a bad manifest
//! is caught at commit time, not after a wasted build/release.
//!
//! Kept deliberately small. `new`/`dev`/`release` conveniences are layered on
//! later; the load-bearing pieces (generate the formula, lint the manifest) come
//! first.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Roots a tool's directory can live under. `tools/` members are Cargo workspace
/// crates; `scripts/` members are interpreted and deliberately NOT crates (the
/// `members = ["tools/*"]` glob hard-fails on a dir without a Cargo.toml).
const ROOTS: [&str; 2] = ["tools", "scripts"];

#[derive(Parser)]
#[command(
    name = "forge",
    version,
    about = "Scaffolding + formula generator for the tap"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render Formula/<tool>.rb from tool.json + a released asset (CI uses this).
    Formula {
        /// Tool name (a directory under tools/ or scripts/).
        name: String,
        /// Release version, e.g. 0.1.0 (from the git tag).
        #[arg(long)]
        version: String,
        /// Download URL of the released tarball.
        #[arg(long)]
        url: String,
        /// SHA-256 of that tarball.
        #[arg(long)]
        sha256: String,
        /// Write to this path instead of Formula/<tool>.rb.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate tool.json against the rules `brew audit --strict` enforces.
    Lint {
        /// Tool name (a directory under tools/ or scripts/).
        name: String,
    },
}

#[derive(Deserialize)]
struct Manifest {
    name: String,
    lang: String,
    bin: String,
    desc: String,
    homepage: String,
    license: String,
    #[allow(dead_code)]
    #[serde(default)]
    targets: Vec<String>,
    /// Homebrew formula dependencies. Only declare tools genuinely absent on a
    /// clean macOS AND resolvable as formulae (e.g. `gh`, `yazi`) — declaring
    /// `git`/`curl`/`bash`/`python3` forces brew kegs and shadows the system
    /// copies for no gain.
    #[serde(default)]
    depends_on: Vec<String>,
    /// Optional `caveats` text for the formula (e.g. an external, un-tappable
    /// runtime dep the user must install themselves).
    #[serde(default)]
    caveats: Option<String>,
    test: String,
}

/// True for interpreted tools (live under `scripts/`, installed as-is).
fn is_script_lang(lang: &str) -> bool {
    matches!(lang, "bash" | "python")
}

/// Locate a tool's directory under one of the roots. `tools/` wins ties.
fn manifest_dir(name: &str) -> Result<PathBuf> {
    for root in ROOTS {
        let dir = Path::new(root).join(name);
        if dir.join("tool.json").is_file() {
            return Ok(dir);
        }
    }
    bail!("no tool.json for `{name}` under tools/ or scripts/")
}

fn load_manifest(name: &str) -> Result<(Manifest, PathBuf)> {
    let dir = manifest_dir(name)?;
    let path = dir.join("tool.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok((m, dir))
}

/// `imap-extract` -> `ImapExtract` (the Ruby class name brew expects).
fn class_name(name: &str) -> String {
    name.split('-')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let mut c = seg.chars();
            match c.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn render_formula(m: &Manifest, rel_dir: &str, url: &str, sha256: &str) -> String {
    let class = class_name(&m.name);

    // `depends_on "x"` lines, slotted after the license stanza. Empty for a tool
    // with no deps → output stays byte-identical to the pre-script formulae, so
    // the drift gate doesn't flag already-committed formulae as stale.
    let deps_block: String = m
        .depends_on
        .iter()
        .map(|d| format!("  depends_on \"{d}\"\n"))
        .collect();

    // Optional `def caveats` block between install and test. `<<~EOS` strips the
    // common leading indent, so we indent every body line by 6 spaces.
    let caveats_block = match m.caveats.as_deref() {
        Some(c) if !c.trim().is_empty() => {
            let body: String = c
                .lines()
                .map(|l| {
                    if l.is_empty() {
                        "\n".to_string()
                    } else {
                        format!("      {l}\n")
                    }
                })
                .collect();
            format!("\n  def caveats\n    <<~EOS\n{body}    EOS\n  end\n")
        }
        _ => String::new(),
    };

    // No `version` stanza: Homebrew scans the version from the release URL
    // (`.../<tool>-vX.Y.Z/...`), and `brew audit --strict` flags a redundant one.
    format!(
        "# Generated by forge from {rel_dir}/tool.json — do not edit by hand.\n\
         # Re-render via `forge formula {name}` (CI does this on each release).\n\
         class {class} < Formula\n\
         \x20 desc \"{desc}\"\n\
         \x20 homepage \"{homepage}\"\n\
         \x20 url \"{url}\"\n\
         \x20 sha256 \"{sha256}\"\n\
         \x20 license \"{license}\"\n\
         {deps_block}\
         \n\
         \x20 def install\n\
         \x20\x20\x20 bin.install \"{bin}\"\n\
         \x20 end\n\
         {caveats_block}\
         \n\
         \x20 test do\n\
         \x20\x20\x20 system bin/\"{bin}\", \"{test}\"\n\
         \x20 end\n\
         end\n",
        rel_dir = rel_dir,
        name = m.name,
        class = class,
        desc = m.desc,
        homepage = m.homepage,
        url = url,
        sha256 = sha256,
        license = m.license,
        deps_block = deps_block,
        bin = m.bin,
        caveats_block = caveats_block,
        test = m.test,
    )
}

/// Lint the manifest fields against the rules `brew audit --strict` enforces,
/// plus our own license gate. Pure (no filesystem) so it's unit-testable.
fn lint(m: &Manifest) -> Vec<String> {
    let mut errs = Vec::new();

    // Slug: lowercase, starts alpha, no double hyphen, maps to a CamelCase class.
    let slug_ok = !m.name.is_empty()
        && m.name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        && m.name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !m.name.contains("--");
    if !slug_ok {
        errs.push(format!(
            "name `{}` must match ^[a-z][a-z0-9-]*$ with no double hyphen",
            m.name
        ));
    }

    if !matches!(m.lang.as_str(), "rust" | "bun" | "bash" | "python") {
        errs.push(format!(
            "lang `{}` must be one of: rust, bun, bash, python",
            m.lang
        ));
    }

    // The release pipeline tars/builds by the tool's name and `bin.install`s
    // `bin`, so they must match or the released asset won't install.
    if m.bin != m.name {
        errs.push(format!(
            "bin `{}` must equal name `{}` (release tars/builds by name)",
            m.bin, m.name
        ));
    }

    // desc rules (brew audit --strict).
    let d = m.desc.trim();
    if d.is_empty() {
        errs.push("desc is empty".into());
    }
    let first = d
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(first.as_str(), "a" | "an" | "the") {
        errs.push("desc must not start with an article (a/an/the)".into());
    }
    if d.to_ascii_lowercase()
        .contains(&m.name.to_ascii_lowercase())
    {
        errs.push("desc must not contain the formula name (brew prepends it)".into());
    }
    if d.ends_with('.') {
        errs.push("desc must not end with a period".into());
    }
    // brew renders "<name>: <desc>" and caps the whole thing near 80 chars.
    if m.name.len() + 2 + d.len() > 80 {
        errs.push(format!(
            "desc too long: \"<name>: <desc>\" is {} chars (max 80)",
            m.name.len() + 2 + d.len()
        ));
    }

    if !m.homepage.starts_with("https://") {
        errs.push("homepage must be an https:// URL".into());
    }

    // License gate: require a license, reject copyleft (we ship permissive binaries).
    let lic = m.license.to_ascii_uppercase();
    if m.license.trim().is_empty() {
        errs.push("license is required (SPDX id)".into());
    } else if lic.contains("GPL") || lic.contains("AGPL") {
        errs.push(format!(
            "license `{}` is copyleft — the tap ships permissive binaries only",
            m.license
        ));
    }

    for dep in &m.depends_on {
        if dep.trim().is_empty() {
            errs.push("depends_on contains an empty entry".into());
        }
    }

    if m.test.trim().is_empty() {
        errs.push("test is empty (name a deterministic subcommand, e.g. \"selftest\")".into());
    }

    errs
}

/// Filesystem-dependent checks: the tool lives under the right root for its
/// lang, and (for scripts) the file exists, is executable, and has a portable
/// `env` shebang. Split from `lint` so the field rules stay pure/testable.
fn lint_files(m: &Manifest, dir: &Path) -> Vec<String> {
    let mut errs = Vec::new();
    let is_script = is_script_lang(&m.lang);
    let in_scripts = dir.starts_with("scripts");

    if is_script && !in_scripts {
        errs.push(format!(
            "lang `{}` must live under scripts/, not {}",
            m.lang,
            dir.display()
        ));
    }
    if !is_script && in_scripts {
        errs.push(format!(
            "lang `{}` is compiled and must live under tools/, not scripts/ (it would never be built or shipped)",
            m.lang
        ));
    }

    if is_script {
        let f = dir.join(&m.bin);
        match std::fs::read(&f) {
            Err(_) => errs.push(format!("script file {} is missing", f.display())),
            Ok(bytes) => {
                match std::fs::metadata(&f) {
                    Ok(meta) if meta.permissions().mode() & 0o111 == 0 => {
                        errs.push(format!(
                            "script {} is not executable (chmod +x)",
                            f.display()
                        ));
                    }
                    _ => {}
                }
                let first = bytes.split(|&b| b == b'\n').next().unwrap_or(&[]);
                let first = String::from_utf8_lossy(first);
                if !first.starts_with("#!/usr/bin/env ") {
                    errs.push(format!(
                        "script {} must start with a portable shebang (#!/usr/bin/env ...), found: {}",
                        f.display(),
                        first.trim_end()
                    ));
                }
            }
        }
    }

    errs
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Formula {
            name,
            // Accepted for forward-compat (URL-less assets) but unused: brew
            // scans the version from the release URL.
            version: _,
            url,
            sha256,
            out,
        } => {
            let (m, dir) = load_manifest(&name)?;
            let mut problems = lint(&m);
            problems.extend(lint_files(&m, &dir));
            if !problems.is_empty() {
                for p in &problems {
                    eprintln!("forge: lint: {p}");
                }
                bail!(
                    "{} has {} lint problem(s); fix tool.json first",
                    name,
                    problems.len()
                );
            }
            let rel_dir = dir.to_string_lossy().replace('\\', "/");
            let rb = render_formula(&m, &rel_dir, &url, &sha256);
            let out = out.unwrap_or_else(|| Path::new("Formula").join(format!("{name}.rb")));
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out, rb)?;
            println!("forge: wrote {}", out.display());
        }
        Command::Lint { name } => {
            let (m, dir) = load_manifest(&name)?;
            let mut problems = lint(&m);
            problems.extend(lint_files(&m, &dir));
            if problems.is_empty() {
                println!("forge: {name} tool.json OK");
            } else {
                for p in &problems {
                    eprintln!("forge: lint: {p}");
                }
                bail!("{} has {} lint problem(s)", name, problems.len());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(desc: &str, license: &str) -> Manifest {
        Manifest {
            name: "imap-extract".into(),
            lang: "rust".into(),
            bin: "imap-extract".into(),
            desc: desc.into(),
            homepage: "https://github.com/orlenko/homebrew-tap".into(),
            license: license.into(),
            targets: vec![],
            depends_on: vec![],
            caveats: None,
            test: "selftest".into(),
        }
    }

    #[test]
    fn class_name_camelcases() {
        assert_eq!(class_name("imap-extract"), "ImapExtract");
        assert_eq!(class_name("forge"), "Forge");
        assert_eq!(class_name("a-b-c"), "ABC");
    }

    #[test]
    fn lint_accepts_a_good_manifest() {
        let m = manifest(
            "Watch an IMAP folder and export new mail as Markdown",
            "MIT",
        );
        assert!(lint(&m).is_empty(), "{:?}", lint(&m));
    }

    #[test]
    fn lint_accepts_script_langs() {
        let mut m = manifest("Print one file from an un-checked-out GitHub repo", "MIT");
        m.name = "ghcat".into();
        m.bin = "ghcat".into();
        m.lang = "bash".into();
        assert!(lint(&m).is_empty(), "{:?}", lint(&m));
        m.lang = "python".into();
        assert!(lint(&m).is_empty(), "{:?}", lint(&m));
    }

    #[test]
    fn lint_rejects_copyleft() {
        let errs = lint(&manifest("Watch mail and export as Markdown", "GPL-3.0"));
        assert!(errs.iter().any(|e| e.contains("copyleft")), "{errs:?}");
    }

    #[test]
    fn lint_rejects_article_name_and_period() {
        let errs = lint(&manifest("The imap-extract tool does stuff.", "MIT"));
        assert!(errs.iter().any(|e| e.contains("article")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("formula name")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("period")), "{errs:?}");
    }

    #[test]
    fn lint_rejects_empty_depends_on_entry() {
        let mut m = manifest("Watch mail and export as Markdown", "MIT");
        m.depends_on = vec!["gh".into(), "  ".into()];
        let errs = lint(&m);
        assert!(errs.iter().any(|e| e.contains("empty entry")), "{errs:?}");
    }

    #[test]
    fn render_is_valid_shape() {
        let rb = render_formula(
            &manifest("Watch mail and export as Markdown", "MIT"),
            "tools/imap-extract",
            "https://x/y.tar.gz",
            "abc123",
        );
        assert!(rb.contains("class ImapExtract < Formula"));
        assert!(rb.contains("sha256 \"abc123\""));
        assert!(rb.contains("system bin/\"imap-extract\", \"selftest\""));
        // No deps / no caveats → none of those stanzas leak in.
        assert!(!rb.contains("depends_on"));
        assert!(!rb.contains("def caveats"));
    }

    #[test]
    fn render_emits_depends_on_and_caveats() {
        let mut m = manifest("Web search via mgrep, capped output", "MIT");
        m.name = "mgw".into();
        m.bin = "mgw".into();
        m.lang = "bash".into();
        m.test = "--help".into();
        m.depends_on = vec!["gh".into()];
        m.caveats = Some("Needs mgrep: npm install -g @mixedbread/mgrep".into());
        let rb = render_formula(&m, "scripts/mgw", "https://x/mgw-noarch.tar.gz", "deadbeef");
        assert!(rb.contains("class Mgw < Formula"));
        assert!(
            rb.contains("  license \"MIT\"\n  depends_on \"gh\"\n\n  def install"),
            "deps must sit between license and install:\n{rb}"
        );
        assert!(
            rb.contains("  end\n\n  def caveats\n    <<~EOS\n      Needs mgrep: npm install -g @mixedbread/mgrep\n    EOS\n  end\n\n  test do"),
            "caveats must sit between install and test:\n{rb}"
        );
        assert!(rb.contains("from scripts/mgw/tool.json"));
    }
}
