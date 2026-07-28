# Code signing policy

This policy describes how official Lumen ASR release artifacts are produced
and, where available, code signed.

## Source and release provenance

- The canonical source repository is
  <https://github.com/fakechris/lumen-asr>.
- Official binary releases are produced from version tags in that repository
  by the repository's GitHub Actions workflows.
- Release workflows must build from the tagged source. Maintainers must not
  substitute locally built or manually modified binaries for signing.
- Published checksums and signatures cover the exact artifacts attached to the
  corresponding GitHub release.

## Signing providers

- Microsoft Store MSIX packages are submitted to Microsoft for certification
  and Store signing.
- Directly distributed Windows artifacts may be signed through SignPath once
  the project is accepted into the SignPath Foundation open-source program.
- macOS artifacts use an Apple-issued Developer ID certificate and Apple's
  notarization service when signed direct-distribution builds are published.
- Until a signing provider is configured for a platform, release notes must
  clearly identify that platform's artifacts as unsigned.

## Project roles and approval

- **Authors** are repository maintainers with permission to modify source code
  and release workflows.
- **Reviewers** are maintainers who review contributions from people without
  direct commit access before those changes are merged.
- **Approvers** are maintainers authorized to approve release signing
  requests.
- Maintainers must use multi-factor authentication for GitHub and signing
  provider access.
- Signing requests are limited to official tagged releases produced by the
  repository's automated release workflow.

## Security controls

- Changes to build, packaging, dependency, release, or signing configuration
  receive the same review as application source changes.
- Signing credentials and private keys must not be stored in the repository.
- Signing keys must remain in the signing provider, platform service, hardware
  security module, or protected CI secret store.
- A release must not be signed when its source provenance, workflow result, or
  artifact integrity cannot be verified.

Security issues can be reported privately through the repository's
[GitHub security advisory form](https://github.com/fakechris/lumen-asr/security/advisories/new).
