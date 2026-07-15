## Scope

- [ ] I fetched and pruned `origin` before creating this branch.
- [ ] This branch was created from the current `origin/main`, not a divergent local branch.
- [ ] `git log --oneline origin/main..HEAD` contains only the commits described by this PR.
- [ ] `git diff --stat origin/main...HEAD` matches the declared scope.
- [ ] The automated ancestry and size gate passes without an override.

## Public repository boundary

- [ ] `scripts/ci/check-public-repo-boundary.sh` passes.
- [ ] No internal research, competitive analysis, evolution plan, private benchmark data, credentials, local agent state, or generated desktop build output is tracked.
- [ ] Any unusually large PR has been split or explicitly justified before review.

## Validation

<!-- List the exact checks run for this change. -->
