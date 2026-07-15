# Public repository boundary

Lumen ASR is a public repository. Internal research, competitive analysis,
evolution plans, private test data, credentials, local automation state, and
generated desktop builds must never be tracked here.

## Prohibited tracked content

- `.research/` and internal vendor or market research;
- internal research, vendor/market evaluation, competitive analysis, design,
  implementation plan, milestone, roadmap, strategy, and evolution documents;
- private benchmark datasets, audio, transcripts, reports, and databases under
  `benchmarks/`;
- `.codex/`, `.mcp.json`, `.playwright-mcp/`, blind-review screenshots, and
  TypeScript incremental build state;
- generated files under `apps/desktop/dist/`;
- credentials, tokens, private keys, personal paths, or user data.

The ignore rules reduce accidental staging. The executable boundary check
rejects prohibited tracked paths, high-confidence credential signatures,
personal absolute paths, and unapproved binary PR additions. It is a
defense-in-depth gate, not a substitute for human review of meaning or privacy.

## Required publish preflight

Start every publish branch from the current remote default branch in a clean
worktree:

```bash
git fetch --prune origin
git worktree add -b codex/<scope> ../lumen-asr-<scope> origin/main
```

Before pushing, run:

```bash
scripts/ci/check-public-repo-boundary.sh
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
git diff --name-only origin/main...HEAD
```

Before merging, inspect the GitHub PR totals and file list. A PR whose commit
list, changed-file count, or line count exceeds the declared scope must stop and
be rebuilt from `origin/main`; mergeability alone is not approval.

The automated scope gate rejects a branch whose pull-request base is not its
merge base. By default it also rejects more than 5 commits, 30 changed files, or
3,000 added lines. A deliberately large change requires the
`large-scope-approved` label after explicit file-list review.

## Enforcement

The `Public repository boundary` workflow runs for every pull request and every
push to `main`. A prohibited tracked path fails the workflow. The pull request
template repeats the human review of intent that cannot be inferred safely from
file patterns alone.
