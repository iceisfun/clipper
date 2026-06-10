//! `clipper-note` — inject a note at the top of the running Clipper's history
//! list without touching the system clipboard. AI-first: takes note text from
//! arguments or stdin, prints nothing on success, exits non-zero on failure.
//!
//! Usage:
//!   clipper-note "remember to rotate the API key"
//!   printf '%s\n' "$multi_line_text" | clipper-note
//!
//! Talks to the daemon over the same per-user control socket used for the
//! single-instance focus handshake (see `src/ipc.rs`, shared verbatim).

#[path = "../ipc.rs"]
mod ipc;

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

const USAGE: &str = "\
clipper-note — add a note to the top of the Clipper list (not the clipboard)

USAGE:
    clipper-note <text>...     send the given text as a note
    clipper-note               read the note body from stdin

The note appears at the top of Clipper's history, tinted as an AI note, and
persists with the rest of the history. Clipper must be running.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    // Note body: joined args, or stdin when no args are given.
    let body = if args.is_empty() {
        let mut s = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut s) {
            eprintln!("clipper-note: failed to read stdin: {e}");
            return ExitCode::FAILURE;
        }
        s
    } else {
        args.join(" ")
    };

    if body.trim().is_empty() {
        eprintln!("clipper-note: empty note; nothing to send");
        return ExitCode::FAILURE;
    }

    let path = ipc::socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "clipper-note: could not reach Clipper at {} ({e}); is it running?",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let request = format!("{}\n{}", ipc::VERB_NOTE, body);
    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("clipper-note: failed to send note: {e}");
        return ExitCode::FAILURE;
    }
    let _ = stream.flush();
    // Close our write half so the daemon's read_to_end returns promptly.
    let _ = stream.shutdown(Shutdown::Write);

    ExitCode::SUCCESS
}
