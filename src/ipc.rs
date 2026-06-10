//! Shared control-socket details between the `clipper` daemon and the
//! `clipper-note` CLI. Both compile this file directly (the daemon as `mod ipc`,
//! the CLI via `#[path = "../ipc.rs"]`) so the socket path and verbs have a
//! single source of truth.

use std::path::PathBuf;

/// Verb that asks the running instance to raise/focus its window. (Used by the
/// daemon; unused in the `clipper-note` client, which only sends notes.)
#[allow(dead_code)]
pub const VERB_FOCUS: &str = "focus";
/// Verb that injects the request body as a note at the top of the list. The
/// body is everything after the first newline, sent verbatim.
pub const VERB_NOTE: &str = "note";

/// Path of the per-user control socket the daemon binds and clients connect to.
/// Prefers `$XDG_RUNTIME_DIR` (per-user, 0700, cleaned on logout); falls back to
/// a `/tmp` path keyed by `$USER`.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("clipper.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    PathBuf::from(format!("/tmp/clipper-{user}.sock"))
}
