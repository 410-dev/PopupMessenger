#!/usr/bin/env bash
# PopupMessenger macOS build (.app + .dmg).
# Usage:  ./build-macos.sh [--universal] [--bundles app,dmg] [--debug] [extra tauri build args]
#   --universal  build one binary for both Apple Silicon and Intel
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"

BUNDLES="app,dmg"
PROFILE="release"
TARGET=""
EXTRA=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --universal) TARGET="universal-apple-darwin"; shift ;;
    --bundles)   BUNDLES="$2"; shift 2 ;;
    --debug)     PROFILE="debug"; EXTRA+=("--debug"); shift ;;
    *)           EXTRA+=("$1"); shift ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || { echo "This script must run on macOS." >&2; exit 1; }
for cmd in cargo node npm; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "'$cmd' not found. Install Rust (stable) and Node.js first." >&2; exit 1; }
done
xcode-select -p >/dev/null 2>&1 || { echo "Xcode Command Line Tools missing. Run: xcode-select --install" >&2; exit 1; }

if [[ -n "$TARGET" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  EXTRA+=("--target" "$TARGET")
fi

cd "$ROOT"
[[ -d node_modules ]] || npm install

npx tauri build --bundles "$BUNDLES" ${EXTRA[@]+"${EXTRA[@]}"}

TARGET_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"
BUNDLE_DIR="$TARGET_DIR/${TARGET:+$TARGET/}$PROFILE/bundle"
echo
echo "Build complete. Bundles:"
find "$BUNDLE_DIR" -maxdepth 2 \( -name '*.app' -o -name '*.dmg' \) -print | sed 's/^/  /'
