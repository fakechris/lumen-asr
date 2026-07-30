#!/usr/bin/env bash
# Regenerate EVERY app icon from a single source of truth so the brand mark can
# never drift again (this is how a deprecated icon crept back into the bundle).
#
# Source of truth:
#   apps/desktop/src/assets/product-icons/lumen-asr.svg
#   — the Lumen Design System mark: espresso #231a13 rounded-square tile +
#     one categorical-orange waveform glyph, flat (no gradient, no glow).
#
# Outputs (all derived from that SVG):
#   apps/desktop/src-tauri/icons/{icon.png,32x32.png,128x128.png,128x128@2x.png,icon.icns,icon.ico,Lumen.icns}
#   apps/desktop/src/assets/icon/{AppIcon.svg,AppIcon-small.svg,AppIcon-1024.png,AppIcon-512.png,Lumen.icns}
#   apps/desktop/src/assets/icon/Lumen.iconset/*
#   docs/images/{app-icon.png,app-icon-128.png}
#
# Requires: rsvg-convert (brew install librsvg), sips + iconutil (macOS),
#           magick/convert (brew install imagemagick, for the Windows .ico).
#
# Usage: ./scripts/macos/regen-icons.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/apps/desktop/src/assets/product-icons/lumen-asr.svg"
TAURI_ICONS="$ROOT/apps/desktop/src-tauri/icons"
APP_ICON="$ROOT/apps/desktop/src/assets/icon"
DOCS_IMG="$ROOT/docs/images"

[[ -f "$SRC" ]] || { echo "ERROR: missing source SVG: $SRC" >&2; exit 1; }
command -v rsvg-convert >/dev/null || { echo "ERROR: need rsvg-convert (brew install librsvg)" >&2; exit 1; }
command -v iconutil >/dev/null || { echo "ERROR: need iconutil (macOS)" >&2; exit 1; }
# icon.ico is a required output (tauri.conf.json references it), so a missing
# converter must fail the run rather than silently skip it.
if command -v magick >/dev/null; then
  ICO_TOOL="magick"
elif command -v convert >/dev/null; then
  ICO_TOOL="convert"
else
  echo "ERROR: need magick or convert (brew install imagemagick) to regenerate icon.ico" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

png() { # png <size> <out>
  rsvg-convert -w "$1" -h "$1" "$SRC" -o "$2"
}

echo "==> master 1024 from $SRC"
png 1024 "$TMP/master-1024.png"

echo "==> macOS .iconset → .icns"
ISET="$TMP/Lumen.iconset"
mkdir -p "$ISET"
# iconutil expects these exact names (base + @2x retina variants).
png 16   "$ISET/icon_16x16.png"
png 32   "$ISET/icon_16x16@2x.png"
png 32   "$ISET/icon_32x32.png"
png 64   "$ISET/icon_32x32@2x.png"
png 128  "$ISET/icon_128x128.png"
png 256  "$ISET/icon_128x128@2x.png"
png 256  "$ISET/icon_256x256.png"
png 512  "$ISET/icon_256x256@2x.png"
png 512  "$ISET/icon_512x512.png"
cp "$TMP/master-1024.png" "$ISET/icon_512x512@2x.png"
iconutil -c icns "$ISET" -o "$TMP/icon.icns"

echo "==> write Tauri bundle icons → $TAURI_ICONS"
mkdir -p "$TAURI_ICONS"
cp "$TMP/master-1024.png" "$TAURI_ICONS/icon.png"
png 32  "$TAURI_ICONS/32x32.png"
png 128 "$TAURI_ICONS/128x128.png"
png 256 "$TAURI_ICONS/128x128@2x.png"
cp "$TMP/icon.icns" "$TAURI_ICONS/icon.icns"
cp "$TMP/icon.icns" "$TAURI_ICONS/Lumen.icns"
"$ICO_TOOL" "$TMP/master-1024.png" -define icon:auto-resize=256,128,64,48,32,16 "$TAURI_ICONS/icon.ico"

echo "==> write app-icon masters → $APP_ICON"
mkdir -p "$APP_ICON/Lumen.iconset"
# The SVG IS the master vector now — no separate blue 'full detail' source.
cp "$SRC" "$APP_ICON/AppIcon.svg"
cp "$SRC" "$APP_ICON/AppIcon-small.svg"
cp "$TMP/master-1024.png" "$APP_ICON/AppIcon-1024.png"
png 512 "$APP_ICON/AppIcon-512.png"
cp "$ISET"/*.png "$APP_ICON/Lumen.iconset/"
cp "$TMP/icon.icns" "$APP_ICON/Lumen.icns"

echo "==> write docs images → $DOCS_IMG"
mkdir -p "$DOCS_IMG"
png 512 "$DOCS_IMG/app-icon.png"
png 128 "$DOCS_IMG/app-icon-128.png"

echo ""
echo "Done. All icons regenerated from $SRC"
echo "If you changed the brand mark, re-run this and commit the results together."
