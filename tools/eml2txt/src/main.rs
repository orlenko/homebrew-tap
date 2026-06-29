//! eml2txt — read saved `.eml` messages: print the body, save attachments.
//!
//! Rust port of the original Python tool. For each `.eml` it prints the key
//! headers + the decoded text/plain body (falling back to a stripped text/html
//! part), then writes any attachments to a sibling `_attachments/` directory.
//!
//! Deps: mail-parser (MIME). The HTML→text stripper is hand-rolled to keep the
//! binary small and the behavior matching the original's naive stripper.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mail_parser::{MessageParser, MimeHeaders, PartType};

const HELP: &str = "\
eml2txt — read saved .eml messages: print the body, save attachments.

  eml2txt <file.eml> [more.eml ...]

For each .eml: prints key headers + the decoded text/plain body (falling back to
a stripped text/html part if there's no plaintext), then writes any attachments
to a sibling `_attachments/` directory next to the .eml and echoes their names.

Options:
  -l, --list   list attachments without saving them
  -h, --help   this help

Exits non-zero if any file is missing or cannot be parsed.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Deterministic self-check (no filesystem) — backs the formula `test do`.
    if args.len() == 1 && args[0] == "selftest" {
        return match selftest() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eml2txt: selftest: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let mut save = true;
    let mut files: Vec<String> = Vec::new();
    for a in &args {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "-l" | "--list" => save = false,
            "--" => continue,
            s if s.starts_with('-') && s != "-" => {
                eprintln!("eml2txt: unknown flag '{s}'");
                return ExitCode::from(2);
            }
            s => files.push(s.to_string()),
        }
    }

    if files.is_empty() {
        eprintln!("{}", HELP.lines().next().unwrap_or(""));
        eprintln!("usage: eml2txt [-l] <file.eml> [more.eml ...]");
        return ExitCode::from(2);
    }

    let mut rc = ExitCode::SUCCESS;
    for (i, path) in files.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if let Err(e) = process(Path::new(path), save) {
            eprintln!("eml2txt: {path}: {e}");
            rc = ExitCode::FAILURE;
        }
    }
    rc
}

// ---------------------------------------------------------------------------
// Per-file processing
// ---------------------------------------------------------------------------
fn process(path: &Path, save: bool) -> anyhow::Result<()> {
    if !path.is_file() {
        anyhow::bail!("no such file");
    }
    let bytes = std::fs::read(path)?;
    let msg = MessageParser::default()
        .parse(bytes.as_slice())
        .ok_or_else(|| anyhow::anyhow!("could not parse message"))?;

    println!("=== {} ===", path.display());
    let headers: [(&str, Option<String>); 5] = [
        ("From", render_address(msg.from())),
        ("To", render_address(msg.to())),
        ("Cc", render_address(msg.cc())),
        ("Date", msg.date().map(|d| d.to_rfc3339())),
        ("Subject", msg.subject().map(collapse_ws)),
    ];
    for (name, val) in &headers {
        if let Some(v) = val
            && !v.is_empty()
        {
            println!("{name}: {v}");
        }
    }
    println!();

    // Prefer a genuine text/plain body. mail-parser lists an HTML part under
    // text_body too and would flatten it with its own converter, so when the
    // chosen body is HTML we strip it ourselves to keep the block structure
    // (matching the original Python's get_body(plain, html) preference).
    let body = if matches!(msg.text_part(0).map(|p| &p.body), Some(PartType::Text(_))) {
        msg.body_text(0).map(|t| t.trim_end().to_string())
    } else if let Some(html) = msg.body_html(0) {
        Some(html_to_text(&html))
    } else {
        msg.body_text(0).map(|t| t.trim_end().to_string())
    };
    println!("{}", body.as_deref().unwrap_or("(no readable body)"));

    let attachments: Vec<_> = msg.attachments().collect();
    if !attachments.is_empty() {
        // dirname(abspath(path)) — make absolute without resolving symlinks.
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let attach_dir = abs
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("_attachments");

        println!("\n--- attachments ({}) ---", attachments.len());
        for part in &attachments {
            let fname = part.attachment_name();
            if save {
                std::fs::create_dir_all(&attach_dir)?;
                let dest = unique_path(&attach_dir, fname);
                std::fs::write(&dest, part.contents())?;
                println!("saved: {}", rel_to_cwd(&dest));
            } else {
                println!("attachment: {}", fname.unwrap_or("(unnamed)"));
            }
        }
    }

    Ok(())
}

/// Render an address header as `Name <email>` parts joined with `, `.
fn render_address(a: Option<&mail_parser::Address>) -> Option<String> {
    let parts: Vec<String> = a?
        .iter()
        .filter_map(|addr| {
            let email = addr.address.as_deref().unwrap_or("");
            match addr.name.as_deref() {
                Some(name) if !name.is_empty() => Some(format!("{name} <{email}>")),
                _ if !email.is_empty() => Some(email.to_string()),
                _ => None,
            }
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Collapse every run of whitespace to a single space and trim (header values).
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A non-clobbering path inside `dir` for `name` (basename only, never escapes).
fn unique_path(dir: &Path, name: Option<&str>) -> PathBuf {
    let raw = name.unwrap_or("attachment");
    let base = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment");
    let (stem, ext) = split_ext(base);
    let mut candidate = dir.join(base);
    let mut i = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    candidate
}

/// Split a filename into (stem, ext) like Python's `os.path.splitext`:
/// leading dots belong to the stem; ext is the final `.suffix` (or empty).
fn split_ext(name: &str) -> (String, String) {
    let trimmed = name.trim_start_matches('.');
    let lead_len = name.len() - trimmed.len();
    match trimmed.rfind('.') {
        Some(idx) if idx > 0 => (
            format!("{}{}", &name[..lead_len], &trimmed[..idx]),
            trimmed[idx..].to_string(),
        ),
        _ => (name.to_string(), String::new()),
    }
}

/// Display `p` relative to the current directory when it sits underneath it.
fn rel_to_cwd(p: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| p.display().to_string())
}

// ---------------------------------------------------------------------------
// HTML → text (faithful to the original's naive stripper)
// ---------------------------------------------------------------------------
const SKIP_TAGS: [&str; 4] = ["script", "style", "head", "title"];
const BLOCK_TAGS: [&str; 18] = [
    "p",
    "div",
    "br",
    "li",
    "tr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "ul",
    "ol",
    "table",
    "blockquote",
    "pre",
];

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut skip_depth = 0usize;
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        // Text before the tag.
        if skip_depth == 0 {
            out.push_str(&decode_entities(&rest[..lt]));
        }
        rest = &rest[lt..];

        // HTML comment: skip to the matching `-->`.
        if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(end) => rest = &after[end + 3..],
                None => {
                    rest = "";
                    break;
                }
            }
            continue;
        }

        let Some(gt) = rest.find('>') else {
            // Unterminated tag: treat the remainder as text.
            if skip_depth == 0 {
                out.push_str(&decode_entities(&rest[1..]));
            }
            rest = "";
            break;
        };
        let inner = &rest[1..gt];
        rest = &rest[gt + 1..];

        let is_close = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();

        if SKIP_TAGS.contains(&name.as_str()) {
            if is_close {
                skip_depth = skip_depth.saturating_sub(1);
            } else if !inner.trim_end().ends_with('/') {
                skip_depth += 1;
            }
        } else if BLOCK_TAGS.contains(&name.as_str()) {
            // Both open and close of a block element imply a line break.
            out.push('\n');
        }
    }
    if skip_depth == 0 {
        out.push_str(&decode_entities(rest));
    }
    cleanup_ws(&out)
}

/// Decode the HTML entities that actually show up in mail. Unknown named
/// entities are left verbatim (matches the original's pragmatic coverage).
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Entity names/numeric refs are short and ASCII; cap the search window,
        // clamped DOWN to a char boundary so a multi-byte char straddling the
        // cap can't panic the slice (real email is full of em-dashes/emoji).
        let after = &rest[1..];
        let mut end = after.len().min(32);
        while end > 0 && !after.is_char_boundary(end) {
            end -= 1;
        }
        let window = &after[..end];
        if let Some(semi) = window.find(';')
            && let Some(ch) = resolve_entity(&window[..semi])
        {
            out.push(ch);
            rest = &rest[1 + semi + 1..];
            continue;
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn resolve_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "mdash" => Some('\u{2014}'),
        "ndash" => Some('\u{2013}'),
        "hellip" => Some('\u{2026}'),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "copy" => Some('\u{00A9}'),
        "reg" => Some('\u{00AE}'),
        "trade" => Some('\u{2122}'),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Collapse runs of spaces/tabs to one space, strip whitespace after a newline,
/// cap blank runs at one blank line, and trim — mirrors the original's regexes.
fn cleanup_ws(s: &str) -> String {
    // Pass 1: collapse [ \t]+ → single space.
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            prev_space = false;
            collapsed.push(ch);
        }
    }

    // Pass 2: a run of newlines (plus any spaces among them) → at most 2
    // newlines, dropping the spaces (covers "\n  " leading indent + "\n\n\n").
    let chars: Vec<char> = collapsed.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\n' {
            let mut nl = 0;
            while i < chars.len() && (chars[i] == '\n' || chars[i] == ' ' || chars[i] == '\t') {
                if chars[i] == '\n' {
                    nl += 1;
                }
                i += 1;
            }
            for _ in 0..nl.min(2) {
                out.push('\n');
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Selftest
// ---------------------------------------------------------------------------
fn selftest() -> anyhow::Result<()> {
    let sample = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Hello World\r\n\
Date: Tue, 1 Jan 2030 12:00:00 +0000\r\n\
Content-Type: text/html\r\n\r\n\
<h1>Hi</h1><p>Body &amp; &#39;more&#39;</p>\r\n";

    let msg = MessageParser::default()
        .parse(sample.as_slice())
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if msg.subject() != Some("Hello World") {
        anyhow::bail!("subject mismatch: {:?}", msg.subject());
    }
    let from = render_address(msg.from());
    if from.as_deref() != Some("Alice <alice@example.com>") {
        anyhow::bail!("from render mismatch: {from:?}");
    }
    let html = msg
        .body_html(0)
        .ok_or_else(|| anyhow::anyhow!("no html body"))?;
    let text = html_to_text(&html);
    if !text.contains("Body & 'more'") {
        anyhow::bail!("html strip / entity decode failed: {text:?}");
    }
    if split_ext("archive.tar.gz") != ("archive.tar".to_string(), ".gz".to_string()) {
        anyhow::bail!("split_ext mismatch");
    }
    println!("eml2txt selftest: OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_decodes_entities() {
        let t = html_to_text("<h1>Hi</h1><p>a &amp; b &lt;c&gt;</p><script>x=1</script>");
        // </h1> and <p> each emit a newline → one blank line between them.
        assert_eq!(t, "Hi\n\na & b <c>");
    }

    #[test]
    fn skips_script_style_head_title() {
        let t = html_to_text("<head><title>T</title></head><body>Body<style>p{}</style></body>");
        assert_eq!(t, "Body");
    }

    #[test]
    fn numeric_entities() {
        assert_eq!(decode_entities("a&#39;b&#x41;c"), "a'bAc");
        assert_eq!(decode_entities("plain & loose"), "plain & loose");
    }

    #[test]
    fn entity_window_multibyte_no_panic() {
        // '&' then 30 ASCII chars then a multi-byte char straddling the 32-byte
        // cap — must not panic; with no ';' the '&' stays literal.
        let s = format!("&{}\u{20AC} ok", "x".repeat(30));
        assert_eq!(decode_entities(&s), s);
        // A real entity still decodes with trailing multibyte content present.
        assert_eq!(decode_entities("a &amp; \u{20AC} b"), "a & \u{20AC} b");
    }

    #[test]
    fn split_ext_matches_python() {
        assert_eq!(split_ext("a.txt"), ("a".into(), ".txt".into()));
        assert_eq!(
            split_ext("archive.tar.gz"),
            ("archive.tar".into(), ".gz".into())
        );
        assert_eq!(split_ext(".bashrc"), (".bashrc".into(), "".into()));
        assert_eq!(split_ext("noext"), ("noext".into(), "".into()));
    }

    #[test]
    fn collapse_ws_normalizes() {
        assert_eq!(collapse_ws("  a\t b\n c "), "a b c");
    }

    #[test]
    fn cleanup_caps_blank_lines_and_indent() {
        assert_eq!(cleanup_ws("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(cleanup_ws("a\n   b"), "a\nb");
    }

    #[test]
    fn unique_path_increments_on_collision() {
        let dir = std::env::temp_dir().join(format!("eml2txt-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p0 = unique_path(&dir, Some("doc.pdf"));
        assert!(p0.ends_with("doc.pdf"));
        std::fs::write(&p0, b"x").unwrap();
        let p1 = unique_path(&dir, Some("doc.pdf"));
        assert!(p1.ends_with("doc (1).pdf"), "{p1:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
