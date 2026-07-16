## Scope

- [ ] I fetched and pruned `origin` before creating this branch.
- [ ] This branch was created from the current `origin/main`, not a divergent local branch.
- [ ] `git log --oneline origin/main..HEAD` contains only the commits described by this PR.
- [ ] `git diff --stat origin/main...HEAD` matches the declared scope.
- [ ] The automated ancestry and size gate passes without an override.

## Public repository boundary

- [ ] New research was created only under ignored `.research/docs/<topic>/`, never under `docs/`.
- [ ] Public documentation is limited to product/UI behavior, sanitized release material, approved architecture, governance, or public assets.
- [ ] No ASR provider/capability research, Context capture/inference pipeline, benchmark code/data/methodology, or private evaluation material is present in any PR commit.
- [ ] Release docs contain no credentials, personal Apple IDs/Team IDs, certificate snapshots, private URLs, or machine-specific values.
- [ ] Each new/modified binary `docs/images/` asset has a same-commit `.public.md` sidecar and was inspected for private data.
- [ ] `scripts/ci/test-public-repo-boundary.sh` passes.
- [ ] `scripts/ci/check-public-repo-boundary.sh` passes.
- [ ] `scripts/ci/check-pr-scope.sh origin/main HEAD` passes.
- [ ] Any unusually large PR has been split or explicitly justified before review.

## Validation

<!-- List the exact checks run for this change. -->
