## Desktop installation notes

Download only from the official
[Lumen ASR GitHub Releases](https://github.com/fakechris/lumen-asr/releases)
page and verify the matching entry in `SHA256SUMS.txt`.

### Windows

Download `Lumen-ASR-<version>-windows-x64-setup.exe`. The installer is a
per-user NSIS package and can be removed from Windows Settings.

The project is applying to SignPath Foundation. **Free code signing provided
by [SignPath.io](https://signpath.io/), certificate by
[SignPath Foundation](https://signpath.org/).** Until the application is
accepted and the release workflow is connected, Windows downloads are
explicitly marked unsigned and Windows may display SmartScreen or Smart App
Control warnings.

Microsoft Store MSIX packages use a separate Partner Center submission and
Microsoft signing path. They are not signed with the SignPath Foundation
certificate.

### macOS

- Apple Silicon: download the `arm64.dmg`.
- Intel Mac builds are not provided.

Current DMGs use ad-hoc code signing and are not notarized with an Apple
Developer ID. On first launch, macOS may require approval under
**System Settings → Privacy & Security**. After launch, follow the onboarding
instructions for Microphone and Accessibility permissions.

### Privacy and source provenance

Lumen ASR is local-first. Optional cloud speech recognition and correction send
data only to endpoints configured by the user. See the
[privacy policy](https://github.com/fakechris/lumen-asr/blob/main/PRIVACY.md)
and
[code signing policy](https://github.com/fakechris/lumen-asr/blob/main/CODE_SIGNING_POLICY.md).
