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

### M1 IPC (desktop)

| Command | Purpose |
|---------|---------|
| `list_sessions` / `get_session` / `delete_session` | History |
| `save_session` / `seed_demo_session` | Write sessions |
| `list_edit_events` / `record_edit_event` | Edit audit trail |
| `suggest_from_edit` / `confirm_learn` | Edit → dictionary candidates |
| `list_dictionary` / `add_dictionary_term` / `add_dictionary_replacement` / `delete_dictionary_entry` | Dictionary CRUD |

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
