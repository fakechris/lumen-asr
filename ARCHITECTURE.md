# Lumen ASR — Architecture

## 1. Principles

1. **Core has no Tauri / UI deps** — session logic is unit-testable.  
2. **Ports over providers** — ASR, Corrector, Inject, Store swap without rewriting the loop.  
3. **macOS first, platform trait for Windows** — no `#[cfg]` soup in `lumen-core`.  
4. **Fail soft** — corrector/insert errors degrade; never drop the user’s raw transcript without saving.  
5. **Local-first defaults** — models and SQLite on disk; network optional for corrector.

## 2. Crate map

```
                    ┌─────────────────┐
                    │  apps/desktop   │  Tauri commands + React
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
        lumen-core     lumen-store   lumen-platform
              │              ▲              │
    ┌─────────┼─────────┐    │       ┌──────┴──────┐
    ▼         ▼         ▼    │       ▼             ▼
lumen-asr lumen-corrector lumen-dictionary  platform-macos
    │         │              │              (windows stub later)
    └────┬────┴──────┬───────┘
         ▼           ▼
   lumen-inject  lumen-prompts
```

| Crate | Responsibility |
|-------|----------------|
| `lumen-core` | `Session`, state machine, orchestration traits usage |
| `lumen-asr` | `AsrEngine` trait; SenseVoice/Whisper adapters |
| `lumen-corrector` | Rules preprocess + `Corrector` trait (HTTP/Ollama) |
| `lumen-dictionary` | Entries, learn candidates from diffs |
| `lumen-store` | SQLite schema & repositories |
| `lumen-inject` | Ordered insert strategies (paste / ax / type) |
| `lumen-platform` | `Permissions`, `Hotkey`, `FrontmostApp` traits |
| `lumen-platform-macos` | macOS implementations |
| `lumen-prompts` | System prompts (corrector red lines) |

## 3. Session state machine

```
Idle
  → CheckingPermissions
  → Listening
  → Transcribing
  → Correcting
  → Review          # optional preview; may skip
  → Inserting
  → Verifying       # optional AX readback
  → CapturingEdits  # pre_insert + post_paste windows
  → Idle
  → Error(recoverable) → Idle
```

Events (examples): `HotkeyPressed`, `HotkeyReleased`, `AudioChunk`, `Stop`, `TranscriptReady`, `Corrected`, `UserEdited`, `InsertDone`, `Cancel`.

## 4. Insert pipeline

```
TextInjector::insert(text, InsertPolicy)

policy.mode:
  Auto  → try Paste (default first); on hard fail try Ax; then Type
  Paste → clipboard snapshot → write → simulate Cmd+V → restore
  Ax    → AX focused element insert/replace
  Type  → CGEvent unicode string
```

Clipboard restore delay: ~300–500ms (Wispr uses 500ms).  
Always log which strategy succeeded for history/`debug_info`.

## 5. Data model (SQLite)

```sql
sessions (
  id TEXT PK,
  created_at TEXT NOT NULL,
  focused_app TEXT,
  focused_bundle_id TEXT,
  asr_raw TEXT,
  corrected TEXT,
  pasted TEXT,
  asr_engine TEXT,
  corrector_engine TEXT,
  insert_strategy TEXT,
  audio_path TEXT,
  status TEXT
);

edit_events (
  id TEXT PK,
  session_id TEXT NOT NULL,
  source TEXT NOT NULL,  -- pre_insert_ui | post_paste_ax | manual
  before_text TEXT NOT NULL,
  after_text TEXT NOT NULL,
  created_at TEXT NOT NULL
);

dictionary_entries (
  id TEXT PK,
  kind TEXT NOT NULL,     -- term | replacement
  term TEXT,
  from_text TEXT,
  to_text TEXT,
  source TEXT NOT NULL,   -- manual | learned
  hit_count INTEGER NOT NULL DEFAULT 0,
  confirmed INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
```

## 6. Config (`config.toml`)

```toml
[asr]
engine = "sensevoice_sherpa"  # or "whisper"
model_dir = "models"

[corrector]
provider = "ollama"           # ollama | openai_compatible
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:7b"
api_key = ""

[inject]
mode = "auto"                 # auto | paste | ax | type
preserve_clipboard = true
paste_first = true

[hotkey]
# product default TBD; e.g. Right Command hold
toggle = false

[dictionary]
auto_promote = false
auto_promote_threshold = 3

[paths]
# default: ~/Library/Application Support/LumenAsr
```

## 7. Desktop IPC (sketch)

| Command | Purpose |
|---------|---------|
| `get_permission_status` | mic / accessibility |
| `open_system_settings` | deep link to Privacy panes |
| `start_dictation` / `stop_dictation` | session control |
| `list_sessions` / `get_session` | history |
| `list_dictionary` / `upsert_entry` / `delete_entry` | dict |
| `confirm_learn` | promote edit → dictionary |
| `get_config` / `save_config` | settings |

Events to UI: `session_state`, `partial_transcript`, `final_transcript`, `error`.

## 8. Testing strategy

| Layer | How |
|-------|-----|
| `lumen-core` | pure state machine tests |
| `lumen-dictionary` | diff → candidate tests |
| `lumen-store` | temp SQLite integration |
| `lumen-corrector` | mock HTTP |
| inject / platform | manual on macOS; feature-gated |

## 9. Dependency direction (enforced)

```
apps/desktop → core, store, platform-macos, asr, corrector, dictionary, inject, prompts
core         → (traits only via generic bounds; avoid heavy deps)
asr / corrector / dictionary / inject / store → minimal shared types
platform-macos → platform traits + inject helpers
```

No crate depends on `apps/desktop`.

## 10. Windows (future)

Implement `lumen-platform-windows`:

- Permissions: mic; UIAccess / accessibility analog as needed  
- Inject: `SendInput` paste + clipboard restore  
- Hotkey: low-level hook  

Same `lumen-core` session code.
