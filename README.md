# Lumen ASR

Local-first macOS voice dictation.

**Status:** greenfield scaffold (product architecture + crate boundaries).  
**Default stack:** Tauri 2 · Rust workspace · React · SenseVoice (sherpa) · model corrector · paste-first inject.

## Product decisions (locked)

| Area | Choice |
|------|--------|
| Platform | macOS first; Windows via `lumen-platform` traits later |
| ASR | SenseVoice via sherpa-onnx (default); Whisper supported |
| Corrector | **Model required** (Ollama / OpenAI-compatible); rules only as preprocess |
| Insert | paste-first + clipboard restore; AX when available; unicode type fallback |
| Learning | term / replacement dictionary; store edits; **user-confirm** promote (optional auto after N) |

See [PRODUCT.md](./PRODUCT.md) and [ARCHITECTURE.md](./ARCHITECTURE.md).

## Repo layout

```
apps/desktop/          # Tauri + React shell
crates/
  lumen-core/          # Session state machine (no Tauri)
  lumen-asr/           # ASR ports + engines
  lumen-corrector/     # Model corrector + rule preprocess
  lumen-dictionary/    # Dictionary + learn-from-edit
  lumen-store/         # SQLite persistence
  lumen-inject/        # TextInjector orchestration
  lumen-platform/      # Cross-platform traits
  lumen-platform-macos/# macOS mic / AX / paste / hotkey
  lumen-prompts/       # Corrector system prompts
docs/                  # Extra design notes
```

## Quick start

```bash
# Library crates + unit tests
cargo test --workspace --exclude lumen-asr-desktop

# Desktop app
cd apps/desktop
npm install
npm run tauri dev
```

### Desktop IPC (M1–M2)

| Command | Purpose |
|---------|---------|
| `list_sessions` / `get_session` / `delete_session` | History |
| `save_session` / `seed_demo_session` | Write sessions |
| `list_edit_events` / `record_edit_event` | Edit audit trail |
| `suggest_from_edit` / `confirm_learn` | Edit → dictionary candidates |
| `list_dictionary` / `add_*` / `delete_dictionary_entry` | Dictionary CRUD |
| `list_audio_devices` / `set_audio_device` | Mic selection |
| `set_asr_engine` / `get_asr_status` | SenseVoice / Whisper |
| `start_recording` / `stop_and_transcribe` / `cancel_recording` | Capture + local ASR + correct |
| `get_corrector_config` / `save_corrector_config` / `correct_text` | Model corrector settings |

### Models

Default SenseVoice dir resolution (first match wins):

1. `LUMEN_SENSEVOICE_DIR`
2. `~/Library/Application Support/LumenAsr/models/sensevoice`
3. Common local sherpa packages under `~/.coli/models/...`

Expected files: `model.int8.onnx` (or `model.onnx`) + `tokens.txt`.

Whisper: `LUMEN_WHISPER_DIR` or `.../models/whisper` with encoder/decoder/tokens onnx.

### Corrector (M3)

Default: Ollama at `http://127.0.0.1:11434/v1`, model `qwen2.5:7b` (override with `LUMEN_CORRECTOR_MODEL`).

Config: `~/Library/Application Support/LumenAsr/config.toml`

```toml
[corrector]
enabled = true
provider = "ollama"          # ollama | openai_compatible | none
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:7b"
api_key = ""
timeout_secs = 60
```

On failure: rule preprocess + dictionary replacements only (session still completes).

### Inject + permissions (M4)

- **Paste-first**: write clipboard → simulate ⌘V (CGEvent) → restore clipboard (~450ms)
- Requires **Accessibility** for insert; without it use copy-only / UI text
- **Microphone** usage string in `Info.plist`; first record triggers system prompt
- Settings: auto-insert, preserve clipboard, mode (`auto` / `paste` / `type` / `copy_only`)
- IPC: `get_permission_status`, `insert_text`, `get/save_inject_config`

### Hotkey + capsule (M5)

- Default **push-to-talk**: hold to speak, release to stop + paste into the focused field
- Optional **toggle** mode in Settings
- Default chord **`Alt+Space`** (⌥Space) — avoids Spotlight `⌘Space`
- **Settings → 点击录制**: press a new chord (supports modifier-only like ⌥⇧)
- Floating capsule is non-focusable (must not steal the typing target)
- History is saved in Lumen; insert goes to the frontmost app you were in
- Config:

```toml
[hotkey]
enabled = true
toggle = "Alt+Space"
show_capsule = true
mode = "hold"   # or "toggle"
```

### Edit learning (M6)

- Phrase-level diff: common prefix/suffix → short middle `from→to` candidates
- **Pre-insert**: edit transcript in Record tab → blur / 「从编辑生成候选」→ confirm into dictionary
- **Post-paste** (optional): after insert, poll focused field via Accessibility ~N seconds; emit `learn-suggestion`
- **Auto-promote** (optional, default off): same replacement reaches threshold N → confirmed

```toml
[learning]
auto_promote = false
auto_promote_threshold = 3
post_paste_capture = true
post_paste_seconds = 20
```

Data directory (runtime):

```
~/Library/Application Support/LumenAsr/
  config.toml
  lumen.sqlite
  models/
  recordings/   # optional
```

## License

Private / TBD.
