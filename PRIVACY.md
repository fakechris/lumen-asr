# Privacy policy

Effective date: November 24, 2026

Lumen ASR is a local-first desktop dictation and meeting-notes application. The Lumen ASR
project does not operate an account service, analytics service, advertising
service, telemetry endpoint, or crash-reporting endpoint.

## Data processed on the device

Depending on the features you use, Lumen ASR processes:

- microphone audio recorded while dictation is active;
- for meetings: microphone audio and, when you choose to capture the other
  side of a call, system audio — which includes the voices of other
  participants — together with transcripts, speaker labels, minutes, notes and
  a live-transcript journal kept while the meeting is recorded;
- speaker voiceprints: numeric voice embeddings created when you enroll or name
  a speaker (including enrolling yourself from dictation recordings), used to
  recognize the same speaker in later meetings;
- on macOS, when calendar access is granted, the title and attendees of the
  current calendar event, read-only, used to name a meeting and recorded in its
  notes (notes are part of what a configured corrector receives for minutes);
- transcripts, corrected text, insertion results, personal dictionary entries,
  configuration, and pipeline diagnostics;
- on macOS, frontmost-application metadata and bounded editor, browser, or
  visible-text context used for optional context-aware correction;
- API endpoints, model names, and credentials that you configure for optional
  recognition or correction providers.

Dictation history, meeting recordings and transcripts, voiceprints, models,
configuration, and diagnostics are stored in local application-data
directories. On macOS, provider API keys are kept in the
login Keychain rather than in the configuration file — in official release
builds. Local development builds are ad-hoc signed with an identity that
changes on every rebuild, which the Keychain cannot authorize without a
password prompt each time, so they keep API keys in the 0600 configuration
file instead (`LUMEN_KEYCHAIN=1` opts a dev build into the Keychain).
Conversely, an official build run with `LUMEN_KEYCHAIN=0` also keeps keys
in the file. A key present in the configuration file never overwrites an
existing Keychain entry — the Keychain credential wins on load. On
other platforms keys remain in the configuration file in the user's
application-data directory.
macOS context snapshots are sealed before they are retained. Windows context capture is currently disabled and uses a
fail-closed adapter.

Lumen ASR does not automatically upload locally stored history, meeting
recordings or transcripts, voiceprints, raw context snapshots, dictionary data,
or diagnostics to the project maintainers.

## Network transfers

Lumen ASR makes network requests only for features that the user selects or
configures:

- **Model installation:** when the user requests installation of a local model,
  the model package is downloaded from the configured upstream model host.
- **Cloud speech recognition:** when a user selects a cloud ASR provider
  (OpenAI-compatible, Volcengine or MiniMax), the recorded dictation audio and
  request metadata are sent to the endpoint configured by the user; in the
  opt-in realtime mode the audio is streamed while you speak. If the cloud
  request fails, dictation falls back to the local engine.
- **Cloud correction or translation:** when a user selects a networked
  OpenAI-compatible corrector, transcript text and correction instructions are
  sent to the configured endpoint.
- **Cloud meeting transcription (opt-in):** when enabled, the meeting's
  microphone and system-audio tracks are streamed or uploaded to the cloud
  speech provider you configured. It is off by default; local meeting
  transcription does not send audio anywhere.
- **Meeting minutes and questions:** when a corrector is configured, the
  meeting transcript (or windows of it) and your notes are sent to that
  corrector to write minutes and draft minutes. When you ask a question in
  the meeting library, the question, plus titles, dates, minutes and
  transcript excerpts from up to five matching meetings, are sent to the same
  corrector. Voiceprints and meeting audio are never sent to a corrector.
- **Optional captured context:** a bounded, source-labelled context projection
  is added to a corrector request only when the user enables
  `use_captured_context`. The full sealed context snapshot is not sent.

Local endpoints such as Ollama or LM Studio may be used instead of Internet
services. Data sent to a third-party provider is governed by that provider's
privacy policy and retention terms. Lumen ASR does not control those providers.

## Permissions

- **Microphone** is used to record dictation and meetings only while recording
  is active. Meeting detection observes whether a meeting app is using audio
  input in order to show a prompt; it never starts or stops a recording by
  itself.
- **System audio recording on macOS** is requested only when a meeting that
  captures the other side of a call starts.
- **Calendar on macOS** (optional) is read-only and used to name meetings and
  list attendees.
- **Accessibility on macOS** is used for focused-control discovery, context
  capture, and inserting text into another application. Without it, Lumen ASR
  can fall back to the clipboard.
- **Clipboard and input APIs** are used to deliver the resulting text. Windows
  inserts via simulated keyboard / paste and falls back to the clipboard when
  the target refuses input. Windows does not perform OS context capture.

## Retention and deletion

Local data remains on the device until the user removes it. Dictation history
can be exported and given an optional retention period, and sealed context
snapshots can be given an optional retention window; both are off by default.
Deleting a meeting removes its database records, transcript journal and the
recording files Lumen ASR created. Voiceprints can be renamed, merged or
removed in the voiceprint manager. Deleting a history
entry removes its database record, but related files may remain in the
application-data directory in some versions. Uninstalling the application may
also leave application data and downloaded models behind. Users who want a
complete removal should close Lumen ASR and delete its application-data and
shared model directories.

Third-party providers may retain requests according to their own terms. Review
the selected provider's policy before enabling a cloud endpoint.

## Recording other people

Recording other people can be subject to consent laws where you or they are.
You are responsible for obtaining any consent required before recording a
meeting.

## Security and contact

Please do not publish private audio, transcripts, API credentials, or context
captures in a public issue. Report security or privacy concerns through the
repository's
[private security advisory form](https://github.com/fakechris/lumen-asr/security/advisories/new).

Material changes to this policy will be committed to the public repository.
