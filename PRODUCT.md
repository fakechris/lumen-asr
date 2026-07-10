# Lumen ASR — Product Spec (MVP)

> Locked product decisions for Lumen ASR.  
> Date: 2026-07-09

## 1. One-liner

Hold a hotkey, speak, get polished text into the focused app — **local-first**, privacy-friendly, with a personal dictionary that learns from your edits.

## 2. Non-goals (MVP)

- Agent / chat / Total Recall screenshots
- Billing / membership
- Team dictionary sync
- Windows delivery (architecture only)
- Auto-silent dictionary writes without user control

## 3. Core user loop

```
Hotkey down → record → ASR → normalize → model correct (dict injected)
  → optional in-app review/edit → insert into focused app
  → save session + capture edits → optional "add to dictionary"
```

## 4. Platform & permissions (macOS)

| Permission | Required for | Without it |
|------------|--------------|------------|
| **Microphone** | Recording | Hard block + onboarding |
| **Accessibility** | Paste/AX insert, reliable observation | **Copy-only mode** (app UI + clipboard copy, no silent inject) |
| Screen Recording | — | Not in MVP |
| Automation | Optional launch-at-login later | Skip |

Onboarding order: Mic → Accessibility → try dictation playground.

## 5. ASR

| | Default | Alternative |
|--|---------|-------------|
| Engine | SenseVoice via **sherpa-onnx** | Whisper (same `AsrEngine` port) |
| Sample rate | 16 kHz mono | Resample from device |
| Offline | Yes | — |

Cloud ASR is **out of MVP** (port may exist later).

## 6. Corrector

| Layer | Role |
|-------|------|
| Rules preprocess | Extra spaces, Chinese punct, optional filler strip |
| **Model corrector** | Primary path — Ollama or any OpenAI-compatible HTTP API |
| Fallback | On error / empty / filter → return preprocess output (never hard-fail the session) |

Corrector prompt: voice-input **text organizer**, not a chat assistant (strict red-line rules). Dictionary terms + replacements injected into prompt.

Modes (later): Raw / Light / Structured — MVP ships **Light** only.

## 7. Text insertion

### Our policy

```
injection_mode = auto  (product default behavior)

1. Unicode CGEvent type          ← primary (insert at current key focus)
2. Clipboard paste + restore     ← fallback
3. AX insert_at_cursor           ← optional when editable AX value works
```

Config knobs:

- `preserve_clipboard: true` (default)
- `injection_mode: auto | paste | ax | type`

## 8. Dictionary & edit learning

### Entry kinds

| kind | Fields | Use |
|------|--------|-----|
| `term` | `term` | Hotword / prompt bias (e.g. Morpho, GPT-4) |
| `replacement` | `from`, `to` | Deterministic or prompt correction (脱肯→Token) |

### Capture sources

1. **pre_insert_ui** — user edits preview before insert  
2. **post_paste_ax** — after paste, poll focused field (~60s, if AX available)  
3. **manual** — user adds from settings / history

### Promote policy (MVP)

- Always **persist** `edit_events` (before/after)
- Suggest dictionary candidates from phrase-level diff
- **Default: user must confirm** “Add to dictionary”
- Optional setting: auto-promote after same replacement confirmed **N≥3** times (off by default)

### What we learn / don’t

| Learn | Don’t learn |
|-------|-------------|
| Stable proper nouns, brands, jargon | Whole-paragraph rewrites |
| Stable homophone fixes | One-off tone changes |
| User-confirmed pairs | Unattested guesses |

### Re-injection

- Terms → ASR hotwords (when engine supports)  
- Terms + replacements → corrector prompt  
- High-confidence replacements → optional pre-model deterministic replace

## 9. UI surfaces (MVP)

1. **Onboarding** — permissions + first dictation  
2. **Settings** — ASR engine, corrector endpoint/model, hotkey, injection mode, dictionary  
3. **History** — sessions list; open edit/diff; add dict entry  
4. **Dictionary** — CRUD terms/replacements  
5. **Floating capsule** — recording state (non-activating if possible)

## 10. Data location

```
~/Library/Application Support/LumenAsr/
  config.toml
  lumen.sqlite
  models/          # SenseVoice / Whisper assets
  recordings/      # optional debug
```

## 11. Success criteria (MVP)

- [ ] Mic + Accessibility onboarding works on clean macOS account  
- [ ] Push-to-talk or toggle hotkey records → local SenseVoice transcript  
- [ ] Model corrector (Ollama) improves punctuation / light cleanup  
- [ ] Text lands in Notes / browser input via paste-first without destroying clipboard permanently  
- [ ] User edit → confirm → dictionary entry affects next corrector run  
- [ ] `cargo test --workspace` green; session state machine unit-tested  

## 12. Milestone sketch

| Stage | Deliverable |
|-------|-------------|
| M0 | This repo scaffold, docs, compiling workspace |
| M1 | Session SM + store + dictionary CRUD (CLI/tests) |
| M2 | macOS mic capture + sherpa SenseVoice offline path |
| M3 | Corrector (Ollama) + prompts + dict inject |
| M4 | Paste-first inject + clipboard restore + permission UI |
| M5 | Desktop shell: hotkey, capsule, history, settings |
| M6 | Edit learning UX + optional post-paste AX capture |
