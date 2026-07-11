# Voice Output Shaping — Design (Cleanup / Polish / Intent Hotkeys)

> Product design for making Lumen’s post-ASR text pipeline **adjustable**.  
> Core idea: **raw ASR is never discarded**; users control *how much* and *which way* text is shaped before insert.

---

## 0. Git status (this session’s work)

| Item | Status |
|------|--------|
| Feature commits on `main` | Yes — stepwise local commits |
| **Pushed to `origin`** | **No** — branch is **ahead by 7 commits** (as of this write-up) |

Recent local commits (not yet pushed):

1. `d3c1a3b` EventTap hotkey + focus cache  
2. `caeb249` Accessibility gate / onboarding prompt fix  
3. `4ff69a9` Onboarding design doc  
4. `23a1ebb` Onboarding Stage A+B  
5. `4a80038` Onboarding Stage C–E  
6. `a28025d` History play / retry / copy  
7. `26c25f7` History recovery-first UX polish  

Working tree may still have untracked `dist/` / lockfile noise — not part of those commits.

---

## 1. What we have today (Lumen)

```
ASR raw → rule preprocess (spaces/punct) → single system prompt (Light-ish) → insert
```

- One global corrector on/off + provider/model.  
- Fixed Chinese system prompt in `lumen-prompts` (red lines: never answer questions).  
- History keeps `asr_raw` + `corrected` (good base for “undo AI edit”).  
- **No** cleanup level, style, polish rules, secondary hotkeys, or per-intent prompts.

---

## 2. Patterns worth adopting (product, not UI clone)

Mature voice-dictation products usually separate **three layers**:

| Layer | User mental model | When it runs |
|-------|-------------------|--------------|
| **Cleanup** | “How much fix by default, every time” | Every dictation |
| **Polish / Style** | “How formal / tight / structured” | Default pipeline or opt-in |
| **Intent action** | “Do something *extra* this time” (translate, email, bullet list…) | Separate hotkey or post-action |

Also common:

- **Keep original forever** (Lumen already: `asr_raw`).  
- **Undo AI edit** → restore raw or previous corrected.  
- **Secondary chord** (free-mode / short vs long press / second hotkey) for a different post-process, not a second record path.  
- **Custom prompt** as *additive overlay* on a safe base system prompt, not a full replace that breaks red lines.  
- **Rule preprocess** independent of LLM (`text_normalization_*`) so “None” level still can strip spaces.

Reference architecture fields seen in production Tauri shells (for mapping only):

- `voice_input_organize_level` — default cleanup intensity  
- `enable_ai_correction` + press-hold / toggle switches  
- `use_custom_prompt` + `system_prompt` / `system_prompt_text` / `user_prompt`  
- `text_normalization_enabled` + rules  
- Dual hotkeys: `recording_hotkey` vs `freemode_hotkey`; unified short/long press → different actions  
- Correction can be disabled for empty ASR on retry  

We design Lumen’s own names and UX.

---

## 3. Lumen model: **Output Profile** + **Intent Chord**

### 3.1 Output Profile (global default)

Applies to the **primary** hold-to-talk hotkey every time.

```toml
[output]
# Cleanup intensity (always applies; LLM may be skipped at none)
cleanup = "light"          # none | light | medium | strong

# Written style after cleanup
style = "neutral"          # formal | neutral | casual | very_casual

# Capitals + punctuation policy (mainly EN; CN ignores case)
casing = "sentence"        # preserve | sentence | lower
punctuation = "standard"   # preserve | standard | light

# Optional polish rules (multi-select, default empty)
polish = []                # concise | clarity | reorder | structure | keep_tone

# Custom instruction layered ON TOP of built-in red-line base
custom_enabled = false
custom_instruction = ""    # user free text, short

# Target language for style/translate defaults (ISO or free text)
language = "auto"          # auto | zh | en | ja | ...
```

**Cleanup semantics** (examples aligned with industry None/Light/Medium; names are ours):

| Level | Behavior | Model call? |
|-------|----------|-------------|
| **none** | Preprocess only (or pure ASR). Keep mistakes, fillers. | No |
| **light** | Fix ASR errors, fillers, light grammar/punct. Preserve wording. | Yes (short prompt) |
| **medium** | Clarity + mild concision; merge false starts. | Yes |
| **strong** | Aggressive rewrite for readability (still no Q&A). | Yes |

**Style** (orthogonal to cleanup):

| Style | Caps (EN) | Punctuation |
|-------|-----------|-------------|
| formal | Sentence case | Standard full stops |
| neutral | Sentence case | Standard |
| casual | Sentence case | Slightly lighter |
| very_casual | lower | light / optional |

Style modifiers map into prompt clauses, not separate models.

**Polish rules** (multi-select, prompt bullets):

| Rule id | Prompt intent |
|---------|----------------|
| `concise` | Prefer shorter sentences; cut redundancy |
| `clarity` | Disambiguate pronouns / broken ASR |
| `reorder` | Fix word order for readability without inventing facts |
| `structure` | Light structure (lists when user listed items) |
| `keep_tone` | Preserve speaker attitude / slang when cleanup would sanitize it |

Conflict: `concise` + `keep_tone` → tone wins on slang; length still trims fillers.

### 3.2 Prompt assembly (critical)

Always:

```
BASE_SYSTEM (immutable red lines)
  + CLEANUP_CLAUSE(level)
  + STYLE_CLAUSE(style, casing, punctuation)
  + POLISH_CLAUSES(rules[])
  + CUSTOM_INSTRUCTION (if enabled, truncated, sanitized)
  + DICTIONARY_BLOCK
  + INTENT_CLAUSE (if intent hotkey; see §4)
```

Rules:

1. **Base red lines never optional** — no answering questions even if custom says “answer me”.  
2. Custom is **append-only** enhancement (“also format as bullet list”), not full system replace in v1.  
3. Advanced: “Expert mode” can override user message template only after explicit toggle.  
4. Temperature/max tokens may scale with cleanup (light → lower temp).

### 3.3 Undo AI edit

History / session:

- Always store `asr_raw`.  
- After corrector: `corrected` (and `pasted` if injected).  
- UI: **还原原文** → show/copy/re-insert `asr_raw`.  
- Optional stack later: raw → light → medium snapshots.

---

## 4. Intent hotkeys (second chord, not second product)

### 4.1 Problem

User wants: default is cleanup/polish; **another hotkey** means “this time also translate to English” (or email, bullets…).

### 4.2 Design

```toml
[hotkey]
# Existing primary
enabled = true
toggle = "Alt+Space"
mode = "hold"   # push-to-talk

# Secondary intents: list of named chords
[[hotkey.intents]]
id = "translate"
chord = "Alt+Shift+T"   # or modifier-only if we allow
mode = "hold"
intent = "translate"
# intent-specific options
target_language = "en"

[[hotkey.intents]]
id = "polish_strong"
chord = "Control+Shift+Space"
mode = "hold"
intent = "polish"
# temporarily override profile for this take
cleanup = "strong"
polish = ["concise", "clarity"]
```

**Intent catalog (v1):**

| `intent` | Effect on pipeline |
|----------|-------------------|
| `default` | Use global Output Profile only |
| `translate` | Cleanup (light min) + translate to `target_language`; keep meaning |
| `polish` | Force medium/strong + selected polish rules |
| `raw` | Force cleanup=none (bypass model) |
| `custom` | Apply named custom recipe (see §5) |

Flow for secondary chord = **same record → same ASR → different corrector profile**, not a second mic pipeline.

Primary chord: `intent = default`.  
Secondary: inject `INTENT_CLAUSE` e.g. “Output language: English. Translate if needed after cleanup.”

### 4.3 UX

Settings → **输出** tab:

1. Cleanup level (segmented None / Light / Medium / Strong) with live example strings (like the coffee/Joey demos).  
2. Style + casing + punctuation.  
3. Polish checkboxes.  
4. Custom instruction (textarea, char limit ~500).  
5. **快捷意图**：table of intent chords (add/edit/delete) + target language for translate.

History: show which intent/profile produced the corrected text (meta chip: `light · casual` / `translate→en`).

---

## 5. Custom “recipes” (optional v1.5)

Named presets beyond single custom string:

```toml
[[output.recipes]]
id = "email_zh"
label = "邮件口吻"
cleanup = "medium"
style = "formal"
polish = ["clarity", "structure"]
custom_instruction = "适合商务邮件正文，不要称呼和署名"

[[output.recipes]]
id = "en_meeting"
label = "英文会议纪要"
cleanup = "medium"
style = "formal"
intent = "translate"
target_language = "en"
polish = ["concise", "structure"]
```

Bind a recipe to an intent hotkey: `intent = "recipe:email_zh"`.

---

## 6. Architecture changes

### 6.1 Config

Extend `CorrectorConfig` → or split:

```
[output]          # profile
[corrector]       # provider/model (existing)
[hotkey]          # primary + intents[]
```

### 6.2 `lumen-prompts`

```rust
pub struct PromptBuildInput {
  pub cleanup: CleanupLevel,
  pub style: Style,
  pub casing: Casing,
  pub punctuation: PunctPolicy,
  pub polish: Vec<PolishRule>,
  pub custom: Option<String>,
  pub intent: IntentSpec, // Default | Translate { lang } | ...
  pub dictionary: Option<String>,
}
pub fn build_system_prompt(input: &PromptBuildInput) -> String;
pub fn build_user_message(asr: &str, dictionary: Option<&str>) -> String;
```

Unit-test: red lines present for every combination; translate clause only when intent says so.

### 6.3 `lumen-corrector`

- `CorrectRequest` gains `profile: OutputProfile` + `intent`.  
- `none` skips HTTP.  
- Preprocess stays independent; optional filler-regex pass at light without LLM (future).

### 6.4 Hotkey monitor

- Register N chords (EventTap already supports one; extend to multi-spec dispatch with `intent_id`).  
- On press: capture focus + **bind intent to session**.  
- Dictation stop uses session intent when calling corrector.

### 6.5 History

- Persist `output_profile_snapshot` + `intent` + keep `asr_raw`.  
- UI: 还原原文 / 复制 / 再识别（可选：用另一 profile 重跑）.

---

## 7. Settings UI sketch (Lumen voice)

```
设置 → 输出整理
┌─────────────────────────────────────────────┐
│ 自动整理强度                                 │
│ ( ) 无  (•) 轻  ( ) 中  ( ) 强               │
│  [示例预览区：同一句 ASR → 四级结果]          │
│                                             │
│ 语气  [正式 ▼]   大小写 [句首大写 ▼]          │
│ 标点  [标准 ▼]                               │
│                                             │
│ 额外整理  ☐更短  ☐更清楚  ☐理顺语序  ☐加结构 │
│                                             │
│ 自定义补充说明                               │
│ ┌─────────────────────────────────────────┐ │
│ │ 例如：保留英文专有名词大小写               │ │
│ └─────────────────────────────────────────┘ │
│                                             │
│ 意图快捷键                                   │
│  翻译 → Alt+Shift+T   目标语言 [en ▼]  [改] │
│  + 添加意图                                  │
└─────────────────────────────────────────────┘
```

Preview uses fixed sample ASR strings (CN + EN) so users see level differences without speaking.

---

## 8. Implementation stages

| Stage | Deliverable | Success |
|-------|-------------|---------|
| **P0** | `cleanup` none/light/medium + prompt builder + settings UI + history 还原原文 | Levels change output; raw always recoverable |
| **P1** | `style` + casing/punct + polish multi-select | Examples match table |
| **P2** | Custom instruction (append-only) | Custom cannot break red-line tests |
| **P3** | Multi intent hotkeys + translate target language | Second chord records + translates |
| **P4** | Named recipes + re-run from history with different profile | Power users |

---

## 9. Non-goals (v1)

- Full agent / chat on secondary hotkey (freemode chat is a different product surface).  
- Per-app cleanup profiles (useful later).  
- Cloud-only polish models.  
- Letting custom prompt replace red-line base without expert mode.

---

## 10. Relation to current corrector prompt

Keep existing Chinese red-line base as `BASE_SYSTEM`.  
Map today’s behavior ≈ **cleanup=light**, style=neutral, no polish, no custom.  
Migration: existing configs get those defaults; no behavior surprise.

---

## 11. Locked product decisions

| # | Decision | Choice |
|---|----------|--------|
| 1 | Default cleanup | **`medium`** |
| 2 | Translate pipeline | **Light cleanup first, then translate** to `target_language` |
| 3 | Secondary hotkeys | **Independent chords** (not short/long on the same key) |
| 4 | Ship / git | **Push every completed stage** to `origin` |

**Strong** level: include in P0 as optional fourth segment (default remains medium).

---

*When implementing, use Lumen product wording (整理强度 / 语气 / 意图快捷键 / 还原原文).*
