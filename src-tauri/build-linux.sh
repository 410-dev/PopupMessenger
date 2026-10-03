#!/usr/bin/env bash
# PopupMessenger Linux build (.deb, .rpm, AppImage).
# Usage:  ./build-linux.sh [--bundles deb,rpm,appimage] [--debug] [extra tauri build args]
#
# System packages (Debian/Ubuntu):
#   sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
#     libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"

BUNDLES="deb,rpm,appimage"
PROFILE="release"
EXTRA=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundles) BUNDLES="$2"; shift 2 ;;
    --debug)   PROFILE="debug"; EXTRA+=("--debug"); shift ;;
    *)         EXTRA+=("$1"); shift ;;
  esac
done

for cmd in cargo node npm; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "'$cmd' not found. Install Rust (stable) and Node.js first." >&2; exit 1; }
done
if command -v pkg-config >/dev/null 2>&1 && ! pkg-config --exists webkit2gtk-4.1; then
  echo "webkit2gtk-4.1 development files not found. See the package list at the top of this script." >&2
  exit 1
fi

cd "$ROOT"
[[ -d node_modules ]] || npm install

npx tauri build --bundles "$BUNDLES" ${EXTRA[@]+"${EXTRA[@]}"}

BUNDLE_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}/$PROFILE/bundle"
echo
echo "Build complete. Packages:"
find "$BUNDLE_DIR" -maxdepth 2 -type f \( -name '*.deb' -o -name '*.rpm' -o -name '*.AppImage' \) -print | sed 's/^/  /'
