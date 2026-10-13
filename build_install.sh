#!/usr/bin/env bash
# Build sx, install plugins and binaries.
set -euo pipefail

PLUGIN_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sx/plugins"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

# ── dependency check ──────────────────────────────────────────────────────────

MISSING=""
check() { pkg-config --exists "$1" 2>/dev/null || MISSING="$MISSING $1"; }

check glib-2.0
check gio-2.0
check liblzma
check openssl
check tracker-sparql-3.0
check gtk4
check gtk4-layer-shell-0
check gdk-pixbuf-2.0
check pango
check cairo
check harfbuzz
check fontconfig
check freetype2
check xkbcommon
check xkbcommon-x11

if [ -n "$MISSING" ]; then
    echo "Missing pkg-config packages:$MISSING"
    echo ""
    echo "Fedora:  sudo dnf install glib2-devel xz-devel openssl-devel tinysparql-devel gtk4-devel gtk4-layer-shell-devel gdk-pixbuf2-devel pango-devel cairo-devel harfbuzz-devel fontconfig-devel freetype-devel libxkbcommon-devel libxkbcommon-x11-devel"
    echo "Debian:  sudo apt-get install libglib2.0-dev liblzma-dev libssl-dev libtracker-sparql-3.0-dev libgtk-4-dev libgtk4-layer-shell-dev libgdk-pixbuf2.0-dev libpango1.0-dev libcairo2-dev libharfbuzz-dev libfontconfig-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev"
    exit 1
fi

# ── rust check ────────────────────────────────────────────────────────────────

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found — install Rust from https://rustup.rs"
    exit 1
fi

# ── build ─────────────────────────────────────────────────────────────────────

cargo build --release --workspace --features tui,gpui

# ── install plugins ───────────────────────────────────────────────────────────

mkdir -p "$PLUGIN_DIR"
cp -f target/release/*.so "$PLUGIN_DIR"/
echo "Plugins installed to $PLUGIN_DIR"

# ── install binaries ──────────────────────────────────────────────────────────

cargo install --path sxc --bin sxc --bin sxp

# ── install desktop entry ─────────────────────────────────────────────────────

mkdir -p "$APPS_DIR"
cp -f sxp.desktop "$APPS_DIR"/
echo "Desktop entry installed to $APPS_DIR"

echo "Done."
