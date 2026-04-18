#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/clipper"
TEMPLATE="$SCRIPT_DIR/clipper.desktop"
DEST_DIR="$HOME/.local/share/applications"
DEST="$DEST_DIR/clipper.desktop"

if [[ ! -x "$BIN" ]]; then
    echo "Release binary not found at: $BIN"
    echo "Build it first: cargo build --release"
    exit 1
fi

mkdir -p "$DEST_DIR"
sed "s|__CLIPPER_BIN__|$BIN|g" "$TEMPLATE" > "$DEST"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$DEST" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DEST_DIR" 2>/dev/null || true
fi

echo "Installed: $DEST"
echo
echo "Refresh your shell to see the entry:"
echo "  GNOME/X11:  press Alt+F2, type r, Enter"
echo "  Wayland:    log out and back in"
