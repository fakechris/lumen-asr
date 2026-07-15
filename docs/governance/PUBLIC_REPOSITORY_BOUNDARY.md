# Public repository boundary

Lumen ASR is a public repository. The tracked `docs/` tree is a publication
surface, not a working-notes directory.

## Directory contract

Public documentation is restricted to the categories listed in
[`docs/README.md`](../README.md): product, UI, sanitized release material,
approved architecture, governance, and public assets. Root `README.md`,
`PRODUCT.md`, and `ARCHITECTURE.md` remain public repository entry documents.

All research begins in `.research/docs/<topic>/`. The whole `.research/` tree is
ignored and the boundary checker rejects it even if it is force-added. There is
no tracked `docs/research/` staging area.

## Prohibited tracked content

- ASR provider/capability selection, vendor evaluation, experiments, prompt
  optimization, training material, or unpublished implementation gaps;
- Context capture/inference pipeline code, schemas, architecture, privacy
  experiments, internal design, and planning;
- benchmark harnesses, private reference tools, datasets, audio, transcripts,
  methodology, reports, and results;
- internal roadmaps, milestones, evolution notes, local agent/browser state,
  generated desktop builds, credentials, personal identifiers, or user data.

Product and UI specifications are allowed when they describe approved public
behavior. Release and DMG material is allowed only after removing credentials,
personal Apple IDs and Team IDs, certificate snapshots, private asset URLs, and
machine-specific values.

New or modified binary images may appear only under `docs/images/` as PNG,
JPEG, WebP, or GIF, with a matching `<image-path>.public.md` review sidecar in
the same commit. The sidecar must confirm human inspection and approved public,
synthetic, or otherwise public content using `asset-class:
public-product-ui`, `data-class: synthetic` or `data-class: public`,
`human-reviewed: true`, and `asset-blob: <current-git-blob-oid>`. It cannot
authorize research, private evaluation results, user data, or sensitive
screenshots.

## Required publish preflight

Start every publication branch from the current remote default branch in a
clean worktree:

```bash
git fetch --prune origin
git worktree add -b codex/<scope> ../lumen-asr-<scope> origin/main
```

Before pushing, run:

```bash
scripts/ci/test-public-repo-boundary.sh
scripts/ci/check-public-repo-boundary.sh
scripts/ci/check-pr-scope.sh origin/main HEAD
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
git diff --name-only origin/main...HEAD
```

Before merging, inspect the GitHub PR commit count, totals, and complete file
list. A mismatch between the declared scope and any commit in the PR range is a
hard stop; mergeability alone is not approval.

## Enforcement

The boundary workflow runs checker regression tests and scans the complete
tracked tree on every pull request and every branch push. Push checks inspect
every new commit, including material added and removed within the same range.
Pull-request checks additionally require the PR base to be its merge base and
reject more than 5 commits, 30 changed files, 3,000 added lines, or any binary
file by default. There is no agent-controlled override.
