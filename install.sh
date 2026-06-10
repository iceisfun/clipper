#!/usr/bin/env bash
#
# Clipper installer.
#
# Builds the release binaries, installs the `clipper-note` CLI onto your PATH,
# and registers the desktop launcher. Performs preflight checks for Rust
# tooling, a graphical session, and common runtime libraries before building.
#
# Usage:
#   ./install.sh [--bindir DIR] [--no-desktop] [--skip-build] [-h|--help]
#
# Options:
#   --bindir DIR   Where to install the clipper-note CLI (default: ~/.local/bin)
#   --no-desktop   Skip installing the GUI desktop launcher
#   --skip-build   Use the existing target/release build instead of rebuilding
#   -h, --help     Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIN_RUST="1.88.0"

BINDIR="${BINDIR:-$HOME/.local/bin}"
DO_DESKTOP=1
DO_BUILD=1

# ---------- pretty output ----------------------------------------------------
if [[ -t 1 ]]; then
    C_RESET=$'\033[0m'; C_RED=$'\033[31m'; C_GRN=$'\033[32m'
    C_YEL=$'\033[33m'; C_BLU=$'\033[34m'; C_BOLD=$'\033[1m'
else
    C_RESET=; C_RED=; C_GRN=; C_YEL=; C_BLU=; C_BOLD=
fi
info()  { printf '%s==>%s %s\n'  "$C_BLU" "$C_RESET" "$*"; }
ok()    { printf '%s  ✓%s %s\n'  "$C_GRN" "$C_RESET" "$*"; }
warn()  { printf '%s  !%s %s\n'  "$C_YEL" "$C_RESET" "$*" >&2; }
die()   { printf '%s  ✗%s %s\n'  "$C_RED" "$C_RESET" "$*" >&2; exit 1; }

# ---------- args -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bindir)     BINDIR="${2:?--bindir needs a path}"; shift 2 ;;
        --bindir=*)   BINDIR="${1#*=}"; shift ;;
        --no-desktop) DO_DESKTOP=0; shift ;;
        --skip-build) DO_BUILD=0; shift ;;
        -h|--help)    awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "${BASH_SOURCE[0]}"; exit 0 ;;
        *)            die "unknown option: $1 (try --help)" ;;
    esac
done

# ---------- preflight: platform ---------------------------------------------
info "Checking platform"
[[ "$(uname -s)" == "Linux" ]] || die "Clipper targets Linux; this is $(uname -s)."
ok "Linux $(uname -r)"

# ---------- preflight: graphical session ------------------------------------
info "Checking for a graphical session"
if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
    ok "Wayland session ($WAYLAND_DISPLAY)"
elif [[ -n "${DISPLAY:-}" ]]; then
    ok "X11 session ($DISPLAY)"
else
    warn "No \$DISPLAY or \$WAYLAND_DISPLAY — no desktop session detected."
    warn "The build will still work, but the GUI needs X11 or Wayland to run."
fi

# ---------- preflight: rust toolchain ---------------------------------------
info "Checking Rust toolchain (need >= $MIN_RUST)"
if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    die "Rust not found. Install it from https://rustup.rs:
       curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi
RUST_VER="$(rustc --version | awk '{print $2}')"
# Pass only if MIN_RUST sorts first (i.e. installed >= minimum).
if [[ "$(printf '%s\n%s\n' "$MIN_RUST" "$RUST_VER" | sort -V | head -n1)" != "$MIN_RUST" ]]; then
    die "rustc $RUST_VER is older than required $MIN_RUST. Update with: rustup update"
fi
ok "cargo + rustc $RUST_VER"

# ---------- preflight: runtime libraries ------------------------------------
# Pure soft checks: the GUI dlopen/links these at runtime. Missing ones are a
# warning with a per-distro hint, not a hard failure — the build is the real
# test, and a headless install may legitimately lack them.
info "Checking runtime libraries"

case "$(. /etc/os-release 2>/dev/null; echo "${ID_LIKE:-${ID:-}}")" in
    *debian*|*ubuntu*) PM="apt"  ;;
    *fedora*|*rhel*)   PM="dnf"  ;;
    *arch*)            PM="pac"  ;;
    *suse*)            PM="zyp"  ;;
    *)                 PM=""     ;;
esac

# Snapshot the linker cache once and substring-match in-shell. Avoids a
# `ldconfig | grep -q` pipe, which under `set -o pipefail` can report a false
# negative: grep -q exits on first match, SIGPIPEs ldconfig, and pipefail then
# propagates that as failure.
LDCACHE=""
if command -v ldconfig >/dev/null 2>&1; then
    LDCACHE="$(ldconfig -p 2>/dev/null || true)"
fi
have_lib() { [[ "$LDCACHE" == *"$1"* ]]; }

# name | soname | apt | dnf | pacman | zypper
LIBS=(
    "OpenGL|libGL.so.1|libgl1|mesa-libGL|mesa|Mesa-libGL1"
    "libxkbcommon|libxkbcommon.so.0|libxkbcommon0|libxkbcommon|libxkbcommon|libxkbcommon0"
)
# Only require the Wayland client lib when actually on Wayland.
if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
    LIBS+=("Wayland client|libwayland-client.so.0|libwayland-client0|wayland|wayland|libwayland-client0")
fi

missing=()
if [[ -z "$LDCACHE" ]]; then
    warn "ldconfig unavailable — skipping runtime library checks."
    LIBS=()
fi
for entry in "${LIBS[@]+"${LIBS[@]}"}"; do
    IFS='|' read -r name soname apt dnf pac zyp <<<"$entry"
    if have_lib "$soname"; then
        ok "$name"
    else
        case "$PM" in
            apt) pkg="$apt" ;; dnf) pkg="$dnf" ;; pac) pkg="$pac" ;; zyp) pkg="$zyp" ;; *) pkg="" ;;
        esac
        warn "missing $name ($soname)${pkg:+ — install package: $pkg}"
        missing+=("$name")
    fi
done
if [[ ${#missing[@]} -gt 0 && -n "$PM" ]]; then
    case "$PM" in
        apt) hint="sudo apt install" ;; dnf) hint="sudo dnf install" ;;
        pac) hint="sudo pacman -S"   ;; zyp) hint="sudo zypper install" ;;
    esac
    warn "Install the missing libraries with: $hint <package(s) above>"
fi

# ---------- build ------------------------------------------------------------
if [[ "$DO_BUILD" -eq 1 ]]; then
    info "Building release binaries (cargo build --release)"
    ( cd "$SCRIPT_DIR" && cargo build --release )
    ok "Built clipper and clipper-note"
else
    info "Skipping build (--skip-build)"
fi

CLIPPER_BIN="$SCRIPT_DIR/target/release/clipper"
NOTE_BIN="$SCRIPT_DIR/target/release/clipper-note"
[[ -x "$CLIPPER_BIN" ]] || die "missing $CLIPPER_BIN — build first (drop --skip-build)."
[[ -x "$NOTE_BIN"    ]] || die "missing $NOTE_BIN — build first (drop --skip-build)."

# ---------- install clipper-note CLI ----------------------------------------
info "Installing clipper-note CLI"
mkdir -p "$BINDIR"
install -m 0755 "$NOTE_BIN" "$BINDIR/clipper-note"
ok "Installed $BINDIR/clipper-note"

case ":$PATH:" in
    *":$BINDIR:"*) : ;;
    *) warn "$BINDIR is not on your \$PATH; add it, e.g.:"
       warn "  echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.bashrc && source ~/.bashrc" ;;
esac

# ---------- install desktop launcher ----------------------------------------
if [[ "$DO_DESKTOP" -eq 1 ]]; then
    info "Installing desktop launcher"
    if [[ -x "$SCRIPT_DIR/add_desktop_link.sh" ]]; then
        "$SCRIPT_DIR/add_desktop_link.sh"
    else
        warn "add_desktop_link.sh not found; skipping GUI launcher."
    fi
else
    info "Skipping desktop launcher (--no-desktop)"
fi

# ---------- summary ----------------------------------------------------------
echo
ok "${C_BOLD}Clipper installed.${C_RESET}"
echo "  GUI:  $CLIPPER_BIN  (launch from your app menu, or run it directly)"
echo "  CLI:  $BINDIR/clipper-note  — e.g.  clipper-note \"hello\""
echo
echo "If Clipper is already running an older build, restart it so clipper-note"
echo "can talk to it over the control socket."
