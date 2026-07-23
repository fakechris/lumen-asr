#!/usr/bin/env bash
# Generate the macOS app icon (.icns) from the Lumen ASR mark SVG at build
# time. The rendered icon is written into a built .app's Resources — it is
# never committed, because the public repo forbids tracking binaries.
#
# Usage: gen-app-icon.sh [path/to/App.app]
#   Default target: target/release/bundle/macos/Lumen ASR.app
#   Without a valid .app, writes icon.icns to $LUMEN_ICON_OUT (or a temp file).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SVG="$ROOT/apps/desktop/src/assets/product-icons/lumen-asr.svg"
APP="${1:-$ROOT/target/release/bundle/macos/Lumen ASR.app}"

[[ -f "$SVG" ]] || { echo "ERROR: mark SVG not found: $SVG" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

render_1024() {
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w 1024 -h 1024 "$SVG" -o "$1"
  elif command -v magick >/dev/null 2>&1; then
    magick -background none -density 1024 "$SVG" -resize 1024x1024 "$1"
  elif command -v convert >/dev/null 2>&1; then
    convert -background none -density 1024 "$SVG" -resize 1024x1024 "$1"
  else
    echo "ERROR: need rsvg-convert or ImageMagick to rasterize the icon" >&2
    exit 1
  fi
}

ICONSET="$WORK/lumen.iconset"
mkdir -p "$ICONSET"
render_1024 "$WORK/base-1024.png"

gen() { sips -z "$2" "$2" "$WORK/base-1024.png" --out "$ICONSET/$1" >/dev/null; }
gen icon_16x16.png 16
gen icon_16x16@2x.png 32
gen icon_32x32.png 32
gen icon_32x32@2x.png 64
gen icon_128x128.png 128
gen icon_128x128@2x.png 256
gen icon_256x256.png 256
gen icon_256x256@2x.png 512
gen icon_512x512.png 512
gen icon_512x512@2x.png 1024

iconutil -c icns "$ICONSET" -o "$WORK/icon.icns"

if [[ -d "$APP/Contents" ]]; then
  mkdir -p "$APP/Contents/Resources"
  cp -f "$WORK/icon.icns" "$APP/Contents/Resources/icon.icns"
  echo "app icon → $APP/Contents/Resources/icon.icns"
else
  OUT="${LUMEN_ICON_OUT:-$WORK/icon.icns}"
  [[ "$OUT" != "$WORK/icon.icns" ]] && cp -f "$WORK/icon.icns" "$OUT"
  echo "app icon → $OUT (no .app target given)"
fi
