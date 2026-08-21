# Privacy policy

Effective date: July 28, 2026

Lumen ASR is a local-first desktop dictation application. The Lumen ASR
project does not operate an account service, analytics service, advertising
service, telemetry endpoint, or crash-reporting endpoint.

## Data processed on the device

Depending on the features you use, Lumen ASR processes:

- microphone audio recorded while dictation is active;
- transcripts, corrected text, insertion results, personal dictionary entries,
  configuration, and pipeline diagnostics;
- on macOS, frontmost-application metadata and bounded editor, browser, or
  visible-text context used for optional context-aware correction;
- API endpoints, model names, and credentials that you configure for optional
  recognition or correction providers.

Dictation history, audio, models, configuration, and diagnostics are stored in
local application-data directories. macOS context snapshots are sealed before
they are retained. Windows context capture is currently disabled and uses a
fail-closed adapter.

Lumen ASR does not automatically upload locally stored history, raw context
snapshots, dictionary data, or diagnostics to the project maintainers.

## Network transfers

Lumen ASR makes network requests only for features that the user selects or
configures:

- **Model installation:** when the user requests installation of a local model,
  the model package is downloaded from the configured upstream model host.
- **Cloud speech recognition:** when a user selects an OpenAI-compatible cloud
  ASR provider, the recorded dictation audio and request metadata are sent to
  the endpoint configured by the user.
- **Cloud correction or translation:** when a user selects a networked
  OpenAI-compatible corrector, transcript text and correction instructions are
  sent to the configured endpoint.
- **Optional captured context:** a bounded, source-labelled context projection
  is added to a corrector request only when the user enables
  `use_captured_context`. The full sealed context snapshot is not sent.

Local endpoints such as Ollama or LM Studio may be used instead of Internet
services. Data sent to a third-party provider is governed by that provider's
privacy policy and retention terms. Lumen ASR does not control those providers.

## Permissions

- **Microphone** is used to record dictation only while recording is active.
- **Accessibility on macOS** is used for focused-control discovery, context
  capture, and inserting text into another application. Without it, Lumen ASR
  can fall back to the clipboard.
- **Clipboard and input APIs** are used to deliver the resulting text. Windows
  inserts via simulated keyboard / paste and falls back to the clipboard when
  the target refuses input. Windows does not perform OS context capture.

## Retention and deletion

Local data remains on the device until the user removes it. Deleting a history
entry removes its database record, but related files may remain in the
application-data directory in some versions. Uninstalling the application may
also leave application data and downloaded models behind. Users who want a
complete removal should close Lumen ASR and delete its application-data and
shared model directories.

Third-party providers may retain requests according to their own terms. Review
the selected provider's policy before enabling a cloud endpoint.

## Security and contact

Please do not publish private audio, transcripts, API credentials, or context
captures in a public issue. Report security or privacy concerns through the
repository's
[private security advisory form](https://github.com/fakechris/lumen-asr/security/advisories/new).

Material changes to this policy will be committed to the public repository.
