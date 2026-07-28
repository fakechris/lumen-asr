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
- The project is applying to the SignPath Foundation open-source program for
  directly distributed Windows artifacts.
- **Free code signing provided by
  [SignPath.io](https://signpath.io/), certificate by
  [SignPath Foundation](https://signpath.org/).** This statement applies once
  the application is accepted. Until then, release notes identify Windows
  artifacts as unsigned.
- macOS artifacts use an Apple-issued Developer ID certificate and Apple's
  notarization service when signed direct-distribution builds are published.
- macOS prereleases may use ad-hoc signatures for bundle integrity. An ad-hoc
  signature does not establish a trusted publisher identity or qualify the
  artifact for Apple notarization.
- Release notes must clearly classify each platform's artifacts as
  provider-signed and notarized where applicable, ad-hoc signed, or unsigned.

## Project roles and approval

- **Author:** [Chris Song (`@fakechris`)](https://github.com/fakechris), the
  repository owner and maintainer.
- **Reviewer:** [Chris Song (`@fakechris`)](https://github.com/fakechris).
  Contributions from people without direct commit access are reviewed before
  merge.
- **Approver:** [Chris Song (`@fakechris`)](https://github.com/fakechris), who
  manually approves each release-signing request.
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

The project's handling of microphone audio, transcripts, context, local
storage, and optional network providers is documented in the
[privacy policy](./PRIVACY.md).

Security issues can be reported privately through the repository's
[GitHub security advisory form](https://github.com/fakechris/lumen-asr/security/advisories/new).
