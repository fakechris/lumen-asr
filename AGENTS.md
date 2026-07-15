# Lumen ASR agent rules

## Hard publication boundary

This repository is public. Competitor or vendor research, recovered private
evaluation context, prompt-optimization or training artifacts, internal design,
implementation plans, roadmaps, milestones, and evolution notes are local-only.

- Never stage, commit, push, attach to a pull request, or publish these materials.
- Never reclassify research as ordinary documentation to make it publishable.
- Keep private research outside the repository. `.gitignore` is defense in depth,
  not permission to place private material in the worktree.
- Do not weaken `.gitignore`, boundary scripts, workflows, or branch protection
  to make a change pass.
- If classification is uncertain, stop before `git add` and ask the user.

This rule applies to the root agent, subagents, automated review, generated
documents, and inherited commits in branch history.

## Mandatory memory and publish preflight

Before any staging, commit, push, pull request, tag, release, or merge:

1. Read the project learnings and apply `private-research-local-only`.
2. Read this file again after any context compaction or handoff.
3. Run `git fetch --prune origin`.
4. Create publication branches from current `origin/main` in a clean worktree.
5. Run `scripts/ci/check-public-repo-boundary.sh`.
6. Run `scripts/ci/check-pr-scope.sh origin/main HEAD`.
7. Inspect all of:
   - `git log --oneline origin/main..HEAD`
   - `git diff --stat origin/main...HEAD`
   - `git diff --name-only origin/main...HEAD`
   - GitHub PR `commits`, `files`, `changedFiles`, `additions`, and `deletions`.

Reviewing only `HEAD`, the latest commit, mergeability, or CI status is not a
publication review. Any mismatch between the requested scope and the complete
PR range is a hard stop: rebuild the branch from `origin/main`.

## Human authorization boundary

An agent must not merge a PR that changes publication-boundary enforcement,
branch protection, credentials, or repository visibility without explicit user
authorization in the current turn after reporting the exact diff and checks.
An agent must never approve its own exception or use a size/label override to
bypass the scope gate.
