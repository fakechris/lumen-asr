# Lumen Launch Media System Design

**Date:** 2026-07-10

**Status:** Approved in conversation; awaiting written-spec review

**Scope:** Reproducible product screenshots, a usage tutorial, a Remotion launch video, and a bilingual static marketing site

## 1. Goal

Build one reproducible media pipeline for the completed Lumen ASR macOS app. A single isolated demo profile and a small set of deterministic scenarios must produce four deliverables:

1. Five real-product screenshots embedded in the bilingual README.
2. A 60–90 second silent usage tutorial in English and Simplified Chinese.
3. A 25–30 second Remotion launch video in English and Simplified Chinese, rendered at 16:9, 1:1, and 9:16.
4. A locally runnable, deploy-ready static marketing site with equal `/en/` and `/zh/` routes.

The pipeline must use the real Lumen app and real UI text. It must not reconstruct or fabricate product screens.

## 2. Current State and Preconditions

The product milestones M0–M6 are marked complete, the README is already bilingual, and the frontend production build succeeds. The repository does not yet contain product screenshots or media automation.

The release-readiness gate is currently red because `cargo test --workspace` fails in `crates/lumen-inject`:

```text
test tests::auto_type_first_succeeds ... FAILED
left: Paste
right: Type
```

Formal capture must not begin until this is diagnosed, the intended insertion policy is made explicit, all workspace tests pass, and a release build completes a real dictation smoke test into a separate macOS text field.

The current app data path is hard-coded under `~/Library/Application Support/LumenAsr`. Media automation therefore requires an environment override so it never reads or mutates the operator's real sessions, dictionary, configuration, recordings, or credentials.

## 3. Locked Product and Creative Decisions

- Primary story: hold the hotkey in a real input field, speak, let Lumen clean the transcript, release to insert, then briefly show translation, history, and dictionary learning.
- Audience: English and Simplified Chinese are equal launch audiences.
- Visual direction: native and restrained. Real macOS UI is dominant; system blue is primary, warm orange highlights the voice-to-text moment, and dark animated waveforms appear only in video transitions.
- Tutorial: 60–90 seconds, no narration and no music; retain restrained operation sounds and export separate English and Chinese caption versions.
- Launch video: 25–30 seconds, no narration; use real operation sounds plus original or procedural restrained sound design, with no externally licensed media.
- Site: static, locally runnable, and deploy-ready, but not deployed or uploaded in this scope.
- Automation: Codex Desktop orchestrates project tools and reviews artifacts. `deskagent` provides reproducible native macOS control and ScreenCaptureKit recording. WebdriverIO is not a first-stage dependency.

## 4. Architecture

```text
Release quality gate
        ↓
Isolated demo profile + deterministic seed
        ↓
Project-owned scenarios and capture manifest
        ↓
Codex orchestration + deskagent dry-run / assert / record
        ↓
Canonical screenshots + raw recording + provenance manifest
        ├── README image selection and compression
        ├── 60–90s tutorial exports
        ├── Remotion launch-video exports
        └── bilingual static marketing site
```

### 4.1 Codex Desktop

Codex reads the repository, runs builds and tests, launches the isolated app, invokes capture scripts, inspects images and rendered frames, fixes in-scope product or media code, and verifies the finished artifacts. Codex is the orchestrator and reviewer; it is not the source of nondeterministic mid-recording decisions.

### 4.2 Demo profile

Add `LUMEN_DATA_DIR` support to `lumen-platform::default_data_dir()`. Media commands launch Lumen with a repository-local ignored directory under `media/.work/profile/`. The profile contains only deterministic fixtures, an onboarding-complete config, safe provider labels with no API keys, sample history, dictionary entries, and a project-owned demo WAV with documented provenance.

Formal automation launches with `LUMEN_MEDIA_MODE=1` and `LUMEN_AUDIO_FIXTURE` pointing at that WAV. The normal hotkey and dictation state machine runs, but the capture adapter streams the fixture instead of reading the physical microphone. The audio still passes through the real local SenseVoice engine and the normal insertion path.

For repeatable cleanup, `scripts/media/prepare-profile.sh` starts a loopback-only OpenAI-compatible fixture server and configures the isolated profile to use it. That server returns the approved cleaned or translated result for the known demo input through Lumen's existing corrector HTTP integration. It binds only to `127.0.0.1`, requires no API key, and runs only in media mode. Static history and dictionary screenshots use deterministic database seed data. The manifest records both fixture modes so the capture is reproducible and never confused with a model-quality benchmark.

### 4.3 Scenario engine

Project-owned scenario JSON is the source of truth for window size, locale, initial state, actions, assertions, caption cues, capture checkpoints, and expected outputs. `deskagent` first explores selectors, then replays a screenplay without observe-then-decide behavior during recording.

WebdriverIO is introduced only if the same WebView target fails two complete dry-run attempts after selector refinement with stable accessibility properties or OCR text. If required, WDIO controls only that local UI segment; deskagent remains responsible for cross-app behavior, the floating capsule, system surfaces, and recording.

### 4.4 Capture manifest

Every formal run writes a machine-readable manifest containing:

- source commit and dirty-worktree state;
- scenario version and locale;
- app build identity;
- window and output dimensions;
- screenshot names, hashes, and capture checkpoints;
- video duration, frame rate, codec, and hashes;
- caption source hashes;
- results of privacy, visual, link, and accessibility reviews.

## 5. Proposed Repository Structure

```text
media/
  README.md
  brand/
    tokens.json
  fixtures/
    demo-profile.json
  scenarios/
    core-dictation.json
    readme-screenshots.json
  captions/
    tutorial.en.json
    tutorial.zh-CN.json
    launch.en.json
    launch.zh-CN.json
  screenshots/
    source/
    readme/
    website/
  recordings/
    raw/
    final/
  promo/
    remotion/
  manifests/
  .work/
scripts/media/
  preflight.sh
  prepare-profile.sh
  capture.sh
  verify-assets.sh
apps/website/
  src/
  public/
docs/images/product/
```

Generated raw recordings, temporary profiles, caches, and render intermediates are ignored. Final reviewed screenshots, captions, the Remotion source, the website source, and small web-ready media are versioned. Large final videos are kept out of Git unless the repository explicitly adopts Git LFS.

## 6. Deliverable Design

### 6.1 README screenshots

Capture five privacy-safe images at a fixed 1280×800 logical app window size on Retina display:

1. `hero-result`: full Lumen window with a successful cleaned transcript.
2. `dictation-in-context`: a real macOS input field with the Lumen listening capsule.
3. `history-detail`: corrected result, playback controls, and pipeline details.
4. `dictionary-learning`: safe terms and replacement-learning UI.
5. `local-first-settings`: local SenseVoice and local corrector readiness with no path, key, email, or account data.

The README places the hero image after each language introduction and the remaining images beside the relevant workflow and feature sections. The English and Chinese sections use the same product images; alt text is localized. Losslessly compressed PNG is the canonical README format, with each embedded image kept below 2 MB.

### 6.2 Usage tutorial

Target duration is 75 seconds at 1920×1080, 30 fps, H.264:

| Time | Scene |
|---|---|
| 0–4s | Show the finished inserted result first. |
| 4–12s | Show Lumen ready and the primary hotkey. |
| 12–35s | Focus a real input field, hold, speak, release, and insert. |
| 35–49s | Repeat with the translation intent. |
| 49–65s | Open History, then show dictionary learning. |
| 65–75s | Recap the three-step loop and show the repository CTA. |

English and Chinese exports share identical visual timing. Captions are authored separately, burned into their corresponding export, and also shipped as WebVTT and SRT. Captions must remain inside 16:9 and vertical safe areas and pass overflow checks.

### 6.3 Remotion launch video

The 28-second composition shows the real product from frame one:

| Time | Beat |
|---|---|
| 0–3s | Lumen window and product promise. |
| 3–8s | Speak: hotkey and listening capsule. |
| 8–14s | Polish: raw speech becomes clean text. |
| 14–18s | Paste: text lands in the target app. |
| 18–22s | Translate intent. |
| 22–26s | Local-first, history, and dictionary evidence. |
| 26–28s | App icon, tagline, and repository CTA. |

One Remotion source renders 1920×1080, 1080×1080, and 1080×1920 variants. Layouts use composition-aware safe zones rather than cropping the landscape output. English and Chinese text variants share timing. All UI shown inside the composition comes from real screenshots or recordings.

### 6.4 Bilingual marketing site

Create a standalone Vite + React static app under `apps/website`. It uses shared components and manually locked copy sources for `/en/` and `/zh/`; it does not translate at runtime.

Page order:

1. Hero with real UI, language switch, repository CTA, and tutorial CTA.
2. Three-step “Speak / Polish / Paste” explanation.
3. Large real-workflow media section.
4. Translation, history, and dictionary feature sections.
5. Local-first privacy and provider-choice evidence.
6. Build-from-source and repository CTA.

Use the macOS system font stack and the existing Lumen visual tokens. Do not add third-party analytics or remote fonts. Provide a poster and controls for video, keyboard navigation, localized alt text, and `prefers-reduced-motion` behavior. The page is validated at 375, 768, and 1440 CSS pixels.

## 7. Quality Gates and Failure Handling

### Gate 0: release readiness

- `npm run build` succeeds in `apps/desktop`.
- `cargo test --workspace` passes.
- Formal capture runs from a clean dedicated worktree; generated ignored media is the only permitted dirtiness.
- The signed release app starts with the isolated profile.
- A real dictation smoke test inserts correct text into a separate app.

### Gate 1: capture environment

- `deskagent doctor` reports Screen Recording and Accessibility ready.
- The app uses the isolated data directory.
- Window size, locale, theme, and initial screen match the scenario.
- Automated scans and visual review find no credentials or private user data.

### Gate 2: dry-run

- Every action has an accessibility or OCR assertion.
- Cross-app insertion and the floating capsule are visible.
- All capture checkpoints exist.
- The complete scenario passes twice consecutively before recording.

### Gate 3: artifact verification

- Dimensions, durations, frame rates, codecs, and hashes match the manifest.
- Images receive full visual inspection.
- Videos receive sampled-frame and caption-overflow inspection.
- README links and both website routes pass automated checks.
- The website scores at least 90 for Lighthouse Performance, Accessibility, and Best Practices in the local production build.

If a permission is missing, pause at Gate 1 and resume after the operator grants it. If a selector fails, re-inspect and prefer stable accessibility attributes or visible text over coordinates. If the same target fails two refined dry-runs, add local WDIO control for that target. If the real product behaves incorrectly, fix the product and restart Gate 0; do not conceal the problem in editing. If a derived render fails, retain the canonical recording and rerun only the derivation stage.

## 8. Definition of Done

- The README contains five verified real-product images in both language sections.
- The tutorial has English and Chinese 1080p exports between 60 and 90 seconds, plus WebVTT and SRT.
- The launch video has English and Chinese exports at 16:9, 1:1, and 9:16 between 25 and 30 seconds.
- The static site builds locally, serves `/en/` and `/zh/`, passes responsive and accessibility checks, and is not deployed.
- The media pipeline can rebuild all canonical captures from an isolated profile without reading real user data.
- The final manifest identifies the exact source, scenario, assets, and verification results.

## 9. Out of Scope

- Publishing or deploying the site.
- Uploading videos to social platforms.
- App Store packaging, notarization, or a public binary release.
- Voice-over narration.
- Interactive Supademo-style tutorials.
- A permanent WDIO test suite unless dry-run evidence triggers the fallback rule.

## 10. Implementation Phases

This design is one media system but should be implemented as independently verifiable phases:

1. Restore the product baseline and build the isolated capture foundation.
2. Capture and integrate README screenshots.
3. Record and export the bilingual tutorial.
4. Build and render the Remotion launch video.
5. Build and verify the bilingual static site.

Each phase consumes the artifacts and manifest from the preceding phase and can be reviewed before the next begins.

## 11. Primary Tool References

- [desktop-recorder-skill](https://github.com/MobAI-App/desktop-recorder-skill) — screenplay, dry-run, native recording, and edit/export workflow.
- [Tauri WebDriver documentation](https://v2.tauri.app/develop/tests/webdriver/) — optional embedded WebDriver fallback on macOS.
- [Remotion agent skills](https://github.com/remotion-dev/skills) — programmatic React video practices.
- [EveryInc product-launch-video](https://github.com/EveryInc/product-launch-video) — storyboard-first launch-video workflow and real-UI rule.
