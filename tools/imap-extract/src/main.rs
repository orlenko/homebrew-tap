//! imap-extract — watch an IMAP folder/label over IDLE and export new mail as
//! Markdown (plus attachment subdirectories). Configuration is per-directory: it
//! reads a `.env` from the current directory and keeps its sync pointer in a
//! local `state.json`, so several watchers in different folders never interfere.
//!
//! Rust rewrite of the original TypeScript tool. Deps: async-imap (IDLE),
//! mail-parser (MIME), htmd (HTML→Markdown).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures::StreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use toolkit::config;
use toolkit::log::Logger;

/// Concrete session type: both implicit-TLS and STARTTLS paths converge here.
type ImapSession = async_imap::Session<TlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "imap-extract",
    version,
    about = "Watch an IMAP folder/label and dump new mail as Markdown.",
    long_about = "Configuration is per-directory: it reads a .env from the current directory. \
Each folder can point at a different IMAP server. Sync state lives in ./state.json.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    watch: WatchArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Args)]
struct WatchArgs {
    /// IMAP folder/label to watch, e.g. "Labels/FSR" or "INBOX" (falls back to IMAP_FOLDER).
    folder: Option<String>,
    /// Where to write .md + attachments (falls back to TARGET_DIR, then the current dir).
    target_dir: Option<PathBuf>,
    /// Sync once and exit (no live IDLE loop). Good for cron/scripts.
    #[arg(long)]
    once: bool,
    /// Config file to read instead of ./.env (or $IMAP_EXTRACTOR_ENV).
    #[arg(long = "env-file", value_name = "PATH")]
    env_file: Option<PathBuf>,
    /// Print the resolved server/folder/target/state paths and exit.
    #[arg(long = "print-config")]
    print_config: bool,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run a self-check (parse a fixture, HTML→Markdown — no network) and exit.
    Selftest,
}

// ---------------------------------------------------------------------------
// Resolved configuration
// ---------------------------------------------------------------------------
struct Cfg {
    host: Option<String>,
    port: u16,
    secure: bool,
    starttls: bool,
    tls_reject_unauthorized: bool,
    user: Option<String>,
    password: Option<String>,
    folder: String,
    target_dir: PathBuf,
    state_file: PathBuf,
    env_file: PathBuf,
    env_loaded: bool,
    tag: String,
    once: bool,
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn expand_home(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    if s == "~" {
        return dirs_home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return dirs_home().join(rest);
    }
    p
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn resolve(args: &WatchArgs) -> Result<Cfg> {
    let env_file = args
        .env_file
        .clone()
        .or_else(|| std::env::var_os("IMAP_EXTRACTOR_ENV").map(PathBuf::from))
        .unwrap_or_else(|| cwd().join(".env"));
    let env_file = expand_home(env_file);
    let env_loaded = config::load_env(&env_file).unwrap_or(false);

    let folder = args
        .folder
        .clone()
        .or_else(|| config::get("IMAP_FOLDER"))
        .ok_or_else(|| {
            anyhow!(
                "no IMAP folder given. Pass it as the first argument (e.g. `imap-extract Labels/FSR`) \
or set IMAP_FOLDER in this directory's .env{}.",
                if env_loaded {
                    String::new()
                } else {
                    format!(" (no .env found at {})", env_file.display())
                }
            )
        })?;

    let target_dir = args
        .target_dir
        .clone()
        .or_else(|| config::get("TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(cwd);
    let target_dir = std::fs::canonicalize(&target_dir).unwrap_or(target_dir);

    let tag = folder
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&folder)
        .to_string();

    Ok(Cfg {
        host: config::get("IMAP_HOST"),
        port: config::get_int("IMAP_PORT", 993) as u16,
        secure: config::get_bool("IMAP_SECURE", true),
        starttls: config::get_bool("IMAP_STARTTLS", false),
        tls_reject_unauthorized: config::get_bool("IMAP_TLS_REJECT_UNAUTHORIZED", true),
        user: config::get("IMAP_USER"),
        password: config::get("IMAP_PASSWORD"),
        folder,
        target_dir,
        state_file: cwd().join("state.json"),
        env_file,
        env_loaded,
        tag,
        once: args.once,
    })
}

fn print_config(cfg: &Cfg) {
    println!("Resolved configuration:");
    println!(
        "  env file    : {}{}",
        cfg.env_file.display(),
        if cfg.env_loaded { "" } else { " (not found)" }
    );
    println!(
        "  IMAP server : {}@{}:{}",
        cfg.user.as_deref().unwrap_or("?"),
        cfg.host.as_deref().unwrap_or("?"),
        cfg.port
    );
    println!("  folder      : {}", cfg.folder);
    println!("  target dir  : {}", cfg.target_dir.display());
    println!("  state file  : {}", cfg.state_file.display());
}

// ---------------------------------------------------------------------------
// Sync state (local to the directory, just like the .env)
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Default)]
struct State {
    uid_validity: Option<u32>,
    last_seen_uid: u32,
}

fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------
fn make_connector(accept_invalid: bool) -> Result<tokio_native_tls::TlsConnector> {
    let mut builder = native_tls::TlsConnector::builder();
    if accept_invalid {
        builder
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true);
    }
    Ok(tokio_native_tls::TlsConnector::from(builder.build()?))
}

/// Read a single CRLF-terminated line off a raw TCP stream (used only for the
/// few STARTTLS control lines before the TLS handshake — kept byte-at-a-time so
/// nothing is buffered past the tagged OK, which would desync the handshake).
async fn read_line(tcp: &mut TcpStream) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Perform the STARTTLS command exchange on a plaintext connection, leaving the
/// socket ready for the TLS handshake. Kept at the raw-TCP level so nothing is
/// buffered past the tagged OK (which would desync the handshake).
async fn starttls_handshake(tcp: &mut TcpStream) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let _greeting = read_line(tcp).await?;
    tcp.write_all(b"A1 STARTTLS\r\n").await?;
    loop {
        let line = read_line(tcp).await?;
        if line.is_empty() {
            bail!("connection closed during STARTTLS");
        }
        if let Some(rest) = line.strip_prefix("A1 ") {
            if rest.trim_start().to_ascii_uppercase().starts_with("OK") {
                break;
            }
            bail!("STARTTLS rejected: {}", line.trim());
        }
    }
    Ok(())
}

async fn connect(cfg: &Cfg) -> Result<ImapSession> {
    let host = cfg.host.as_deref().context("missing IMAP_HOST")?;
    let user = cfg.user.as_deref().context("missing IMAP_USER")?;
    let pass = cfg.password.as_deref().context("missing IMAP_PASSWORD")?;
    if !cfg.starttls && !cfg.secure {
        bail!("plain (non-TLS) IMAP is unsupported; set IMAP_SECURE=true or IMAP_STARTTLS=true");
    }
    let connector = make_connector(!cfg.tls_reject_unauthorized)?;

    let addr = format!("{host}:{}", cfg.port);
    let mut tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;

    if cfg.starttls {
        starttls_handshake(&mut tcp).await?;
    }
    let tls_stream = connector
        .connect(host, tcp)
        .await
        .context("TLS handshake")?;

    let client = async_imap::Client::new(tls_stream);
    let session = client
        .login(user, pass)
        .await
        .map_err(|(e, _)| anyhow!("LOGIN failed: {e}"))?;
    Ok(session)
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------
async fn fetch_source(session: &mut ImapSession, uid: u32) -> Result<Option<Vec<u8>>> {
    let mut stream = session.uid_fetch(uid.to_string(), "RFC822").await?;
    let mut body: Option<Vec<u8>> = None;
    // Drain the whole stream (including the tagged completion) so the next
    // command doesn't read stale data; keep the first body part.
    while let Some(item) = stream.next().await {
        let fetch = item?;
        if body.is_none()
            && let Some(b) = fetch.body()
        {
            body = Some(b.to_vec());
        }
    }
    Ok(body)
}

async fn sync(session: &mut ImapSession, cfg: &Cfg, log: &Logger) -> Result<()> {
    log.info(&format!("Selecting folder: \"{}\"...", cfg.folder));
    let mailbox = session.select(&cfg.folder).await?;
    let current_uidvalidity = mailbox.uid_validity;

    let mut state = load_state(&cfg.state_file);

    // UIDVALIDITY changed or uninitialized → baseline to the highest current UID
    // so we never flood with history.
    if state.uid_validity != current_uidvalidity {
        log.info("UIDVALIDITY changed or uninitialized. Baselining to highest current UID.");
        state.uid_validity = current_uidvalidity;
        state.last_seen_uid = if mailbox.exists > 0 {
            let uids: HashSet<u32> = session.uid_search("ALL").await?;
            uids.into_iter().max().unwrap_or(0)
        } else {
            0
        };
        save_state(&cfg.state_file, &state)?;
        log.info(&format!(
            "Sync pointer initialized to UID {}",
            state.last_seen_uid
        ));
        return Ok(());
    }

    let last = state.last_seen_uid;
    log.info(&format!("Syncing new messages since UID {last}"));

    let mut uids: Vec<u32> = session
        .uid_search(format!("UID {}:*", last + 1))
        .await?
        .into_iter()
        .filter(|u| *u > last)
        .collect();
    uids.sort_unstable();

    if uids.is_empty() {
        log.info("No new messages found.");
        return Ok(());
    }
    log.info(&format!("Found {} new message(s) to process.", uids.len()));

    for uid in uids {
        log.info(&format!("Fetching message UID {uid}..."));
        if let Some(source) = fetch_source(session, uid).await? {
            process_message(&source, &cfg.target_dir, uid, log).await?;
        }
        // Persist immediately after a successful write so a crash never re-dumps.
        state.last_seen_uid = uid;
        save_state(&cfg.state_file, &state)?;
    }

    log.info("Synchronization complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Email → Markdown
// ---------------------------------------------------------------------------
fn esc(s: &str) -> String {
    s.replace('"', "\\\"")
}

fn addr_to_string(a: Option<&mail_parser::Address>) -> String {
    let Some(addr) = a.and_then(|a| a.first()) else {
        return "Unknown".to_string();
    };
    let email = addr.address.as_deref().unwrap_or("");
    match addr.name.as_deref() {
        Some(name) if !name.is_empty() => format!("{name} <{email}>"),
        _ => email.to_string(),
    }
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_whitespace() {
            out.push('-');
        } else if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        }
    }
    // Collapse runs of '-' and trim from both ends.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_dash = false;
    for ch in out.chars() {
        if ch == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(ch);
            prev_dash = false;
        }
    }
    collapsed.trim_matches('-').to_string()
}

/// Slugify a subject into a filename component, capped so the final filename
/// stays under the 255-byte-per-component limit (macOS rejects longer names
/// with ENAMETOOLONG / os error 63). slugify output is ASCII, so truncating by
/// bytes never splits a char; we trim a dash left dangling at the cut.
fn slug_for_filename(subject: &str) -> String {
    const MAX_SLUG: usize = 80;
    let mut s = slugify(subject);
    if s.len() > MAX_SLUG {
        s.truncate(MAX_SLUG);
        while s.ends_with('-') {
            s.pop();
        }
    }
    if s.is_empty() {
        "no-subject".to_string()
    } else {
        s
    }
}

fn sanitize_filename(text: &str) -> String {
    text.chars()
        .map(|c| if "/\\:*?\"<>|\0".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

fn date_for_filename(d: Option<&mail_parser::DateTime>) -> String {
    match d {
        Some(d) => format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        ),
        None => jiff::Zoned::now().strftime("%Y-%m-%d_%H-%M-%S").to_string(),
    }
}

fn unique_path(dir: &Path, base: &str, ext: &str) -> (PathBuf, u32) {
    let mut suffix = 0u32;
    let mut path = dir.join(format!("{base}{ext}"));
    while path.exists() {
        suffix += 1;
        path = dir.join(format!("{base}_{suffix}{ext}"));
    }
    (path, suffix)
}

fn to_markdown(html: &str) -> String {
    htmd::convert(html).unwrap_or_else(|_| html.to_string())
}

async fn process_message(source: &[u8], target_dir: &Path, uid: u32, log: &Logger) -> Result<()> {
    let msg = MessageParser::default()
        .parse(source)
        .context("parsing email")?;

    let subject = msg.subject().unwrap_or("No Subject").to_string();
    let date_str = msg
        .date()
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "Unknown Date".to_string());
    let from_str = addr_to_string(msg.from());
    let to_str = addr_to_string(msg.to());

    log.info(&format!("Parsing email: \"{subject}\""));

    let body_md = match msg.body_html(0) {
        Some(html) => to_markdown(html.as_ref()),
        None => msg.body_text(0).map(|c| c.into_owned()).unwrap_or_default(),
    };

    let md = format!(
        "---\nsubject: \"{}\"\nfrom: \"{}\"\ndate: \"{}\"\nto: \"{}\"\n---\n\n# {}\n\n**From:** {}\n**Date:** {}\n**To:** {}\n\n---\n\n{}\n",
        esc(&subject),
        esc(&from_str),
        esc(&date_str),
        esc(&to_str),
        subject,
        from_str,
        date_str,
        to_str,
        body_md
    );

    let date_part = date_for_filename(msg.date());
    let slug = slug_for_filename(&subject);
    let base = format!("{date_part}_{uid}_{slug}");

    tokio::fs::create_dir_all(target_dir).await?;
    let (md_path, suffix) = unique_path(target_dir, &base, ".md");
    tokio::fs::write(&md_path, md).await?;
    log.info(&format!(
        "Saved Markdown file: {}",
        md_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));

    let attachments: Vec<_> = msg.attachments().collect();
    if !attachments.is_empty() {
        let folder_suffix = if suffix > 0 {
            format!("_{suffix}")
        } else {
            String::new()
        };
        let att_dir = target_dir.join(format!("{base}{folder_suffix} attachments"));
        tokio::fs::create_dir_all(&att_dir).await?;
        for att in attachments {
            let name = sanitize_filename(att.attachment_name().unwrap_or("unnamed_attachment"));
            let bytes = att.contents();
            tokio::fs::write(att_dir.join(&name), bytes).await?;
            log.info(&format!("Saved attachment: {name} ({} bytes)", bytes.len()));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------
async fn connect_and_run(cfg: &Cfg, log: &Logger) -> Result<()> {
    log.info("Connecting to mail server...");
    let mut session = connect(cfg).await?;

    sync(&mut session, cfg, log).await?;

    if cfg.once {
        let _ = session.logout().await;
        log.info("--once given; sync complete, exiting.");
        return Ok(());
    }

    loop {
        log.info("Entering IDLE mode. Waiting for updates from server...");
        let mut handle = session.idle();
        handle.init().await?;
        let (idle_fut, _stop) = handle.wait_with_timeout(Duration::from_secs(29 * 60));
        let outcome = idle_fut.await?;
        session = handle.done().await?;
        log.info(&format!("Server update ({outcome:?}). Running sync..."));
        sync(&mut session, cfg, log).await?;
    }
}

async fn serve(cfg: &Cfg, log: &Logger) -> Result<()> {
    loop {
        match connect_and_run(cfg, log).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                log.error(&format!("Connection lost or failed, retrying in 15s: {e}"));
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------
// Selftest (deterministic, no network) — backs the formula `test do`.
// ---------------------------------------------------------------------------
fn selftest() -> Result<()> {
    let sample = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Hello World\r\n\
Date: Tue, 1 Jan 2030 12:00:00 +0000\r\n\
Content-Type: text/html\r\n\r\n\
<h1>Hi</h1><p>Body text</p>\r\n";

    let msg = MessageParser::default()
        .parse(sample.as_slice())
        .context("selftest: parse failed")?;
    if msg.subject() != Some("Hello World") {
        bail!("selftest: subject mismatch: {:?}", msg.subject());
    }
    let html = msg.body_html(0).context("selftest: no html body")?;
    let md = to_markdown(html.as_ref());
    if !md.contains("# Hi") {
        bail!("selftest: HTML→Markdown failed: {md:?}");
    }
    if slugify("Hello, World!  ") != "hello-world" {
        bail!(
            "selftest: slugify mismatch: {:?}",
            slugify("Hello, World!  ")
        );
    }
    if sanitize_filename("a/b:c?.txt") != "a_b_c_.txt" {
        bail!(
            "selftest: sanitize mismatch: {:?}",
            sanitize_filename("a/b:c?.txt")
        );
    }
    println!("imap-extract selftest: OK");
    Ok(())
}

// ---------------------------------------------------------------------------
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Selftest) = cli.command {
        return selftest();
    }

    let cfg = resolve(&cli.watch)?;
    let log = Logger::new(&cfg.tag);

    if cli.watch.print_config {
        print_config(&cfg);
        return Ok(());
    }

    log.info("Starting IMAP Markdown Extractor...");
    log.info(&format!(
        "Folder: \"{}\"  ->  {}",
        cfg.folder,
        cfg.target_dir.display()
    ));
    log.info(&format!("State:  {}", cfg.state_file.display()));

    tokio::select! {
        r = serve(&cfg, &log) => r,
        _ = shutdown_signal() => {
            log.info("Shutting down...");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_rules() {
        assert_eq!(slugify("Hello, World!  "), "hello-world");
        assert_eq!(slugify("  Multiple   Spaces "), "multiple-spaces");
        assert_eq!(slugify("UPPER_case-1"), "upper_case-1");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn slug_for_filename_caps_and_trims() {
        let long = "word ".repeat(100); // ~500 chars
        let s = slug_for_filename(&long);
        assert!(s.len() <= 80, "len {}", s.len());
        assert!(!s.ends_with('-'), "trailing dash: {s:?}");
    }

    #[test]
    fn slug_for_filename_empty_fallback() {
        assert_eq!(slug_for_filename(""), "no-subject");
        assert_eq!(slug_for_filename("•••"), "no-subject"); // all dropped
    }

    #[test]
    fn slug_for_filename_real_subject_stays_under_os_limit() {
        // The exact subject that produced ENAMETOOLONG (os error 63) at UID 312.
        let subject = "3465 Chemin de la Côte des Neiges - AVIS IMPORTANT - Élection \
du conseil d'administration - Assemblée générale spéciale VIRTUELLE le 18 novembre \
2024 / IMPORTANT NOTICE - Board members election - VIRTUAL general special meeting \
on November 18th, 2024";
        let slug = slug_for_filename(subject);
        // Worst-case component: date(19) + "_" + uid(<=10) + "_" + slug + " attachments".
        let worst = 19 + 1 + 10 + 1 + slug.len() + " attachments".len();
        assert!(
            worst < 255,
            "worst-case filename component is {worst} bytes"
        );
    }

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(sanitize_filename("a/b:c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_filename("clean.pdf"), "clean.pdf");
    }

    #[test]
    fn esc_escapes_quotes() {
        assert_eq!(esc("a\"b\"c"), "a\\\"b\\\"c");
    }

    #[test]
    fn date_filename_fallback_is_well_formed() {
        let s = date_for_filename(None); // YYYY-MM-DD_HH-MM-SS
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "_");
    }

    #[test]
    fn unique_path_increments_on_collision() {
        let dir = std::env::temp_dir().join(format!("imapx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (p0, s0) = unique_path(&dir, "base", ".md");
        assert_eq!(s0, 0);
        std::fs::write(&p0, "x").unwrap();
        let (p1, s1) = unique_path(&dir, "base", ".md");
        assert_eq!(s1, 1);
        assert!(p1.to_string_lossy().contains("base_1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
