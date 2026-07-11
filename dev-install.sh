#!/usr/bin/env bash
# Convenience entry from repo root → scripts/macos/dev-install.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
exec "$ROOT/scripts/macos/dev-install.sh" "$@"
