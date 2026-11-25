## Lumen ASR v0.4.0 — 2026-11-24

The first release with **Meetings** as a full product surface, plus realtime
dictation and a round of reliability and safety work across the dictation path.

### Highlights

**Meetings**
- Dual-track recording (microphone + system audio), echo suppression, Ogg-Opus by default, silence auto-stop, max-duration cap, low-battery and sleep warnings.
- Meeting detection: a prompt when a call starts and a stop suggestion when it ends — it never records or stops on its own.
- Speaker diarization, live speaker labels and a global voiceprint library (enroll, rename, merge, re-identify).
- Structured minutes from built-in or your own templates; long meetings are summarized in windows and malformed model output is repaired or retried.
- **Draft minutes** from the live transcript within seconds of stopping, replaced by the final minutes; a keyword-based **local recap** when no AI is configured.
- **Full-text library search** (titles, transcripts, minutes, notes — Chinese included) with keyboard navigation.
- **Ask your meetings**: natural-language questions answered only from your meeting library, every claim cited with a clickable meeting + timestamp.
- One meeting processed at a time, resumed after a restart; crash-safe live transcript journal; one-click retry for failed meetings; Markdown / SRT / JSON export and MP3 / OGG / WAV audio export.

**Dictation**
- Realtime dictation on OpenAI Audio and Volcengine (opt-in) with a live preview in the capsule and dictionary hotwords; new cloud engines (Volcengine, MiniMax, next-gen OpenAI transcription).
- **Esc** cancels an active dictation; the paste is verified when the field exposes its value and the text stays on the clipboard with a notice when it does not land.
- The original clipboard — images and rich text included — is restored only if nobody else wrote to it.
- AI cleanup: output integrity guard, truncation detection with raw-text fallback, relevance-bounded dictionary injection, translation styles.
- Local model prewarm on record (first dictation after launch: SenseVoice ~0.8 s → ~0.02 s, Qwen3-ASR ~3.1 s → ~0.5 s on Apple Silicon), Bluetooth-headset fallback to the built-in mic, Silero VAD auto-stop.

**Privacy & data**
- macOS release builds keep provider API keys in the Keychain; existing plaintext keys are migrated on first launch (Windows keeps them in the config file).
- History export (Markdown / JSONL) and optional retention; optional retention window for sealed context snapshots (off by default).
- The [privacy policy](https://github.com/fakechris/lumen-asr/blob/main/PRIVACY.md) now covers meetings, voiceprints, calendar access and cloud meeting transcription.

### Upgrading from v0.3.x

- **macOS:** on first launch your API keys move from `config.toml` into the Keychain. Because builds are ad-hoc signed, macOS may ask for Keychain access after an upgrade; allow it. If the prompt is dismissed, the key is not lost — it is read again on the next launch.
- **Windows:** API keys stay in `config.toml` in your application-data directory, as before.
- Existing meeting recordings stay as they are; new recordings use Ogg-Opus.
- History and context retention are off by default; nothing is deleted by the upgrade.

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
instructions for Microphone and Accessibility permissions. Meetings that capture
the other side of a call ask for system audio recording when they start.

### Privacy and source provenance

Lumen ASR is local-first. Optional cloud speech recognition, correction, minutes
and meeting questions send data only to endpoints configured by the user. See the
[privacy policy](https://github.com/fakechris/lumen-asr/blob/main/PRIVACY.md)
and
[code signing policy](https://github.com/fakechris/lumen-asr/blob/main/CODE_SIGNING_POLICY.md).
