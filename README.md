# Clipper

Clipboard history for Linux, built with [iced](https://iced.rs). Watches the
system clipboard, keeps a rolling history of text and image clips, and lets you
re-copy anything back with one click.

## Features

- Two-pane UI: scrollable clip list on the left, full content on the right
- Text and image clips (PNG/screenshots, etc.)
- Polls the clipboard every 500 ms via [`arboard`](https://crates.io/crates/arboard)
- Dedup by content hash — repeated copies bump the existing entry to the top
  instead of creating duplicates
- `Copy` button writes the selected clip back to the system clipboard
- `×` button per row removes an entry; clipboard-change tracking prevents it
  from immediately reappearing if the same content is still on the clipboard
- Dark theme, 200-entry history cap

## Requirements

- Rust (1.88+)
- Linux with X11 or Wayland
- System libs typically already present (Xlib, Wayland, GL)

## Build and run

```
cargo run --release
```

## Install as a launcher entry

```
cargo build --release
cp clipper.desktop ~/.local/share/applications/
update-desktop-database ~/.local/share/applications/ 2>/dev/null
```

On GNOME/X11, press `Alt+F2` then `r` to restart the shell so the new entry
appears in Activities. On Wayland, log out and back in.

The shipped `clipper.desktop` points `Exec` at
`/home/iceisfun/work/clipper/target/release/clipper` — edit that path if you
clone the project elsewhere.

## Notes

- On X11, arboard sometimes returns images with every pixel's alpha byte set to
  0 (fully transparent). Clipper detects that and forces alpha to 255 so the
  image actually shows.
- History is in-memory only; quitting the app clears it.
