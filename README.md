# Lumen ASR

Local-first macOS voice dictation — a productized alternative to Wispr Flow / Typeless / 闪电说.

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
# Workspace compile (library crates)
cargo check --workspace

# Desktop app (once frontend deps installed)
cd apps/desktop
npm install
npm run tauri dev
```

Data directory (runtime):

```
~/Library/Application Support/LumenAsr/
  config.toml
  lumen.sqlite
  models/
  recordings/   # optional
```

## Relation to demo

The reverse-engineered `shandianshuo` / LumenAsr demo is **reference only** (prompts, competitor notes). This repo is the product codebase.

## License

Private / TBD.
