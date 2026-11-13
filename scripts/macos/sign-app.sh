#!/usr/bin/env bash
# Sign a .app with the best stable identity available — WITHOUT touching
# the login keychain (INV-1817 follow-up: codesign keychain prompts).
#
# Strategy: export the signing identity to a .p12 file once, then for
# every build create a temporary keychain (no password, pre-unlocked),
# import the identity into it, and pass --keychain to codesign. The
# login keychain is never read; macOS never prompts.
#
# The .p12 lives at ~/.lumen-codesign.p12 (0600). Delete it and re-run
# to re-export from the login keychain (will prompt once for that export).
#
# Usage: sign-app.sh [/path/to/Lumen ASR.app]

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP="${1:-$ROOT/target/release/bundle/macos/Lumen ASR.app}"
P12="${LUMEN_CODESIGN_P12:-$HOME/.lumen-codesign.p12}"
TEMP_KEYCHAIN="/tmp/lumen-build-$$"
IDENTITY="${LUMEN_CODESIGN_IDENTITY:-Lumen Local Codesign}"

if [[ ! -d "$APP" ]]; then
  echo "ERROR: app not found: $APP" >&2
  exit 1
fi

# ── One-time: export the identity to a .p12 file ─────────────────────
if [[ ! -f "$P12" ]]; then
  echo "==> One-time export: $IDENTITY → $P12 (may prompt for keychain once)"
  security export -k "$HOME/Library/Keychains/login.keychain-db" \
    -t identities -f pkcs12 -P "lumen-dev" \
    -o "$P12" "$IDENTITY" 2>/dev/null || {
    # Fallback: try by finding the identity hash
    HASH=$(security find-identity -v -p codesigning 2>/dev/null | grep "$IDENTITY" | awk '{print $2}')
    if [[ -z "$HASH" ]]; then
      echo "WARN: could not find '$IDENTITY' in keychain; falling back to ad-hoc" >&2
      IDENTITY="-"
    else
      security export -k "$HOME/Library/Keychains/login.keychain-db" \
        -t identities -f pkcs12 -P "lumen-dev" \
        -o "$P12" "$HASH" 2>/dev/null || {
        echo "WARN: export failed; falling back to ad-hoc" >&2
        IDENTITY="-"
      }
    fi
  }
  [[ -f "$P12" ]] && chmod 600 "$P12"
fi

xattr -cr "$APP" 2>/dev/null || true

ARGS=(--force --deep --sign "$IDENTITY" --timestamp=none)

ENTITLEMENTS="${LUMEN_CODESIGN_ENTITLEMENTS:-$ROOT/scripts/macos/entitlements.dev.plist}"
if [[ -f "$ENTITLEMENTS" ]]; then
  ARGS+=(--entitlements "$ENTITLEMENTS")
fi
if [[ "${LUMEN_CODESIGN_HARDENED:-0}" == "1" ]]; then
  ARGS+=(--options runtime)
fi

echo "Signing: $APP"

if [[ "$IDENTITY" == "-" ]]; then
  echo "  identity: ad-hoc (-)"
  echo "  WARN: ad-hoc changes cdhash every rebuild → Mic/Accessibility often need re-grant."
  codesign "${ARGS[@]}" "$APP"
else
  echo "  identity: $IDENTITY (via temp keychain — no login keychain access)"

  # Create a temporary keychain with no password, import the identity,
  # unlock it, sign with it, then clean up. The login keychain is
  # NEVER read → macOS NEVER prompts.
  security create-keychain -p "" "$TEMP_KEYCHAIN" 2>/dev/null || true
  security import "$P12" -k "$TEMP_KEYCHAIN" -P "lumen-dev" \
    -T /usr/bin/codesign 2>/dev/null || true
  security unlock-keychain -p "" "$TEMP_KEYCHAIN" 2>/dev/null || true
  security set-key-partition-list -S apple-tool:,apple: \
    -s -k "" "$TEMP_KEYCHAIN" 2>/dev/null || true

  ARGS+=(--keychain "$TEMP_KEYCHAIN")

  if ! codesign "${ARGS[@]}" "$APP"; then
    echo "ERROR: codesign failed; cleaning up temp keychain" >&2
    security delete-keychain "$TEMP_KEYCHAIN" 2>/dev/null || true
    echo "Retrying ad-hoc so the app at least launches…" >&2
    codesign --force --deep --sign - --timestamp=none "$APP"
    IDENTITY="-"
  else
    # Clean up the temporary keychain immediately.
    security delete-keychain "$TEMP_KEYCHAIN" 2>/dev/null || true
  fi
fi

echo "Verify:"
if codesign --verify --deep --strict --verbose=2 "$APP" 2>&1 | tail -8; then
  :
else
  echo "WARN: strict verify failed (common for ad-hoc); app may still run locally." >&2
fi
codesign -dv --verbose=2 "$APP" 2>&1 | grep -iE "Authority|Signature|TeamIdentifier|Identifier|Format|flags=" || true

if [[ "$IDENTITY" != "-" ]]; then
  echo "OK: signed with stable identity via temp keychain (zero keychain prompts)."
else
  echo "OK: ad-hoc signed."
fi
