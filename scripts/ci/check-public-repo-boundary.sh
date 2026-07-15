#!/usr/bin/env bash

set -euo pipefail
shopt -s nocasematch

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

violations=()

while IFS= read -r -d '' path; do
  case "$path" in
    .research/*|\
    .codex/*|\
    .mcp.json|\
    .playwright-mcp/*|\
    blind-review-*|\
    *.tsbuildinfo|\
    apps/desktop/dist/*|\
    benchmarks/*|\
    docs/research/*|\
    docs/superpowers/*|\
    docs/internal/*|\
    docs/private/*|\
    docs/*/internal/*|\
    docs/*/private/*|\
    docs/evolution/*|\
    docs/competitive/*|\
    docs/competitor/*|\
    docs/design/*|\
    docs/plans/*|\
    docs/planning/*|\
    docs/strategy/*|\
    docs/vendor/*|\
    docs/market/*|\
    docs/*research*|\
    docs/*competitive*|\
    docs/*competitor*|\
    docs/*evolution*|\
    docs/*design*|\
    docs/*plan*|\
    docs/*milestone*|\
    docs/*roadmap*|\
    docs/*strategy*|\
    docs/*vendor*|\
    docs/*market*|\
    docs/*selection*|\
    docs/*竞品*|\
    docs/*调研*|\
    docs/*研究*|\
    docs/*进化*)
      violations+=("$path")
      ;;
    .env|.env.*|*/.env|*/.env.*|*.pem|*.key|*.p12|*.pfx|id_rsa|id_rsa.*|*/id_rsa|*/id_rsa.*|id_ed25519|id_ed25519.*|*/id_ed25519|*/id_ed25519.*|*credentials*.json|*secrets*.json)
      violations+=("$path")
      ;;
  esac

  case "$path" in
    *.md|*.mdx|*.txt|*.pdf|*.docx)
      case "$path" in
        *research*|*competitive*|*competitor*|*evolution*|*implementation-plan*|*vendor-evaluation*|*market-landscape*|*roadmap*|*milestone*|*strategy*|*design*|*/planning/*|*/plans/*|*竞品*|*调研*|*研究*|*进化*)
          violations+=("$path")
          ;;
      esac
      ;;
  esac
done < <(git ls-files -z)

if ((${#violations[@]} > 0)); then
  printf 'public repository boundary violation:\n' >&2
  printf '  - %s\n' "${violations[@]}" | LC_ALL=C sort >&2
  printf 'Move internal material outside tracked paths before publishing.\n' >&2
  exit 1
fi

secret_pattern='-----BEGIN ([A-Z0-9 ]+ )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{20,}|/Users/[A-Za-z0-9._-]+/|[A-Za-z]:\\Users\\[^\\ ]+\\'
if git grep -I -n -E -e "$secret_pattern" -- . \
  ':(exclude)scripts/ci/check-public-repo-boundary.sh' \
  ':(exclude)scripts/macos/create_local_codesign_identity.swift' \
  ':(exclude)docs/PUBLIC_REPOSITORY_BOUNDARY.md'; then
  printf 'possible credential or personal absolute path found in tracked content\n' >&2
  exit 1
fi

printf 'public repository boundary: clean\n'
