#!/usr/bin/env bash

set -euo pipefail

base="$(git rev-parse "${1:?usage: check-pr-scope.sh <base-sha> [head-sha]}^{commit}")"
head="$(git rev-parse "${2:-HEAD}^{commit}")"
allow_large="${ALLOW_LARGE_SCOPE:-false}"

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

if [[ "$allow_large" == "true" ]]; then
  printf 'large-scope-approved: explicit size override accepted\n'
  exit 0
fi

if ((commit_count > 5 || changed_files > 30 || added_lines > 3000 || binary_files > 0)); then
  printf 'PR exceeds the default scope boundary (5 commits / 30 files / 3000 additions).\n' >&2
  printf 'Binary additions also require large-scope-approved.\n' >&2
  printf 'Split the PR or apply large-scope-approved after explicit review.\n' >&2
  exit 1
fi

printf 'PR ancestry and size boundary: clean\n'
