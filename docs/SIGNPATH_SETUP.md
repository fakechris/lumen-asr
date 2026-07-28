# SignPath Foundation onboarding

This document records the repository-side setup for the SignPath Foundation
open-source application. It does not contain credentials or claim that the
application has already been accepted.

## Application URLs

- Repository: <https://github.com/fakechris/lumen-asr>
- Homepage: <https://github.com/fakechris/lumen-asr>
- Downloads: <https://github.com/fakechris/lumen-asr/releases>
- Privacy policy:
  <https://github.com/fakechris/lumen-asr/blob/main/PRIVACY.md>
- Code signing policy:
  <https://github.com/fakechris/lumen-asr/blob/main/CODE_SIGNING_POLICY.md>

## Proposed application text

**Tagline**

> A local-first desktop dictation app for fast, private speech-to-text.

**Description**

> Lumen ASR is an open-source desktop dictation application for macOS and
> Windows. It records speech while a user-controlled hotkey is active, performs
> local speech recognition by default, optionally corrects or translates the
> transcript, and delivers the result to the user's current workflow. Cloud
> recognition and correction are optional and use endpoints selected by the
> user.

The reputation field must contain only current, verifiable evidence such as
release download statistics, independent articles, community discussions, or
adoption reports. Do not substitute CI results for user reputation.

Select:

- Maintainer type: `Individual maintainer(s)`
- Build system: `GitHub Actions`

The maintainer must personally provide the account name and email address,
accept the SignPath Foundation Code of Conduct, and consent to processing of
the application data.

## After acceptance

1. Install the SignPath GitHub App for `fakechris/lumen-asr`.
2. Link the predefined GitHub.com trusted build system to the SignPath project.
3. Create a release signing policy restricted to this repository and version
   tags, with origin verification and one manual approval.
4. Create an artifact configuration for the directly distributed Windows NSIS
   installer. Sign the Lumen ASR installer and Lumen ASR executable only; do
   not apply the project certificate to redistributed third-party binaries.
5. Add the organization ID, project slug, signing policy slug, artifact
   configuration slug, and submitter API token to GitHub repository variables
   and secrets.
6. Update the tag release workflow to upload the unsigned artifact, submit its
   GitHub artifact ID through
   `signpath/github-action-submit-signing-request@v2`, download the approved
   signed artifact, verify its Authenticode signature, and publish only the
   signed result.

Microsoft Store MSIX packages remain on the Partner Center signing path because
their manifest publisher identity must match the Microsoft Store publisher.
