#!/usr/bin/env bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REX_HOME="${REX_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/rex}"

if [[ "${1:-}" == "--uninstall" || "${1:-}" == "uninstall" ]]; then
  if [[ -x "$HOME/.local/bin/rex-omnidiff" ]]; then
    "$HOME/.local/bin/rex-omnidiff" uninstall --rex-home "$REX_HOME"
  elif [[ -x "$DIR/target/release/rex-omnidiff" ]]; then
    "$DIR/target/release/rex-omnidiff" uninstall --rex-home "$REX_HOME"
  else
    cargo run --release --manifest-path "$DIR/Cargo.toml" -- uninstall --rex-home "$REX_HOME"
  fi
  echo "Uninstalled rex-omnidiff from $REX_HOME"
  exit 0
fi

echo "Building native Rust engine & Kitty UI (rex-omnidiff --release)…"
cargo build --release --manifest-path "$DIR/Cargo.toml"

echo "Installing binary, Lua module, and Rex keybindings…"
"$DIR/target/release/rex-omnidiff" install --repo "$DIR" --rex-home "$REX_HOME"

if command -v rex >/dev/null 2>&1; then
  echo "Verifying Rex configuration with rex config check…"
  rex config check "$REX_HOME/init.lua"
  echo "Reloading Rex configuration…"
  rex config reload || true
fi

echo "Done! OmniDiff for Rex is installed (shift+cmd+g = floating review window, alt+cmd+g = docked split)."
