#!/usr/bin/env bash

set -euo pipefail

empty_tree="$(git hash-object -t tree /dev/null)"

is_public_image_path() {
  case "$1" in
    docs/images/*.png|docs/images/*.PNG|\
    docs/images/*.jpg|docs/images/*.JPG|\
    docs/images/*.jpeg|docs/images/*.JPEG|\
    docs/images/*.webp|docs/images/*.WEBP|\
    docs/images/*.gif|docs/images/*.GIF)
      return 0
      ;;
  esac
  return 1
}

has_valid_public_image_magic() {
  local treeish="$1"
  local path="$2"
  local magic
  magic="$(git cat-file blob "${treeish}:${path}" | \
    od -An -tx1 -N12 | tr -d ' \n')"
  case "$path" in
    *.png|*.PNG)
      [[ "$magic" == 89504e470d0a1a0a* ]]
      ;;
    *.jpg|*.JPG|*.jpeg|*.JPEG)
      [[ "$magic" == ffd8ff* ]]
      ;;
    *.webp|*.WEBP)
      [[ "$magic" == 52494646????????57454250* ]]
      ;;
    *.gif|*.GIF)
      [[ "$magic" == 474946383761* || "$magic" == 474946383961* ]]
      ;;
    *)
      return 1
      ;;
  esac
}

has_valid_public_image_attestation() {
  local treeish="$1"
  local path="$2"
  local sidecar="${path}.public.md"
  local sidecar_type
  local asset_blob
  local expected
  local actual
  has_valid_public_image_magic "$treeish" "$path" || return 1
  sidecar_type="$(git cat-file -t "${treeish}:${sidecar}" 2>/dev/null || true)"
  [[ "$sidecar_type" == "blob" ]] || return 1
  asset_blob="$(git rev-parse "${treeish}:${path}")"
  expected="$(printf '%s\n' \
    'asset-class: public-product-ui' \
    'data-class: synthetic' \
    'human-reviewed: true' \
    "asset-blob: ${asset_blob}")"
  actual="$(git show "${treeish}:${sidecar}")"
  if [[ "$actual" == "$expected" ]]; then
    return 0
  fi
  expected="${expected/data-class: synthetic/data-class: public}"
  [[ "$actual" == "$expected" ]]
}

list_prohibited_binary_additions() {
  local treeish="$1"
  local previous="$2"
  local added deleted path
  while IFS=$'\t' read -r added deleted path; do
    if is_public_image_path "$path"; then
      local sidecar="${path}.public.md"
      if git diff --quiet "$previous" "$treeish" -- "$sidecar" || \
        ! has_valid_public_image_attestation "$treeish" "$path"; then
        printf '%s (missing, stale, or invalid %s attestation)\n' "$path" "$sidecar"
      fi
    elif [[ "$added" == "-" || "$deleted" == "-" ]]; then
      printf '%s\n' "$path"
    fi
  done
}

list_invalid_changed_sidecars() {
  local treeish="$1"
  local previous="$2"
  local sidecar path image_type sidecar_type
  while IFS= read -r sidecar; do
    [[ -n "$sidecar" ]] || continue
    path="${sidecar%.public.md}"
    image_type="$(git cat-file -t "${treeish}:${path}" 2>/dev/null || true)"
    sidecar_type="$(git cat-file -t "${treeish}:${sidecar}" 2>/dev/null || true)"
    if [[ -z "$image_type" && -z "$sidecar_type" ]]; then
      continue
    fi
    if [[ "$image_type" != "blob" ]] || ! is_public_image_path "$path" || \
      ! has_valid_public_image_attestation "$treeish" "$path"; then
      printf '%s\n' "$sidecar"
    fi
  done < <(git diff --name-only --no-renames --diff-filter=AMD \
    "$previous" "$treeish" -- 'docs/images/*.public.md')
}

usage='usage: check-pr-scope.sh <base-sha> [head-sha] [--history-only]'
base="$(git rev-parse "${1:?$usage}^{commit}")"
head="$(git rev-parse "${2:-HEAD}^{commit}")"
mode="${3:-}"
if [[ -n "$mode" && "$mode" != "--history-only" ]]; then
  printf '%s\n' "$usage" >&2
  exit 2
fi

merge_base="$(git merge-base "$base" "$head")"
if [[ "$merge_base" != "$base" ]]; then
  printf 'PR branch is not based on the current base SHA.\n' >&2
  printf 'base: %s\nmerge-base: %s\n' "$base" "$merge_base" >&2
  exit 1
fi

commit_count="$(git rev-list --count "$base..$head")"
changed_files="$(git diff --name-only "$base...$head" | sed '/^$/d' | wc -l | tr -d ' ')"
added_lines="$(git diff --numstat "$base...$head" | awk '$1 ~ /^[0-9]+$/ { total += $1 } END { print total + 0 }')"
prohibited_binary_files="$(git diff --numstat --no-renames --diff-filter=AM \
  "$base...$head" | list_prohibited_binary_additions "$head" "$base" | \
  sed '/^$/d' | wc -l | tr -d ' ')"

if [[ "$mode" == "--history-only" ]]; then
  printf 'Push history scope: %s commits\n' "$commit_count"
else
  printf 'PR scope: %s commits, %s files, %s added lines, %s prohibited binary additions\n' \
    "$commit_count" "$changed_files" "$added_lines" "$prohibited_binary_files"

  if ((commit_count > 5 || changed_files > 30 || added_lines > 3000 || prohibited_binary_files > 0)); then
    printf 'PR exceeds the default scope boundary (5 commits / 30 files / 3000 additions).\n' >&2
    printf 'Binary additions are allowed only for public docs/images assets. Split or sanitize the PR.\n' >&2
    exit 1
  fi
fi

while IFS= read -r commit; do
  first_parent="$(git rev-list --parents -n 1 "$commit" | awk '{ print $2 }')"
  if [[ -n "$first_parent" ]]; then
    prohibited_binary_paths="$(git diff --numstat --no-renames --diff-filter=AM \
      "$first_parent" "$commit" | \
      list_prohibited_binary_additions "$commit" "$first_parent")"
    invalid_sidecars="$(list_invalid_changed_sidecars "$commit" "$first_parent")"
  else
    prohibited_binary_paths="$(git diff-tree --root --no-commit-id --numstat -r \
      --no-renames --diff-filter=AM "$commit" | \
      list_prohibited_binary_additions "$commit" "$empty_tree")"
    invalid_sidecars="$(list_invalid_changed_sidecars "$commit" "$empty_tree")"
  fi
  if [[ -n "$prohibited_binary_paths" ]]; then
    printf 'prohibited binary addition exists in published history at %s:\n' \
      "$commit" >&2
    printf '  - %s\n' "$prohibited_binary_paths" >&2
    exit 1
  fi
  if [[ -n "$invalid_sidecars" ]]; then
    printf 'invalid or orphaned public-image attestation exists at %s:\n' \
      "$commit" >&2
    printf '  - %s\n' "$invalid_sidecars" >&2
    exit 1
  fi

  if ! scripts/ci/check-public-repo-boundary.sh "$commit" >/dev/null; then
    printf 'public repository boundary violation exists in PR history at %s\n' "$commit" >&2
    scripts/ci/check-public-repo-boundary.sh "$commit" || true
    exit 1
  fi
done < <(git rev-list --reverse "$base..$head")

if [[ "$mode" == "--history-only" ]]; then
  printf 'push ancestry and complete history boundary: clean\n'
else
  printf 'PR ancestry, size, and complete history boundary: clean\n'
fi
