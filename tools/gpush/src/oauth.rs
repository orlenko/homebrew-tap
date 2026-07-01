//! OAuth2 for an installed ("Desktop") Google app: one-time consent via a
//! loopback redirect + PKCE (`gpush auth`), then silent access-token refresh on
//! every run. Hand-rolled on `ureq` — the OOB flow Google killed in 2022 is
//! avoided in favour of a `http://127.0.0.1:<port>` redirect.
//!
//! Reads client credentials from `<dir>/credentials.json` (the JSON you download
//! for a Desktop OAuth client) and caches the token at `<dir>/token.json` (mode
//! 0600 — it holds a refresh token).

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AUTH_URI: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const TASKS_SCOPE: &str = "https://www.googleapis.com/auth/tasks";

#[derive(Serialize, Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    /// Unix seconds; refreshed 60s early to avoid edge-of-expiry 401s.
    expires_at: u64,
    scope: String,
}

pub struct Auth {
    creds_path: PathBuf,
    token_path: PathBuf,
    agent: ureq::Agent,
    /// In-memory cache of the loaded token, so repeated `access_token()` calls
    /// across a batch avoid re-reading disk and refresh only at the expiry edge.
    cached: RefCell<Option<Token>>,
}

impl Auth {
    pub fn new(dir: &Path) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            creds_path: dir.join("credentials.json"),
            token_path: dir.join("token.json"),
            agent,
            cached: RefCell::new(None),
        }
    }

    /// Run the interactive consent flow and cache the resulting token.
    pub fn authorize(&self, scope: &str) -> Result<()> {
        let (client_id, client_secret) =
            load_client_creds(&self.creds_path).with_context(|| {
                format!(
                    "reading Desktop-app credentials from {} (download them from your Google Cloud \
                 OAuth client and save them there)",
                    self.creds_path.display()
                )
            })?;

        let verifier = b64url(random_bytes(32)?);
        let challenge = b64url(Sha256::digest(verifier.as_bytes()));
        let state = b64url(random_bytes(16)?);

        let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback listener")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}");

        let url = format!(
            "{AUTH_URI}?client_id={}&redirect_uri={}&response_type=code&scope={}\
             &code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
            pct(&client_id),
            pct(&redirect),
            pct(scope),
            pct(&challenge),
            pct(&state),
        );

        eprintln!(
            "gpush: opening a browser for Google consent. If it doesn't open, visit:\n\n{url}\n"
        );
        let _ = std::process::Command::new("open").arg(&url).status();

        let (code, got_state) = wait_for_code(&listener)?;
        if got_state != state {
            bail!("OAuth state mismatch — aborting (possible CSRF)");
        }

        let body = form(&[
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("code", &code),
            ("code_verifier", &verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &redirect),
        ]);
        let (status, val) = self.token_request(&body)?;
        if status != 200 {
            bail!("token exchange failed ({status}): {val}");
        }
        let refresh_token = val["refresh_token"]
            .as_str()
            .ok_or_else(|| {
                anyhow!("no refresh_token in response (need access_type=offline + prompt=consent)")
            })?
            .to_string();
        let token = Token {
            access_token: val["access_token"].as_str().unwrap_or_default().to_string(),
            refresh_token,
            expires_at: now()
                + val["expires_in"]
                    .as_u64()
                    .unwrap_or(3600)
                    .saturating_sub(60),
            scope: val["scope"].as_str().unwrap_or(scope).to_string(),
        };
        self.save_token(&token)?;
        eprintln!(
            "gpush: authorized — token cached at {}",
            self.token_path.display()
        );
        eprintln!(
            "gpush: NOTE — if your OAuth consent screen is in 'Testing' status (the default for a \
             personal project), this refresh token expires in 7 days. Publish the app to \
             'Production' for long-lived access."
        );
        Ok(())
    }

    /// A valid access token for API calls, refreshing transparently. A failed
    /// refresh is surfaced loudly with the exact remedy — it is the single most
    /// likely unattended failure (7-day token death under a Testing consent screen).
    pub fn access_token(&self) -> Result<String> {
        // Take the cached token out (if any) to avoid holding a RefCell borrow
        // across the load/refresh below; it's put back before we return.
        let mut token = match self.cached.borrow_mut().take() {
            Some(t) => t,
            None => self
                .load_token()
                .map_err(|e| anyhow!("{e}\ngpush: not authorized — run `gpush auth` first"))?,
        };

        if now() >= token.expires_at {
            let (client_id, client_secret) = load_client_creds(&self.creds_path)?;
            let body = form(&[
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", &token.refresh_token),
                ("grant_type", "refresh_token"),
            ]);
            let (status, val) = self.token_request(&body)?;
            if status != 200 {
                bail!(
                    "OAuth refresh failed ({status}) — run `gpush auth` again.\n\
                     (Consent screens in 'Testing' status expire refresh tokens after 7 days; \
                     publish the app to 'Production' for long-lived tokens.)\nresponse: {val}"
                );
            }
            token.access_token = val["access_token"].as_str().unwrap_or_default().to_string();
            token.expires_at = now()
                + val["expires_in"]
                    .as_u64()
                    .unwrap_or(3600)
                    .saturating_sub(60);
            self.save_token(&token)?;
        }

        let access = token.access_token.clone();
        *self.cached.borrow_mut() = Some(token);
        Ok(access)
    }

    fn token_request(&self, body: &str) -> Result<(u16, serde_json::Value)> {
        let mut resp = self
            .agent
            .post(TOKEN_URI)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body)
            .context("POST to the token endpoint")?;
        let status = resp.status().as_u16();
        let val = resp
            .body_mut()
            .read_json()
            .unwrap_or(serde_json::Value::Null);
        Ok((status, val))
    }

    fn load_token(&self) -> Result<Token> {
        let raw = fs::read_to_string(&self.token_path)
            .with_context(|| format!("reading {}", self.token_path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", self.token_path.display()))
    }

    fn save_token(&self, t: &Token) -> Result<()> {
        let data = serde_json::to_string_pretty(t)?;
        let tmp = self.token_path.with_extension("json.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(data.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.token_path)?;
        Ok(())
    }
}

/// Pull `client_id`/`client_secret` from a downloaded credentials file, tolerating
/// the `installed`/`web` wrapper Google uses (or a flat object).
fn load_client_creds(path: &Path) -> Result<(String, String)> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading client credentials {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let node = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
    let id = node
        .get("client_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("no client_id in {}", path.display()))?;
    let secret = node
        .get("client_secret")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("no client_secret in {}", path.display()))?;
    Ok((id.to_string(), secret.to_string()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    // macOS-only tap; /dev/urandom is always present and non-blocking.
    let mut f = fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).context("reading /dev/urandom")?;
    Ok(buf)
}

fn b64url(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_ref())
}

/// Percent-encode for a URL query / form value (unreserved set per RFC 3986).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Decode the two hex digits from the *byte* slice — never `&s[..]`,
            // which would panic if a `%` is followed by a non-ASCII (multibyte)
            // byte in hostile loopback input.
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serve loopback redirects until one carries `code` or `error`, replying to each
/// so the browser tab resolves. Bounded so a stray flood (favicon/preconnect
/// requests, which browsers fire alongside the real navigation) can't hang us.
fn wait_for_code(listener: &TcpListener) -> Result<(String, String)> {
    for _ in 0..32 {
        let (mut stream, _) = listener.accept().context("accepting the OAuth redirect")?;
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let line = req.lines().next().unwrap_or("");
        let path = line.split_whitespace().nth(1).unwrap_or("");
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let val = pct_decode(v);
                match k {
                    "code" => code = Some(val),
                    "state" => state = Some(val),
                    "error" => error = Some(val),
                    _ => {}
                }
            }
        }

        let resolved = code.is_some() || error.is_some();
        let page = if resolved {
            "<html><body><h2>gpush: authorized</h2><p>You can close this tab.</p></body></html>"
        } else {
            "<html><body>gpush: waiting for the Google redirect…</body></html>"
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(resp.as_bytes());

        if let Some(e) = error {
            bail!("Google returned an OAuth error: {e}");
        }
        if let Some(c) = code {
            return Ok((c, state.unwrap_or_default()));
        }
        // Stray request (favicon, preconnect, empty) — keep listening.
    }
    bail!("no authorization code received on the loopback redirect")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_decode_survives_hostile_input() {
        assert_eq!(pct_decode("a%2Fb"), "a/b");
        assert_eq!(pct_decode("a+b"), "a b");
        // `%` followed by a multibyte char must not panic (the original bug).
        assert_eq!(pct_decode("%€"), "%€");
        // `%` at end, and a non-hex escape, are passed through literally.
        assert_eq!(pct_decode("x%"), "x%");
        assert_eq!(pct_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn pct_roundtrip_reserved() {
        let raw = "a/b+c=d&e";
        assert_eq!(pct_decode(&pct(raw)), raw);
    }
}
