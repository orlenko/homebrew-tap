//! Shared glue for `orlenko/homebrew-tap` CLI tools.
//!
//! Deliberately tiny. It holds only the two things every tool in this factory
//! would otherwise copy-paste: loading a per-directory `.env` without clobbering
//! real shell env vars, and pretty, TTY-aware logging. Anything that already has
//! a good single-purpose crate (HTTP, JSON, dates, mail) is pulled in by the tool
//! directly — this crate is not a kitchen sink, and it grows only when a second
//! tool genuinely shares something.

pub mod config;
pub mod log;
