#!/usr/bin/env bash
# Build release binary → install into .app bundle → stable-sign → optional launch.
#
# This is the daily local loop WITHOUT a paid Apple Developer Program account.
#
# Usage:
#   ./scripts/macos/dev-install.sh              # rebuild frontend + backend, install, sign
#   ./scripts/macos/dev-install.sh --open       # also launch
#   ./scripts/macos/dev-install.sh --skip-frontend  # backend-only iteration (reuse dist)
#   ./scripts/macos/dev-install.sh --skip-build # only reinstall/sign the current binary
#   LUMEN_CODESIGN_IDENTITY="Apple Development: you@x.com (…)" ./scripts/macos/dev-install.sh
#
# By default the frontend is rebuilt first so a fresh backend never ships stale
# UI (plain `cargo build` embeds whatever `dist/` already exists). Use
# --skip-frontend to skip that when only backend code changed.
#
# After first install, grant Accessibility / Microphone once for this app.
# Re-running this script keeps the same signing identity → TCC usually sticks.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP_DIR="$ROOT/target/release/bundle/macos/Lumen ASR.app"
BIN_SRC="$ROOT/target/release/lumen-asr-desktop"
BIN_DST="$APP_DIR/Contents/MacOS/lumen-asr-desktop"
# Final install location. Building into target/ but launching from /Applications
# (Spotlight/Dock) is how you end up running a stale copy — so after signing we
# sync the fresh bundle here and open THIS one. Override with LUMEN_INSTALL_DEST=
# (set empty to skip and run in place from target/).
INSTALL_DEST="${LUMEN_INSTALL_DEST-/Applications/Lumen ASR.app}"
OPEN_APP=0
SKIP_BUILD=0
SKIP_FRONTEND=0

for arg in "$@"; do
  case "$arg" in
    --open) OPEN_APP=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --skip-frontend) SKIP_FRONTEND=1 ;;
    -h|--help)
      sed -n '2,18p' "$0"
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
  if [[ "$SKIP_FRONTEND" -eq 0 ]]; then
    echo "==> rebuild frontend (npm run build → apps/desktop/dist)"
    npm --prefix "$ROOT/apps/desktop" run build
    # The Rust shell embeds dist/ via generate_context! in lib.rs at compile
    # time. Bump its mtime so cargo re-embeds the freshly built UI instead of
    # baking a stale copy into the new backend.
    touch "$ROOT/apps/desktop/src-tauri/src/lib.rs"
  else
    echo "==> skipping frontend rebuild (--skip-frontend); dist/ may be stale"
  fi
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
  <key>CFBundleIconFile</key><string>icon</string>
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

echo "==> app icon"
# Goal: the dock icon is present after EVERY install. Prefer a prebuilt .icns
# already in the repo (no external rasterizer needed); only fall back to the
# SVG generator when none exists, and warn loudly if even that fails.
ICON_SRC=""
for cand in \
  "$ROOT/apps/desktop/src-tauri/icons/icon.icns" \
  "$ROOT/apps/desktop/src-tauri/icons/Lumen.icns"; do
  if [[ -f "$cand" ]]; then ICON_SRC="$cand"; break; fi
done

ICON_DST="$APP_DIR/Contents/Resources/icon.icns"
PLIST="$APP_DIR/Contents/Info.plist"

if [[ -n "$ICON_SRC" ]]; then
  mkdir -p "$APP_DIR/Contents/Resources"
  cp -f "$ICON_SRC" "$ICON_DST"
  echo "  icon ← $ICON_SRC"
elif "$ROOT/scripts/macos/gen-app-icon.sh" "$APP_DIR"; then
  echo "  icon generated from mark SVG"
else
  echo "WARNING: could not install a dock icon (no prebuilt .icns and the SVG" >&2
  echo "         generator failed — install rsvg-convert or ImageMagick). The" >&2
  echo "         app will show the generic macOS icon." >&2
fi

# Point Info.plist at the icon so the Finder/Dock actually load it. PlistBuddy
# ships with macOS and preserves the XML format (unlike `defaults write`).
if [[ -f "$ICON_DST" && -f "$PLIST" ]]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleIconFile icon" "$PLIST" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string icon" "$PLIST"
fi

# Sync every privacy consent string from the canonical src-tauri/Info.plist into
# the bundle plist. This is a plain `cargo build` (not `tauri build`), so the
# scaffolded minimal plist above only carries Mic + Accessibility; without this,
# system-audio (NSAudioCaptureUsageDescription) and Calendar consent strings are
# absent, and after any TCC reset the first request is denied — meetings then
# silently degrade to mic-only (system_audio.rs treats denial as a degrade, not
# a crash). Keeping one source of truth avoids that latent trap.
SRC_PLIST="$ROOT/apps/desktop/src-tauri/Info.plist"
if [[ -f "$SRC_PLIST" && -f "$PLIST" ]]; then
  while IFS= read -r key; do
    [[ -n "$key" ]] || continue
    val="$(/usr/libexec/PlistBuddy -c "Print :$key" "$SRC_PLIST" 2>/dev/null)" || continue
    /usr/libexec/PlistBuddy -c "Set :$key $val" "$PLIST" 2>/dev/null \
      || /usr/libexec/PlistBuddy -c "Add :$key string $val" "$PLIST"
  done < <(grep -oE 'NS[A-Za-z]+UsageDescription' "$SRC_PLIST" | sort -u)
fi

echo "==> sign"
"$ROOT/scripts/macos/sign-app.sh" "$APP_DIR"

# Sync the signed bundle to its final launch location so Spotlight/Dock always
# start the build we just made — the codesign signature is identifier-based
# (path-independent), so it travels with the bundle. ditto preserves it.
FINAL_APP="$APP_DIR"
if [[ -n "$INSTALL_DEST" ]]; then
  # Guard the destructive replace: INSTALL_DEST is caller-overridable, so refuse
  # anything but an absolute *.app path, and never a path inside the build tree
  # (that would delete the source bundle we just built).
  case "$INSTALL_DEST" in
    /*.app) : ;;
    *)
      echo "ERROR: LUMEN_INSTALL_DEST must be an absolute path ending in .app" >&2
      echo "       (got: '$INSTALL_DEST')" >&2
      exit 2
      ;;
  esac
  if [[ "$INSTALL_DEST" == "$ROOT"* ]]; then
    echo "ERROR: LUMEN_INSTALL_DEST must not point inside the build tree" >&2
    echo "       (got: '$INSTALL_DEST')" >&2
    exit 2
  fi

  echo "==> install → $INSTALL_DEST"
  if pgrep -x "lumen-asr-desktop" >/dev/null 2>&1; then
    osascript -e 'tell application "Lumen ASR" to quit' 2>/dev/null || true
    sleep 0.4
  fi
  # Stage to a sibling, verify the signature there, and only then swap it in — so
  # a failed copy/verify never leaves the destination half-removed, and a verify
  # failure falls through to the run-in-place warning instead of exiting (set -e).
  STAGE="${INSTALL_DEST}.new"
  rm -rf "$STAGE"
  if ditto "$APP_DIR" "$STAGE" && codesign --verify --verbose=1 "$STAGE" 2>&1 | tail -1; then
    rm -rf "$INSTALL_DEST"
    mv "$STAGE" "$INSTALL_DEST"
    FINAL_APP="$INSTALL_DEST"

    # A stale same-bundle-id copy from an older install (classically
    # ~/Applications/<name>.app) can outrank this one in Launch Services and make
    # `--open` / the Dock show the PREVIOUS icon even though the bundle here is
    # correct. Remove that known duplicate and force LS to (re)register the copy
    # we just installed so it resolves the fresh one.
    lsreg="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    stale_home="$HOME/Applications/$(basename "$INSTALL_DEST")"
    if [[ -d "$stale_home" && "$stale_home" != "$INSTALL_DEST" ]]; then
      echo "  removing stale duplicate that would shadow the icon: $stale_home"
      [[ -x "$lsreg" ]] && "$lsreg" -u "$stale_home" 2>/dev/null || true
      rm -rf "$stale_home"
    fi
    [[ -x "$lsreg" ]] && "$lsreg" -f "$INSTALL_DEST" 2>/dev/null || true
  else
    rm -rf "$STAGE"
    echo "WARNING: could not install into $INSTALL_DEST; run in place from" >&2
    echo "         $APP_DIR instead (set LUMEN_INSTALL_DEST= to silence)." >&2
  fi
fi

echo ""
echo "Installed: $FINAL_APP"
echo "Identity:  ${LUMEN_CODESIGN_IDENTITY:-Lumen Local Codesign}"
echo ""
echo "TCC tip: first run → System Settings → Privacy → Microphone + Accessibility"
echo "         enable \"Lumen ASR\". Reinstall with this script should keep grants"
echo "         (same cert). Ad-hoc (-s -) does NOT."

if [[ "$OPEN_APP" -eq 1 ]]; then
  echo "==> open"
  open "$FINAL_APP"
fi
