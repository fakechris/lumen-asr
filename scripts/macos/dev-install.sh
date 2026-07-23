#!/usr/bin/env bash
# Build release binary → install into .app bundle → stable-sign → optional launch.
#
# This is the daily local loop WITHOUT a paid Apple Developer Program account.
#
# Usage:
#   ./scripts/macos/dev-install.sh           # build + install + sign
#   ./scripts/macos/dev-install.sh --open    # also launch
#   ./scripts/macos/dev-install.sh --skip-build  # only reinstall/sign current binary
#   LUMEN_CODESIGN_IDENTITY="Apple Development: you@x.com (…)" ./scripts/macos/dev-install.sh
#
# After first install, grant Accessibility / Microphone once for this app.
# Re-running this script keeps the same signing identity → TCC usually sticks.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP_DIR="$ROOT/target/release/bundle/macos/Lumen ASR.app"
BIN_SRC="$ROOT/target/release/lumen-asr-desktop"
BIN_DST="$APP_DIR/Contents/MacOS/lumen-asr-desktop"
OPEN_APP=0
SKIP_BUILD=0

for arg in "$@"; do
  case "$arg" in
    --open) OPEN_APP=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

cd "$ROOT"

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "==> cargo build -p lumen-asr-desktop --release"
  cargo build -p lumen-asr-desktop --release
fi

if [[ ! -x "$BIN_SRC" ]]; then
  echo "ERROR: missing binary: $BIN_SRC" >&2
  exit 1
fi

# Prefer an existing Tauri-bundled skeleton; otherwise scaffold a minimal .app.
if [[ ! -d "$APP_DIR" ]]; then
  echo "==> no .app skeleton; creating minimal bundle at $APP_DIR"
  mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
  # Try to pull Info.plist / icon from a prior tauri build or src-tauri
  if [[ -f "$ROOT/apps/desktop/src-tauri/Info.plist" ]]; then
    # Minimal Info.plist for local runs
    cat >"$APP_DIR/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>lumen-asr-desktop</string>
  <key>CFBundleIdentifier</key><string>com.lumenopen.asr</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Lumen ASR</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Lumen ASR needs the microphone to record your voice for local speech-to-text.</string>
  <key>NSAccessibilityUsageDescription</key>
  <string>Lumen ASR needs Accessibility permission to paste transcribed text into other apps.</string>
</dict>
</plist>
PLIST
  fi
  echo -n "APPL????" >"$APP_DIR/Contents/PkgInfo"
fi

echo "==> install binary → $BIN_DST"
# Quit previous instance if running (match path, not argv of this script)
if pgrep -x "lumen-asr-desktop" >/dev/null 2>&1; then
  osascript -e 'tell application "Lumen ASR" to quit' 2>/dev/null || true
  sleep 0.4
fi

mkdir -p "$(dirname "$BIN_DST")"
cp -f "$BIN_SRC" "$BIN_DST"
chmod +x "$BIN_DST"

echo "==> sign"
"$ROOT/scripts/macos/sign-app.sh" "$APP_DIR"

echo ""
echo "Installed: $APP_DIR"
echo "Identity:  ${LUMEN_CODESIGN_IDENTITY:-Lumen Local Codesign}"
echo ""
echo "TCC tip: first run → System Settings → Privacy → Microphone + Accessibility"
echo "         enable \"Lumen ASR\". Reinstall with this script should keep grants"
echo "         (same cert). Ad-hoc (-s -) does NOT."

if [[ "$OPEN_APP" -eq 1 ]]; then
  echo "==> open"
  open "$APP_DIR"
fi
