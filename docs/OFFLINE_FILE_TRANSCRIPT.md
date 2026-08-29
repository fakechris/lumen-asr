# Offline file transcription (CLI)

Headless pipeline for **whole-file** audio: diarization → per-turn ASR → optional
LLM translation. Intended for agents, scripts, and MCP-style callers; does not
open the GUI or write into the user's meeting library (uses a throwaway SQLite
DB that is deleted on exit).

Binary: `lumen-asr-desktop` (release build or the installed `.app` executable).

```bash
# From a release build of this repo:
./target/release/lumen-asr-desktop meeting process <audio> [options]

# Installed app (path varies):
"/path/to/Lumen ASR.app/Contents/MacOS/lumen-asr-desktop" meeting process …
```

`stdout` = machine-readable result. `stderr` = progress / diagnostics.

---

## Architecture

```
audio (wav | m4a | mp3 | …)
  │  ffmpeg → 16 kHz mono PCM wav (when not already wav)
  ▼
diar-rs  (speaker turns, macOS + diarize feature)
  │  merge short fragments (default < 1.5 s) → fewer false speakers
  ▼
per-turn AsrEngine  (SenseVoice | Qwen3-ASR sherpa | mlx-whisper Metal | sherpa Whisper)
  │
  ├─ optional LLM corrector: --translate zh[,en,…]
  ▼
stdout: text | json | bilingual | lumen-transcript.v1
```

Same separation as lumen-cut / in-app meetings: **diar owns speaker identity and
time windows; ASR owns text**.

---

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--engine` | `sensevoice` | `sensevoice` \| `qwen` \| `mlx-whisper` \| `whisper` |
| `--lang` | (engine default) | Language hint, e.g. `Spanish`, `es`, `zh`, `auto` |
| `--format` | `text` | `text` \| `json` \| `transcript-v1` \| `bilingual` |
| `--translate` | (none) | Comma-separated target langs for LLM translation (e.g. `zh`) |
| `--minutes` | off | Generate structured minutes with the configured LLM and write `<audio>.minutes.md` next to the input. Does not write the GUI meeting library. Skipped (stderr) if no LLM is configured. |
| `--json` | | Alias for `--format json` |
| `--max-speakers` | `6` | Diar AHC speaker cap |
| `--min-turn-seconds` | `1.5` | Absorb shorter diar turns into a neighbour (`0` = off) |
| `--mlx-whisper-model` | `mlx-community/whisper-large-v3-turbo` | HF repo or local path |

### Engines

| Engine | Runtime | Best for | Notes |
|--------|---------|----------|--------|
| **sensevoice** | sherpa-onnx | CJK dictation | Official langs: zh / en / ja / ko / yue — **not** Spanish |
| **qwen** | sherpa-onnx (in-process) | Multi-lingual file ASR (fast) | Auto-detects language; uses the shared `qwen3-sherpa` model dir |
| **mlx-whisper** | mlx-whisper worker (Metal) | Multi-lingual quality Whisper | Production Whisper path; needs `mlx-whisper` in the Python env |
| **whisper** | sherpa-onnx CPU | Tiny/debug only | **Not** for large multi-lingual production |

### Formats

- **`text`** — one line per segment:  
  `[start–end] S1: …`
- **`json`** — store segment rows (or rows + `translations` if `--translate` set)
- **`transcript-v1`** — [`lumen-transcript.v1`](../contracts) interchange (Cut import). With `--translate`, each segment may include `translations.{lang}`
- **`bilingual`** — human blocks; implies `--translate zh` if no translate list given.
  Source line tag comes from `--lang` (e.g. `ES` / `ZH` / `EN`); missing/`auto` → `SRC`.

```text
[   4.5-14.6  ] S1:
  ES: yo creo que es algo diferencial también
  ZH: 我认为这也是一个与众不同的点。
```

---

## Short-turn merge (false speakers)

After diarization, turns shorter than `--min-turn-seconds` (default **1.5 s**) are
absorbed into a neighbour **only when the silence gap is ≤ 2.0 s**, then consecutive
same-speaker runs are collapsed. Distant short fragments are left alone so ASR is
not handed a silence-padded multi-second slice.

Implementation: `crates/lumen-meeting/src/turns.rs` (`merge_short_diar_turns`),
applied in `pipeline::diarize_wav`.

Typical effect on monologue + noise: `before=11 after=7` speaker fragments.

---

## Engine vs language

Default engine is **sensevoice** (zh/en/ja/ko/yue only). Passing `--lang es` (or any
non-official SenseVoice language) **errors** unless you pick a multi-lingual engine:

```bash
# Wrong (rejected): sensevoice + Spanish
meeting process talk.m4a --lang es

# Right:
meeting process talk.m4a --engine mlx-whisper --lang es
meeting process talk.m4a --engine qwen --lang es
```

---

## Translation

Requires a configured **LLM corrector** (`~/Library/Application Support/LumenAsr/config.toml` `[corrector]`, or Settings → AI cleanup).

Per-segment LLM failures omit that language from `translations` / bilingual lines — the
source text is never written as a fake translation (so Cut import of `transcript-v1`
stays honest). A **disabled or missing** corrector is different: the whole
`--translate` / bilingual run exits with status `1` and does not emit partial output.

```bash
# Spanish ASR + Chinese translation, human bilingual layout
lumen-asr-desktop meeting process talk.m4a \
  --engine mlx-whisper --lang es \
  --format bilingual --translate zh

# Structured Cut-ready JSON with translations.zh on each segment
lumen-asr-desktop meeting process talk.m4a \
  --engine qwen --lang es \
  --format transcript-v1 --translate zh
```

---

## Examples

```bash
# Multi-lingual file, sherpa Qwen3-ASR, printable speakers
./target/release/lumen-asr-desktop meeting process ./recording.m4a \
  --engine qwen --lang es --format text

# Production Whisper (Metal turbo)
./target/release/lumen-asr-desktop meeting process ./recording.m4a \
  --engine mlx-whisper --lang es --format transcript-v1

# ES + ZH bilingual dogfood
./target/release/lumen-asr-desktop meeting process ./recording.m4a \
  --engine mlx-whisper --lang es \
  --format bilingual --translate zh \
  --min-turn-seconds 1.5
```

---

## Models on disk

Shared root: `~/Library/Application Support/Lumen/models/`

| Path | Role |
|------|------|
| `diar/` | diar-rs seg + emb + plda |
| `sensevoice/` | sherpa SenseVoice |
| `qwen3-sherpa/` | Qwen3-ASR sherpa-onnx (`conv_frontend.onnx`, `encoder.int8.onnx`, `decoder.int8.onnx`, `tokenizer/`) |
| `whisper/` | sherpa Whisper ONNX (optional CPU path) |
| HF cache `mlx-community/whisper-large-v3-turbo` | mlx-whisper weights (auto-fetched) |

Python for mlx-whisper only: `[asr] runtime_path` in config, or env `LUMEN_QWEN_PYTHON`.
Install it once, e.g.:

```bash
uv pip install --python "$LUMEN_QWEN_PYTHON" mlx-whisper
```

---

## Optional helper script (experimental)

`scripts/offline_file_transcript.py` can reuse a diar turns JSON and run
mlx-whisper (or the legacy MLX Qwen stack, if still installed) outside the
desktop binary for local experiments. Supported production path:
`lumen-asr-desktop meeting process`.

---

## Related

- In-app meetings: same `lumen_meeting::transcribe_meeting` pipeline (live record → process)
- Interchange schema: lumen-suite `contracts/TRANSCRIPT.md` (`lumen-transcript.v1`)
- Diarization ADR: lumen-suite `docs/ADR-0001-diarization.md`
