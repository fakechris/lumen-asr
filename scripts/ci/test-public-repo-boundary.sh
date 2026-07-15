#!/usr/bin/env bash

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
checker="$repo_root/scripts/ci/check-public-repo-boundary.sh"
fixtures="$(mktemp -d "${TMPDIR:-/tmp}/lumen-boundary-tests.XXXXXX")"
trap 'rm -rf "$fixtures"' EXIT

fail() {
  printf 'boundary regression test failed: %s\n' "$*" >&2
  exit 1
}

assert_ignored() {
  local path="$1"
  git check-ignore --no-index -q "$path" || fail "expected ignored path: $path"
}

assert_not_ignored() {
  local path="$1"
  if git check-ignore --no-index -q "$path"; then
    fail "expected public path to remain trackable: $path"
  fi
}

new_fixture() {
  local name="$1"
  local dir="$fixtures/$name"
  git init -q "$dir"
  git -C "$dir" config user.name boundary-test
  git -C "$dir" config user.email boundary-test@example.invalid
  printf '%s\n' "$dir"
}

commit_fixture() {
  local dir="$1"
  git -C "$dir" add -f .
  git -C "$dir" commit -qm fixture
}

expect_pass() {
  local dir="$1"
  if ! (cd "$dir" && "$checker" >/dev/null 2>&1); then
    (cd "$dir" && "$checker") || true
    fail "expected checker to pass: $dir"
  fi
}

expect_fail() {
  local dir="$1"
  if (cd "$dir" && "$checker" >/dev/null 2>&1); then
    fail "expected checker to reject: $dir"
  fi
}

assert_ignored .research/docs/asr/provider-selection.md
assert_ignored research/docs/asr/provider-selection.md
assert_ignored crates/lumen-bench/src/lib.rs
assert_ignored benchmarks/private-results.json
assert_ignored apps/desktop/src-tauri/src/context_capture.rs
assert_ignored notes/ASR_CAPABILITY_SELECTION.md
assert_ignored notes/CONTEXT_PIPELINE.md
assert_ignored notes/PRIVATE_BENCHMARK.md
assert_ignored notes/INTERNAL_ROADMAP.md
assert_ignored notes/VOICE_RESULTS.csv
assert_ignored planning/implementation.md
assert_not_ignored docs/product/BEHAVIOR.md
assert_not_ignored docs/ui/INTERACTION_DESIGN.md
assert_not_ignored docs/ui/PROVIDER_SELECTION.md
assert_not_ignored docs/release/macos/GITHUB_RELEASE.md
assert_not_ignored src/retry_strategy.rs
assert_not_ignored src/plans/mod.rs

dir="$(new_fixture public-docs)"
mkdir -p "$dir/docs/product" "$dir/docs/ui" "$dir/docs/release/macos" \
  "$dir/docs/governance" "$dir/docs/architecture" "$dir/docs/images"
printf 'approved product behavior\n' >"$dir/docs/product/BEHAVIOR.md"
printf 'The ASR provider selection dropdown persists engine selection.\nThe selected engine is disabled because its API key is missing.\nWe recommend adding an API key before saving.\nAfter testing the microphone, proceed.\n' \
  >"$dir/docs/product/PROVIDER_SELECTION.md"
printf 'public interaction design\n' >"$dir/docs/ui/INTERACTION_DESIGN.md"
printf 'sanitized release procedure\n' >"$dir/docs/release/macos/GITHUB_RELEASE.md"
printf 'Next steps: grant microphone access.\n' \
  >"$dir/docs/release/macos/FIRST_LAUNCH.md"
printf 'repository policy\n' >"$dir/docs/governance/POLICY.md"
printf 'public architecture\n' >"$dir/docs/architecture/OVERVIEW.md"
printf 'documentation map\n' >"$dir/docs/README.md"
printf 'shared contract\n' >"$dir/docs/SHARED_MODELS_CONTRACT.md"
commit_fixture "$dir"
expect_pass "$dir"

dir="$(new_fixture tracked-research-root)"
mkdir -p "$dir/.research/docs/asr"
printf 'provider notes\n' >"$dir/.research/docs/asr/notes.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture research-under-public-docs)"
mkdir -p "$dir/docs/research"
printf 'ASR provider comparison\n' >"$dir/docs/research/ASR_SELECTION.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture disguised-asr-selection)"
mkdir -p "$dir/docs/product"
printf 'provider matrix\n' >"$dir/docs/product/ASR_CAPABILITY_SELECTION.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture research-outside-docs)"
mkdir -p "$dir/notes"
printf 'provider matrix\n' >"$dir/notes/ASR_CAPABILITY_SELECTION.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture neutral-name-asr-research)"
mkdir -p "$dir/docs/product"
printf 'ASR provider comparison matrix\n' >"$dir/docs/product/VOICE_OPTIONS.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture neutral-name-research-outside-docs)"
mkdir -p "$dir/notes"
printf 'ASR provider comparison matrix\n' >"$dir/notes/VOICE_OPTIONS.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture structured-research-csv)"
mkdir -p "$dir/notes"
printf 'provider,WER\nengine-x,0.12\n' >"$dir/notes/VOICE_RESULTS.csv"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture structured-research-json)"
mkdir -p "$dir/notes"
printf '{"provider":"engine-x","wer":0.12}\n' >"$dir/notes/VOICE_RESULTS.json"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture multiline-selection-research)"
mkdir -p "$dir/docs/product"
printf 'ASR provider selection\nWe chose Engine X after testing.\n' \
  >"$dir/docs/product/VOICE_DECISION.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture neutral-name-context-doc)"
mkdir -p "$dir/docs/architecture"
printf 'Context capture pipeline architecture\n' >"$dir/docs/architecture/WINDOW_SNAPSHOT.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture internal-roadmap)"
mkdir -p "$dir/docs/product"
printf 'future sequencing\n' >"$dir/docs/product/INTERNAL_ROADMAP.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture neutral-name-internal-plan)"
mkdir -p "$dir/docs/product"
printf 'Internal roadmap: Q3 and Q4 delivery sequencing.\n' \
  >"$dir/docs/product/DELIVERY_SEQUENCE.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture html-roadmap)"
mkdir -p "$dir/notes"
printf '<h1>Delivery sequence</h1>\n' >"$dir/notes/ROADMAP.html"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture svg-roadmap)"
mkdir -p "$dir/docs/images"
printf '<svg xmlns="http://www.w3.org/2000/svg"></svg>\n' \
  >"$dir/docs/images/INTERNAL_ROADMAP.svg"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture context-path)"
mkdir -p "$dir/apps/desktop/src-tauri/src"
printf 'pub struct Capture;\n' >"$dir/apps/desktop/src-tauri/src/context_capture.rs"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture renamed-context-source)"
mkdir -p "$dir/src"
printf 'pub struct ContextCaptureState;\n' >"$dir/src/private_feature.rs"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture benchmark-crate)"
mkdir -p "$dir/crates/lumen-bench/src"
printf 'pub fn score() {}\n' >"$dir/crates/lumen-bench/src/lib.rs"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture benchmark-doc)"
mkdir -p "$dir/docs/product"
printf 'evaluation method\n' >"$dir/docs/product/PRIVATE_BENCHMARK.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture personal-signing-snapshot)"
mkdir -p "$dir/docs/release/macos"
printf 'Apple Development: owner@example.com (ABCDEFGHIJ)\n' \
  >"$dir/docs/release/macos/LOCAL_SIGNING.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture named-signing-snapshot)"
mkdir -p "$dir/docs/release/macos"
printf 'Apple Development: Jane Doe (ABCDEFGHIJ)\n' \
  >"$dir/docs/release/macos/LOCAL_SIGNING.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture email-signing-snapshot)"
mkdir -p "$dir/docs/release/macos"
printf 'Apple Development: owner@example.com\n' \
  >"$dir/docs/release/macos/LOCAL_SIGNING.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture team-identifier-snapshot)"
mkdir -p "$dir/docs/release/macos"
printf 'TeamIdentifier=ABCDEFGHIJ\n' >"$dir/docs/release/macos/LOCAL_SIGNING.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture spaced-team-identifier-snapshot)"
mkdir -p "$dir/docs/release/macos"
printf 'TeamIdentifier = ABCDEFGHIJ\nTeam ID: ABCDEFGHIJ\n' \
  >"$dir/docs/release/macos/LOCAL_SIGNING.md"
commit_fixture "$dir"
expect_fail "$dir"

dir="$(new_fixture transient-history-leak)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf 'ASR provider comparison matrix\n' >"$dir/docs/product/VOICE_OPTIONS.md"
git -C "$dir" add -f docs/product/VOICE_OPTIONS.md
git -C "$dir" commit -qm add-private-research
git -C "$dir" rm -q docs/product/VOICE_OPTIONS.md
git -C "$dir" commit -qm remove-private-research
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected scope checker to reject a transient history leak"
fi

dir="$(new_fixture transient-binary-history-leak)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf '\0private evaluation\0' >"$dir/docs/product/VOICE_OPTIONS.pdf"
git -C "$dir" add -f docs/product/VOICE_OPTIONS.pdf
git -C "$dir" commit -qm add-private-binary-research
git -C "$dir" rm -q docs/product/VOICE_OPTIONS.pdf
git -C "$dir" commit -qm remove-private-binary-research
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected scope checker to reject a transient binary history leak"
fi

dir="$(new_fixture public-image-history)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product" "$dir/docs/images"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf '\211PNG\r\n\032\n\0public screenshot\0' >"$dir/docs/images/ONBOARDING.png"
asset_blob="$(git -C "$dir" hash-object docs/images/ONBOARDING.png)"
printf 'asset-class: public-product-ui\ndata-class: synthetic\nhuman-reviewed: true\nasset-blob: %s\n' \
  "$asset_blob" \
  >"$dir/docs/images/ONBOARDING.png.public.md"
git -C "$dir" add docs/images/ONBOARDING.png \
  docs/images/ONBOARDING.png.public.md
git -C "$dir" commit -qm add-public-doc-image
if ! (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected sanitized docs/images assets to pass history checks"
fi
if ! (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD >/dev/null 2>&1); then
  fail "expected sanitized docs/images assets to pass PR checks"
fi
reviewed_image_commit="$(git -C "$dir" rev-parse HEAD)"
printf '\211PNG\r\n\032\n\0changed screenshot\0' >"$dir/docs/images/ONBOARDING.png"
git -C "$dir" add docs/images/ONBOARDING.png
git -C "$dir" commit -qm change-image-without-new-review
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$reviewed_image_commit" HEAD --history-only >/dev/null 2>&1); then
  fail "expected a changed image with a stale sidecar to fail"
fi

dir="$(new_fixture image-without-review-sidecar)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product" "$dir/docs/images"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf '\211PNG\r\n\032\n\0unreviewed screenshot\0' \
  >"$dir/docs/images/VOICE_RESULTS.png"
git -C "$dir" add docs/images/VOICE_RESULTS.png
git -C "$dir" commit -qm add-unreviewed-doc-image
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected docs/images assets without review sidecars to fail"
fi

dir="$(new_fixture text-disguised-as-image)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product" "$dir/docs/images"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf 'provider,WER\nengine-x,0.12\n' >"$dir/docs/images/VOICE_RESULTS.png"
git -C "$dir" add -f docs/images/VOICE_RESULTS.png
git -C "$dir" commit -qm add-text-disguised-as-image
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected text or LFS content disguised as an image to fail"
fi

dir="$(new_fixture image-with-empty-sidecar)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product" "$dir/docs/images"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
printf '\211PNG\r\n\032\n\0unreviewed screenshot\0' \
  >"$dir/docs/images/VOICE_RESULTS.png"
asset_blob="$(git -C "$dir" hash-object docs/images/VOICE_RESULTS.png)"
printf 'asset-class: public-product-ui\ndata-class: synthetic\ndata-class: private\nhuman-reviewed: true\nhuman-reviewed: false\nasset-blob: %s\n' \
  "$asset_blob" >"$dir/docs/images/VOICE_RESULTS.png.public.md"
git -C "$dir" add docs/images/VOICE_RESULTS.png \
  docs/images/VOICE_RESULTS.png.public.md
git -C "$dir" commit -qm add-empty-image-sidecar
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected a contradictory docs/images review sidecar to fail"
fi

dir="$(new_fixture deleted-image-sidecar)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product" "$dir/docs/images"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
printf '\211PNG\r\n\032\n\0public screenshot\0' >"$dir/docs/images/ONBOARDING.png"
asset_blob="$(git -C "$dir" hash-object docs/images/ONBOARDING.png)"
printf 'asset-class: public-product-ui\ndata-class: synthetic\nhuman-reviewed: true\nasset-blob: %s\n' \
  "$asset_blob" >"$dir/docs/images/ONBOARDING.png.public.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" rm -q docs/images/ONBOARDING.png.public.md
git -C "$dir" commit -qm delete-image-review-sidecar
if (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected deletion of a live image sidecar to fail"
fi

dir="$(new_fixture clean-long-push-history)"
mkdir -p "$dir/scripts/ci" "$dir/docs/product"
cp "$checker" "$dir/scripts/ci/check-public-repo-boundary.sh"
cp "$repo_root/scripts/ci/check-pr-scope.sh" "$dir/scripts/ci/check-pr-scope.sh"
chmod +x "$dir/scripts/ci/"*.sh
printf 'approved behavior\n' >"$dir/docs/product/BEHAVIOR.md"
commit_fixture "$dir"
base="$(git -C "$dir" rev-parse HEAD)"
for revision in 1 2 3 4 5 6; do
  printf 'approved behavior revision %s\n' "$revision" \
    >"$dir/docs/product/BEHAVIOR.md"
  git -C "$dir" add docs/product/BEHAVIOR.md
  git -C "$dir" commit -qm "approved-behavior-$revision"
done
if ! (cd "$dir" && scripts/ci/check-pr-scope.sh \
  "$base" HEAD --history-only >/dev/null 2>&1); then
  fail "expected history-only mode to allow more than five clean commits"
fi

printf 'public repository boundary regression tests: clean\n'
