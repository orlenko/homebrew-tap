//! Pretty, TTY-aware logging. Colors only when writing to a terminal and when
//! `NO_COLOR` is unset, so piped output and log files stay plain text. Format:
//! `HH:MM:SS <badge> <tag> <message>` — the tag lets several concurrent watchers
//! be told apart at a glance.

use jiff::Zoned;
use std::io::{IsTerminal, Write};

pub struct Logger {
    tag: String,
    color_stdout: bool,
    color_stderr: bool,
}

impl Logger {
    pub fn new(tag: impl Into<String>) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            tag: tag.into(),
            color_stdout: std::io::stdout().is_terminal() && !no_color,
            color_stderr: std::io::stderr().is_terminal() && !no_color,
        }
    }

    fn paint(on: bool, code: &str, s: &str) -> String {
        if on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn line(&self, color: bool, badge_code: &str, badge: &str, msg: &str) -> String {
        let ts = Zoned::now().strftime("%H:%M:%S").to_string();
        format!(
            "{} {} {} {}",
            Self::paint(color, "2", &ts),
            Self::paint(color, badge_code, badge),
            Self::paint(color, "36", &self.tag),
            msg
        )
    }

    /// Success / progress (green dot) to stdout.
    pub fn info(&self, msg: &str) {
        let line = self.line(self.color_stdout, "32", "•", msg);
        let _ = writeln!(std::io::stdout(), "{line}");
    }

    /// Warning (yellow bang) to stdout.
    pub fn warn(&self, msg: &str) {
        let line = self.line(self.color_stdout, "33", "!", msg);
        let _ = writeln!(std::io::stdout(), "{line}");
    }

    /// Error (red cross) to stderr, message itself reddened.
    pub fn error(&self, msg: &str) {
        let painted = Self::paint(self.color_stderr, "31", msg);
        let line = self.line(self.color_stderr, "31", "✗", &painted);
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}
