# Lumen ASR agent rules

## Public and local document taxonomy

This repository is public. Classify material before creating a file.

Public, durable documentation may be tracked only in these locations:

- `docs/product/` for shipped or approved product behavior;
- `docs/ui/` for public UI/UX specifications;
- `docs/release/` for reproducible, sanitized packaging and release guidance;
- `docs/architecture/` for approved public technical contracts;
- `docs/governance/` for repository policy;
- `docs/images/` for public documentation assets;
- `docs/README.md` and the cross-repository `docs/SHARED_MODELS_CONTRACT.md`.

The repository-level `README.md`, `PRODUCT.md`, and `ARCHITECTURE.md`, plus
asset-local README files, are public entry documents outside this taxonomy.

All research starts in `.research/docs/<topic>/`, which is ignored as a whole.
Never create research under `docs/`, even temporarily. Suggested local topics
include `asr/`, `context/`, `benchmarks/`, `competitive/`, `platform/`, and
`experiments/`.

Product/UI documentation is public only when it describes approved behavior.
Release/DMG documentation is public only after removing personal Apple IDs,
Team IDs, certificate details, credentials, private asset URLs, and machine
specific values.

New or modified binary documentation images are allowed only under
`docs/images/` as PNG, JPEG, WebP, or GIF. The same commit must add a matching
`<image-path>.public.md` sidecar confirming that a human inspected the asset and
that it contains only approved public product/UI material with synthetic or
otherwise public data. The sidecar must contain `asset-class:
public-product-ui`, `data-class: synthetic` or `data-class: public`,
`human-reviewed: true`, and the exact Git blob ID as `asset-blob: <oid>`. A
sidecar cannot override the research boundary.

## Hard publication boundary

The following are local-only and must never be staged, committed, pushed,
attached to a pull request, tagged, released, or inherited through branch
history:

- ASR provider selection, capability research, vendor comparisons, experiment
  notes, prompt optimization, training artifacts, and unpublished gaps;
- the Context capture/inference pipeline, its source code, schemas, internal
  architecture, privacy experiments, implementation notes, and roadmaps;
- benchmark harnesses, private reference tooling, datasets, audio, transcripts,
  reports, results, and evaluation methodology;
- internal roadmaps, milestones, evolution notes, local automation state,
  credentials, personal identifiers, and generated desktop builds.

Do not rename or reclassify prohibited material as product, UI, architecture,
or release documentation to make it publishable. `.gitignore` is defense in
depth, not permission to keep private material in a trackable location. If the
classification is uncertain, stop before `git add` and ask the user.

This rule applies to the root agent, subagents, automated review, generated
documents, and every commit in the complete PR range.

## Mandatory publish preflight

Before any staging, commit, push, pull request, tag, release, or merge:

1. Read the project learning `private-research-local-only` and this file again
   after any context compaction or handoff.
2. Run `git fetch --prune origin`.
3. Create the publication branch from current `origin/main` in a clean worktree.
4. Run `scripts/ci/test-public-repo-boundary.sh`.
5. Run `scripts/ci/check-public-repo-boundary.sh`.
6. Run `scripts/ci/check-pr-scope.sh origin/main HEAD`.
7. Inspect all of:
   - `git log --oneline origin/main..HEAD`
   - `git diff --stat origin/main...HEAD`
   - `git diff --name-only origin/main...HEAD`
   - GitHub PR `commits`, `files`, `changedFiles`, `additions`, and `deletions`.

Reviewing only `HEAD`, the latest commit, mergeability, or CI status is not a
publication review. Any mismatch between the requested scope and the complete
PR range is a hard stop: rebuild the branch from current `origin/main`.

## Human authorization boundary

An agent must not merge a PR that changes publication-boundary enforcement,
branch protection, credentials, or repository visibility without explicit user
authorization in the current turn after reporting the exact diff and checks.
An agent must never approve its own exception or use an override to bypass the
scope gate.
