---
name: clipper-note
description: Persist a short note to the top of the user's Clipper clipboard-history list without touching the system clipboard. Use when you want to leave the user a durable, glanceable note, reminder, summary, or hand-off message they will see in their Clipper app — e.g. "remember to rotate the key", a TODO, a link, or a snippet you produced for them to copy later.
---

# clipper-note

`clipper-note` injects a note at the **top of the user's Clipper history list**.
It does **not** write to the system clipboard, so it never disturbs what the user
currently has copied. Notes appear with a green "AI note" tint and persist with
the rest of their history.

## When to use

- You want to leave the user something they'll notice and keep: a reminder, a
  decision summary, a command to run later, a link, or a snippet.
- You produced text the user will likely want to copy later — drop it in Clipper
  so it's one click away in their `Copy` flow, without clobbering their clipboard
  now.

Do **not** use it as a scratchpad or log; each call adds a visible row to the
user's history.

## Usage

```bash
# Single note from arguments
clipper-note "Rotate the staging API key before Friday's deploy."

# Multi-line note from stdin (preferred for anything long or generated)
clipper-note <<'EOF'
Release checklist:
  - bump version in Cargo.toml
  - cargo build --release
  - restart the desktop app
EOF

# Pipe generated output straight in
some-command | clipper-note
```

If arguments are given they are joined with spaces and sent as the note. If no
arguments are given, the note body is read from **stdin**. A trailing newline is
trimmed.

## Behavior & contract

- The note lands at the **top** of the list and is **selected only if nothing
  else is selected**, so it won't yank the user's current view.
- It is **never** placed on the system clipboard.
- It persists to Clipper's on-disk history (when history storage is enabled) and
  survives restarts, tinted as an AI note.
- Identical notes are de-duplicated (a repeat bumps the existing row to the top)
  and are kept distinct from an identical *copied* text clip.

## Requirements & failure handling

- **Clipper must be running.** `clipper-note` talks to it over the per-user
  control socket (`$XDG_RUNTIME_DIR/clipper.sock`).
- Exit code `0` on success; **non-zero** on failure, with a message on stderr:
  - `could not reach Clipper … is it running?` — the app isn't running. Ask the
    user to start Clipper, or start it yourself, then retry.
  - `empty note; nothing to send` — no usable text was provided.

Always check the exit code; a zero exit means the note was delivered to the
running app.

## How it works (for maintainers)

`clipper-note` is a tiny Unix-socket client. It reuses the same control socket
the app already uses for its single-instance focus handshake (see
`src/ipc.rs`, shared verbatim between the daemon and this CLI). The wire protocol
is one line for the verb followed by the payload:

```
note\n<note body, sent verbatim, may span multiple lines>
```

The running app's listener parses the verb (`src/main.rs::parse_ipc`), forwards
a `note` command into the iced event loop (`ipc_worker`), and ingests it via
`Clipper::ingest_note`, which stores it tagged as a note (`storage::save_note`,
`TAG_NOTE`) so the origin round-trips across restarts.

Build & install:

```bash
cargo build --release
cp target/release/clipper-note ~/.local/bin/   # any dir on $PATH
```
