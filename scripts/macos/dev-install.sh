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
SRC_PLIST="$ROOT/apps/desktop/src-tauri/Info.plist"
# Final install location. Building into target/ but launching from /Applications
# (Spotlight/Dock) is how you end up running a stale copy — so after signing we
# sync the fresh bundle here and open THIS one. Override with LUMEN_INSTALL_DEST=
# (set empty to skip and run in place from target/).
INSTALL_DEST="${LUMEN_INSTALL_DEST-/Applications/Lumen ASR.app}"
OPEN_APP=0
SKIP_BUILD=0
SKIP_FRONTEND=0

if [[ ! -f "$SRC_PLIST" ]]; then
  echo "ERROR: canonical Info.plist not found: $SRC_PLIST" >&2
  exit 1
fi
if ! /usr/bin/plutil -lint "$SRC_PLIST" >/dev/null; then
  echo "ERROR: canonical Info.plist is invalid: $SRC_PLIST" >&2
  exit 1
fi

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
  # Minimal bundle metadata for local runs. Privacy consent strings are merged
  # from the canonical src-tauri/Info.plist below.
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
</dict>
</plist>
PLIST
  echo -n "APPL????" >"$APP_DIR/Contents/PkgInfo"
fi

# Stop any running instance so the new binary actually loads. `osascript quit`
# can silently fail when the app is busy/unresponsive, and `open` on a
# still-running app only re-focuses it (the old process keeps executing its
# in-memory image) — that is how you "install" a new build but keep running the
# old one. So ask nicely, then escalate to SIGTERM and SIGKILL, and confirm the
# process is actually gone.
quit_lumen() {
  pgrep -x "lumen-asr-desktop" >/dev/null 2>&1 || return 0
  osascript -e 'tell application "Lumen ASR" to quit' >/dev/null 2>&1 || true
  for _ in $(seq 1 15); do  # up to ~3s for a graceful exit
    pgrep -x "lumen-asr-desktop" >/dev/null 2>&1 || return 0
    sleep 0.2
  done
  echo "  graceful quit timed out → SIGTERM" >&2
  pkill -x "lumen-asr-desktop" 2>/dev/null || true
  for _ in $(seq 1 10); do
    pgrep -x "lumen-asr-desktop" >/dev/null 2>&1 || return 0
    sleep 0.2
  done
  echo "  still running → SIGKILL" >&2
  pkill -9 -x "lumen-asr-desktop" 2>/dev/null || true
  sleep 0.3
  if pgrep -x "lumen-asr-desktop" >/dev/null 2>&1; then
    echo "WARNING: could not stop the running Lumen ASR instance; the new build" >&2
    echo "         may not launch (old process still holds the app)." >&2
    return 1
  fi
}

echo "==> install binary → $BIN_DST"
quit_lumen || true  # match path, not argv of this script; never abort the install

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
# the bundle plist. This is a plain `cargo build` (not `tauri build`), so Tauri
# does not merge these keys for us. Missing consent strings after a TCC reset can
# make system audio or Calendar access silently degrade.
if [[ ! -f "$PLIST" ]]; then
  echo "ERROR: bundle Info.plist not found: $PLIST" >&2
  exit 1
fi

usage_key_count=0
while IFS= read -r key; do
  [[ -n "$key" ]] || continue
  usage_key_count=$((usage_key_count + 1))
  if ! val="$(/usr/bin/plutil -extract "$key" raw -o - "$SRC_PLIST" 2>/dev/null)"; then
    echo "ERROR: could not read $key from canonical Info.plist" >&2
    exit 1
  fi
  /usr/bin/plutil -replace "$key" -string "$val" "$PLIST" 2>/dev/null \
    || /usr/bin/plutil -insert "$key" -string "$val" "$PLIST"
  actual="$(/usr/bin/plutil -extract "$key" raw -o - "$PLIST" 2>/dev/null)" || {
    echo "ERROR: $key was not written to bundle Info.plist" >&2
    exit 1
  }
  if [[ "$actual" != "$val" ]]; then
    echo "ERROR: $key differs between canonical and bundle Info.plist" >&2
    exit 1
  fi
done < <(
  /usr/bin/plutil -p "$SRC_PLIST" \
    | /usr/bin/sed -n 's/^  "\([^"]*UsageDescription\)" =>.*$/\1/p' \
    | /usr/bin/sort -u
)

if [[ "$usage_key_count" -eq 0 ]]; then
  echo "ERROR: canonical Info.plist has no UsageDescription keys" >&2
  exit 1
fi

required_privacy_keys=(
  NSMicrophoneUsageDescription
  NSAudioCaptureUsageDescription
  NSCalendarsUsageDescription
  NSCalendarsFullAccessUsageDescription
  NSAccessibilityUsageDescription
)
for key in "${required_privacy_keys[@]}"; do
  if ! /usr/bin/plutil -extract "$key" raw -o - "$SRC_PLIST" >/dev/null 2>&1; then
    echo "ERROR: canonical Info.plist is missing required key: $key" >&2
    exit 1
  fi
  if ! /usr/bin/plutil -extract "$key" raw -o - "$PLIST" >/dev/null 2>&1; then
    echo "ERROR: bundle Info.plist is missing required key: $key" >&2
    exit 1
  fi
done
if ! /usr/bin/plutil -lint "$PLIST" >/dev/null; then
  echo "ERROR: bundle Info.plist is invalid after privacy-key sync: $PLIST" >&2
  exit 1
fi
echo "==> privacy consent strings synced and verified ($usage_key_count keys)"

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
  quit_lumen || true
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
