#!/usr/bin/env bash

set -euo pipefail

base="$(git rev-parse "${1:?usage: check-pr-scope.sh <base-sha> [head-sha]}^{commit}")"
head="$(git rev-parse "${2:-HEAD}^{commit}")"

merge_base="$(git merge-base "$base" "$head")"
if [[ "$merge_base" != "$base" ]]; then
  printf 'PR branch is not based on the current base SHA.\n' >&2
  printf 'base: %s\nmerge-base: %s\n' "$base" "$merge_base" >&2
  exit 1
fi

commit_count="$(git rev-list --count "$base..$head")"
changed_files="$(git diff --name-only "$base...$head" | sed '/^$/d' | wc -l | tr -d ' ')"
added_lines="$(git diff --numstat "$base...$head" | awk '$1 ~ /^[0-9]+$/ { total += $1 } END { print total + 0 }')"
binary_files="$(git diff --numstat "$base...$head" | awk '$1 == "-" || $2 == "-" { count += 1 } END { print count + 0 }')"

printf 'PR scope: %s commits, %s files, %s added lines, %s binary files\n' \
  "$commit_count" "$changed_files" "$added_lines" "$binary_files"

if ((commit_count > 5 || changed_files > 30 || added_lines > 3000 || binary_files > 0)); then
  printf 'PR exceeds the default scope boundary (5 commits / 30 files / 3000 additions).\n' >&2
  printf 'Binary additions are not allowed. Split the PR before publishing.\n' >&2
  exit 1
fi

secret_pattern='-----BEGIN ([A-Z0-9 ]+ )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{20,}|/Users/[A-Za-z0-9._-]+/|[A-Za-z]:\\Users\\[^\\ ]+\\'
while IFS= read -r commit; do
  if git grep -I -n -E -e "$secret_pattern" "$commit" -- . \
    ':(exclude)scripts/ci/check-public-repo-boundary.sh' \
    ':(exclude)scripts/ci/check-pr-scope.sh' \
    ':(exclude)scripts/macos/create_local_codesign_identity.swift' \
    ':(exclude)docs/PUBLIC_REPOSITORY_BOUNDARY.md'; then
    printf 'possible credential or personal path exists in PR history at %s\n' "$commit" >&2
    exit 1
  fi
done < <(git rev-list --reverse "$base..$head")

printf 'PR ancestry and size boundary: clean\n'
