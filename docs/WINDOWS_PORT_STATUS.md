# Windows port status

Updated: 2026-07-27

This document tracks deliberate compatibility choices made by the
`port/windows` branch.

## Compatibility policy

- macOS remains a supported build target.
- Windows-specific code must be target-gated.
- If a temporary Windows change degrades macOS behavior, the change must be
  listed below with its reason, user impact, and removal condition.

At this point, no intentional macOS behavior break has been introduced.

## Implemented

- The Windows model root is `%LOCALAPPDATA%\Lumen\models`; the former
  `%USERPROFILE%\.lumen\models` location remains a read-only legacy discovery
  root.
- Modifier-only hold chords use `GetAsyncKeyState` on Windows instead of the
  previous non-macOS implementation that always returned zero.
- Application data uses `%LOCALAPPDATA%\LumenAsr`.
- Windows copy-only output now writes UTF-16 text through the Win32 clipboard
  API, including retrying briefly when another process owns the clipboard.
- The microphone settings action opens `ms-settings:privacy-microphone`.
- A Windows Tauri overlay selects an NSIS current-user installer and the
  WebView2 download bootstrapper.
- macOS Context is target-gated; Windows uses a fail-closed disabled adapter
  until UI Automation, Windows Graphics Capture and secure storage are added.
- First-run Windows config migrates the collision-prone `Alt+Space` default to
  `Ctrl+Shift+Space`.
- On Windows, the Apple MLX Qwen3-ASR runtime and Qwen shadow mode are disabled.
  The resource recommendation reports the reason and selects SenseVoice.
- SenseVoice downloads stream and extract in-process through `lumen-models`;
  packaged apps no longer require system `curl` or `tar`.
- Windows CI definitions now exercise the shared crates, frontend, and desktop
  backend on GitHub-hosted `windows-latest` runners.
- Windows CI creates an unsigned MSIX for Microsoft Store ingestion. The
  manifest declares only `runFullTrust` (required for the Tauri Win32 process)
  and `microphone`; Windows context capture and automatic insertion remain
  disabled.
- Store submission builds read the Partner Center identity from the
  `WINDOWS_STORE_IDENTITY_NAME`, `WINDOWS_STORE_PUBLISHER`, and
  `WINDOWS_STORE_PUBLISHER_DISPLAY_NAME` repository variables. Pull requests
  use a development identity so MakeAppx validation does not depend on private
  Partner Center configuration.

## Known Windows limitations

- The desktop backend still contains direct calls to `lumen-platform-macos`.
  Several have non-macOS stubs, but they do not provide Windows foreground
  target capture, text injection, or permission diagnostics.
- Automatic insertion is not yet supported. Windows deliberately returns a
  `copy_only` outcome instead of calling the macOS injector.
- Context capture is not yet implemented with UI Automation, Windows Graphics
  Capture, DPAPI, or Named Pipes.
- The installer is unsigned, so SmartScreen warnings are expected.
- The CI-produced MSIX is also unsigned. Microsoft signs it after Store
  certification; direct sideload distribution still requires a trusted
  signature.
- Qwen local inference uses the Apple MLX worker and is intentionally not
  offered as a Windows local engine. On Windows it falls back to SenseVoice;
  a future CUDA/DirectML/ONNX backend can replace this restriction.
- `diar-rs` has an unconditional Unix-style rpath linker argument and is not
  part of the first Windows desktop milestone.

## Verification pending

The following commands must be run on Windows after explicit approval to
execute this repository's third-party Cargo build scripts and tests:

1. `cargo test -p lumen-models`
2. `cargo test -p lumen-asr-engine --no-default-features`
3. `cargo test -p lumen-asr-engine`
4. `npm test && npm run build` in `apps/desktop`
5. `cargo check -p lumen-asr-desktop`
6. `npm run tauri -- build --bundles nsis`

Failures from these checks should be recorded here until resolved.

The current local workstation cannot execute Cargo-generated build scripts:
Windows Application Control rejects them with OS error 4551. This is an
environment restriction rather than a Rust compiler diagnostic, so GitHub
Windows CI is the initial authoritative build environment.

Verification on this workstation:

- `npm test`: 3/3 tests passed.
- `npm run build`: TypeScript and Vite production build passed.
- `npm run tauri -- build --bundles nsis`: passed.
- Release application: started and showed a responsive `Lumen ASR` window.
- NSIS current-user install: exit code 0; installed application started from
  `%LOCALAPPDATA%\Lumen ASR`.
- SenseVoice onboarding backend: downloaded 163,002,883 bytes, extracted to
  `%LOCALAPPDATA%\Lumen\models\sensevoice`, and passed the model readiness
  check.

The release Rust test target compiles on this workstation, but its generated
test executable currently exits with `STATUS_ENTRYPOINT_NOT_FOUND` while
loading the native sherpa runtime. The packaged release executable does not
have this failure and was verified by the install-and-launch test above.

`npm ci` reported one moderate and two high dependency advisories. They are
recorded but were not automatically rewritten with `npm audit fix`, because
that would be a dependency change outside the first porting patch.
