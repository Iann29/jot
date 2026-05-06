#!/usr/bin/env bash
# Jot installer — builds the binary and installs the desktop entry + icon

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"
ICON_DIR="$PREFIX/share/icons/hicolor/scalable/apps"

cd "$ROOT_DIR"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. install rust: https://rustup.rs" >&2
  exit 1
fi

echo "==> Building jot (release)…"
cargo build --release

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

echo "==> Installing binary to $BIN_DIR/jot"
install -Dm755 "$ROOT_DIR/target/release/jot" "$BIN_DIR/jot"

echo "==> Installing desktop entry"
install -Dm644 "$ROOT_DIR/data/jot.desktop" "$APP_DIR/jot.desktop"

echo "==> Installing icon"
install -Dm644 "$ROOT_DIR/data/icons/jot.svg" "$ICON_DIR/jot.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo
echo "✓ Jot installed"
echo
echo "Next steps:"
echo "  1. Make sure $BIN_DIR is in your PATH"
echo "  2. (Hyprland) source the bundled config:"
echo "       cp $ROOT_DIR/data/hyprland/jot.conf ~/.config/hypr/jot.conf"
echo "       echo 'source = ~/.config/hypr/jot.conf' >> ~/.config/hypr/hyprland.conf"
echo "       hyprctl reload"
echo "  3. Press Super+N or launch from your app menu (Super+Space)"
