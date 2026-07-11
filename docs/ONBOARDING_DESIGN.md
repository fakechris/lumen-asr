# First-run Onboarding Design

> Product design for Lumen ASR first-launch wizard.  
> **Does not ship competitor names or reverse-engineering notes in UI copy.**

## 0. Why permissions feel broken today

### 0.1 Two “Lumen ASR” rows in Accessibility

macOS TCC keys trust by **code signature identity**, not by product name.

| How you run | Binary path | Codesign | Accessibility list name |
|-------------|-------------|----------|-------------------------|
| `cargo` / `tauri dev` | `target/debug/lumen-asr-desktop` | adhoc, ID `lumen_asr_desktop-<hash1>` | often **lumen-asr-desktop** or Lumen ASR |
| Release `.app` / DMG | `…/Lumen ASR.app/Contents/MacOS/lumen-asr-desktop` | adhoc, ID `lumen_asr_desktop-<hash2>` | **Lumen ASR** |

Different paths ⇒ different CDHashes ⇒ **two separate toggles**. Enabling the `.app` does nothing for `tauri dev`, and vice versa.

**Rule for onboarding:** always show the **currently running** executable basename + full path, **settings list name** (usually “Lumen ASR” for `.app`), codesign kind, and which entry to enable. After rebuild (adhoc signature changes), macOS may require: remove stale rows → re-add current → **fully quit & relaunch**.

**Detection is not a guess:** UI uses `AXIsProcessTrusted()`. If the user enabled two “Lumen ASR” rows but status stays off, they almost always enabled **stale identities** (old CDHash), not the process that is running now. Adhoc `tauri build` without Developer ID makes this the default developer pain.

### 0.2 Why there is no system “request” dialog

Accessibility is **not** like Microphone:

| | Microphone | Accessibility |
|--|------------|-----------------|
| API | TCC via first `AVAudio` / cpal open | `AXIsProcessTrusted` / `AXIsProcessTrustedWithOptions` |
| In-app grant? | Yes (system dialog) | **No** — user must flip a switch in System Settings |
| Prompt dialog | First use | Unreliable on modern macOS; often **never** after deny / for adhoc binaries |
| Reliable UX | Open settings + poll until granted | Open settings + poll until granted |

Calling `AXIsProcessTrustedWithOptions(prompt:true)` may:

- show nothing (already denied / adhoc / Sequoia),
- add the app to the list in **off** state,
- return `false` while UI shows `denied`.

**Product rule:** never rely on the system prompt. Wizard step = **explain → open Settings → poll every 1s → green check when trusted → continue**.

### 0.3 `denied` in our settings UI

Our status maps “not trusted” → `denied`. That is correct for inject, but confusing for first run. Onboarding should show:

- `需要开启` (not trusted)
- `已开启` (trusted)
- process path to enable

---

## 1. Goals

After first-run wizard completes, the user has:

1. Understood what Lumen does (local STT + insert at cursor).
2. **Mic** granted (or blocked with clear recovery).
3. **Accessibility** trusted for the **running** binary (or chose “稍后 / 仅剪贴板”).
4. Picked an input device and verified live level (“say a word”).
5. Local **SenseVoice** ready (download or pick existing folder).
6. Corrector configured (Ollama / OpenAI-compat / skip).
7. A non-conflicting **hold hotkey**, verified with a full press → speak → result path.

Persist: `onboarding.completed = true` in config. Incomplete wizard re-opens next launch (skippable with “稍后再说”, but status bar keeps a badge).

**Dismiss anytime:** every step has close (×) + 「稍后再说」; Esc and click on scrim also call `skip_onboarding`. Overlay is **dark scrim + backdrop blur** with a **solid `--card` modal** (not dark glass on dark text — that was inverted/unreadable in light mode).

---

## 2. Architecture (Tauri 2)

Same stack as the rest of the app: **React wizard UI** + **Rust commands/events**.

```
┌─────────────────────────────────────────────────────────┐
│  Main window  (React)                                    │
│  OnboardingWizard  (full-screen overlay if incomplete)   │
│    steps 0…6  ·  progress dots  ·  skip / back / next   │
└───────────────────────────┬─────────────────────────────┘
                            │ invoke / listen
┌───────────────────────────▼─────────────────────────────┐
│  Rust (lumen-asr-desktop)                                │
│  onboard.rs  — state machine + IPC                       │
│  permissions · audio level · model · corrector · hotkey  │
└─────────────────────────────────────────────────────────────────┘
```

### 2.1 Config additions

```toml
[onboarding]
completed = false
skipped = false
version = 1          # bump to re-show critical steps after product changes
completed_at = ""

[asr]
engine = "sensevoice"
sensevoice_dir = ""  # empty → auto-resolve
whisper_dir = ""

[audio]
device_name = ""     # empty → system default
```

### 2.2 IPC surface (new / extended)

| Command / event | Purpose |
|-----------------|--------|
| `get_onboarding_state` | completed, current step, blockers |
| `set_onboarding_step` / `complete_onboarding` / `skip_onboarding` | progress |
| `get_permission_status` | mic + ax + **process_path** + process_hint |
| `request_microphone_access` | open stream → system mic dialog |
| `request_accessibility_access` | open Settings only (no fake grant) |
| `poll_permissions` | lightweight status for 1s UI poll |
| `list_audio_devices` | existing |
| `start_volume_monitoring` / `stop_volume_monitoring` | live RMS/peak |
| event `volume-level` `{ rms, peak, device }` | UI meter |
| `check_asr_model_status` | sensevoice/whisper ready + paths |
| `list_local_asr_models` | scan known caches + user path |
| `start_asr_model_download` / `cancel_…` / event `asr-download-progress` | SenseVoice package |
| `use_existing_asr_model` | set path after validation |
| `probe_corrector` | ollama up? list models; env OpenAI-compat |
| `ollama_list_models` / `ollama_pull_model` + progress event | corrector setup |
| `save_corrector_config` | existing |
| `validate_hotkey` | parse + soft conflict notes |
| `start_e2e_practice` / events | press-hold sandbox (no inject or inject to notepad field) |

### 2.3 Backend readiness event

On setup completion, emit once:

```text
backend-ready  { accessibility, microphone, asr_ready, corrector_ready }
```

Frontend starts the wizard only after `backend-ready` (or timeout fallback).

---

## 3. Wizard steps (UX)

Full-screen card, one primary CTA, secondary “稍后 / 跳过此步” where safe.

### Step 0 — Welcome

**Copy (zh):**  
「Lumen 在本地把语音转成文字，并插入到你正在输入的应用。」

- 3 bullets: 本地转写 · 按住说话 · 插入光标处  
- Primary: **开始设置**  
- Secondary: **稍后再说** (sets `skipped=true`, badge remains)

### Step 1 — Permissions (Mic + Accessibility)

Two cards, each with status pill + action.

**Mic**

1. Button **请求麦克风** → open capture briefly → system dialog.  
2. Poll until `granted`.  
3. If denied: **打开麦克风设置** + instructions.

**Accessibility** (critical for inject + event-tap hotkey)

1. Show running identity:

   ```
   请在「系统设置 → 隐私与安全性 → 辅助功能」中开启：
   名称：lumen-asr-desktop
   路径：/Users/…/target/debug/lumen-asr-desktop
   ```

2. Button **打开辅助功能设置** (never promise a system popup).  
3. Poll `is_accessibility_trusted()` every 1s; when true → green + enable Next.  
4. Optional **仅使用剪贴板模式** (continue without AX; inject becomes copy-only).

**Do not** call silent `AXIsProcessTrustedWithOptions` at cold start without UI — it confuses users and often does nothing. Wizard owns the request moment.

### Step 2 — Microphone device + “say a word”

1. `list_audio_devices` → dropdown (default system default).  
2. On select: `set_audio_device` + `start_volume_monitoring`.  
3. Live meter from `volume-level` events.  
4. Prompt: **请说一句话** — require peak above threshold for ~0.5s → “已检测到声音”.  
5. Stop monitoring on leave.

Implementation sketch (Rust): dedicated cpal input stream (or reuse audio thread) that only computes RMS/peak and emits events; does not run ASR.

### Step 3 — Local ASR model (default SenseVoice)

1. `check_asr_model_status`:

   - ready under app data dir  
   - ready under known caches (`~/.coli/models/…`, env `LUMEN_SENSEVOICE_DIR`)  
   - missing  

2. UI options:

   | Option | Behavior |
   |--------|----------|
   | **使用已检测到的模型** | list + select path |
   | **选择本地文件夹…** | folder picker; validate `model*.onnx` + `tokens.txt` |
   | **下载 SenseVoice (推荐)** | start download; progress bar |

3. Download:

   - Store under `~/Library/Application Support/LumenAsr/models/sensevoice/`  
   - Progress event; cancelable  
   - On success: optional `warmup_model` (load once, log latency)

4. Whisper is optional advanced path; not required for first-run Next.

### Step 4 — AI corrector

Detect in parallel:

```
A. Ollama  — GET http://127.0.0.1:11434/api/tags
B. Env     — OPENAI_API_BASE / OPENAI_BASE_URL / OPENAI_API_KEY
             LUMEN_CORRECTOR_* 
C. None
```

**Branches:**

1. **Ollama running**  
   - list models  
   - prefer `qwen2.5:7b` if present  
   - else pick first, or **拉取 qwen2.5:7b** (`ollama pull` subprocess or HTTP) with progress  

2. **Ollama not installed / not running**  
   - explain install (`brew install ollama` + link)  
   - or switch to OpenAI-compatible  

3. **Env OpenAI-compatible detected**  
   - show base URL (mask key)  
   - “使用该配置？” → save provider=`openai_compatible`  

4. **Skip corrector**  
   - `enabled=false` / provider=`none` — rule-only preprocess  

Probe button: send short “你好” correct request; show latency / error.

### Step 5 — Hotkey

1. Show current default (`Alt+Space` or config).  
2. Soft conflict checks (best-effort, not OS-global):

   - Spotlight-like: `Cmd+Space`  
   - Common IME / dictation chords  
   - Empty / single bare key  

3. **点击录制** → existing `HotkeyRecorder` + pause global hooks while capturing.  
4. Mode fixed to **hold (按住说话)** for first-run (toggle advanced later).  
5. Save + `reregister` hotkeys; if event-tap fails, show AX reminder.

### Step 6 — E2E practice

Sandbox in-wizard (does **not** require external app if AX missing):

```
┌──────────────────────────────────────┐
│  在下方输入框练习（或切到备忘录）      │
│  ┌────────────────────────────────┐  │
│  │  [textarea focus target]       │  │
│  └────────────────────────────────┘  │
│  按住  ⌥Space  说话，松手等待结果     │
│  状态：等待热键 → 录音中 → 转写中 → 完成 │
└──────────────────────────────────────┘
```

Flow:

1. Focus practice textarea (or user focuses Notes/iTerm if AX ok).  
2. User press-hold hotkey → same dictation pipeline.  
3. Prefer insert into practice field if focus is our window; else normal inject.  
4. Success criteria: non-empty transcript shown; if AX granted and external target, insert worked.  
5. **完成设置** → `onboarding.completed=true`.

---

## 4. Reference: how a production Tauri dictation shell structures this

(Patterns observed in mature macOS Tauri voice apps — for engineering alignment only.)

### 4.1 Module map

| Area | Typical files / commands |
|------|---------------------------|
| Keyboard | `keyboard_macos_configurable` · CGEventTap · `init_keyboard_monitor` / `restart_keyboard_monitor` · pause while recording shortcut |
| AX gate | Check before create EventTap; log “please add app to Accessibility”; `open_accessibility_settings` → `Privacy_Accessibility` URL |
| Mouse (optional) | Separate EventTap, same AX gate |
| Audio | Dedicated `audio_thread` · `list_audio_devices` · `start_volume_monitoring` / `stop_volume_monitoring` · device rebind |
| Model | `check_model_status` · `start_model_download` · `get_model_download_state` · `recheck_model_files` · `warmup_model` · progress event |
| UI notify | Overlay events: `show-microphone-permission-required-notification`, `show-permission-required-notification`, `show-asr-not-configured-notification` |
| Hotkey UX | `start_hotkey_recording` / `stop_hotkey_recording` · conflict auto-correct on startup · `validate_hotkey_config` |
| Config flag | `demo_hint_dismissed` / first-run UI state · `silent_start` for autostart without window |
| Corrector | Multi-provider map: ollama / openai_compatible / cloud vendors · separate endpoints per provider |
| ASR package | Hosted SenseVoice package URL + local dir under app models |

### 4.2 Startup sequence (target for us)

```
app setup
  → load config
  → emit backend-ready
  → if !onboarding.completed → show wizard (do not start global hotkey until step 5+ or soft-start with degraded mode)
  → else init_keyboard_monitor (requires AX)
  → if asr ready → warmup async
```

### 4.3 Permission philosophy to copy

- **Mic:** real system prompt via device open.  
- **AX:** open Settings + continuous poll; UI owns education.  
- **Missing AX:** hotkey monitor fails closed with in-app banner; inject → clipboard.  
- **Notifications:** dedicated overlay channels, not only log lines.

---

## 5. Lumen implementation plan (staged)

### Stage A — Permission UX fix (unblock insert) ✅

- [x] `PermissionDto` path + trusted  
- [x] No cold-start Settings open  
- [x] Settings + wizard poll  
- [x] Process path copy  

### Stage B — Wizard shell + steps 0–2 ✅

- [x] `OnboardingWizard.tsx` overlay  
- [x] config `onboarding`  
- [x] Welcome + permissions + volume monitoring  

### Stage C — ASR model step ✅

- [x] `check_asr_model_status` / `list_local_asr_models` / `use_existing_asr_model`  
- [x] SenseVoice download (curl + tar extract, sherpa int8 package)  
- [x] path paste + validate  

### Stage D — Corrector step ✅

- [x] Ollama probe + list + pull  
- [x] env OpenAI-compat detection  
- [x] skip path  

### Stage E — Hotkey + E2E ✅

- [x] `validate_hotkey` soft conflicts  
- [x] HotkeyRecorder in wizard  
- [x] practice + start/stop + hotkey dictation  

### Stage F — Polish

- [x] re-run wizard from Settings / sidebar badge  
- [ ] `backend-ready` event  
- [ ] folder picker dialog (currently path paste)

---

## 6. Event contracts (draft)

### `volume-level`

```json
{ "rms": 0.02, "peak": 0.15, "device": "MacBook Pro Microphone" }
```

### `asr-download-progress`

```json
{ "engine": "sensevoice", "bytes": 12000000, "total": 80000000, "phase": "downloading" }
```

### `onboarding-blocker`

```json
{ "step": "accessibility", "message": "辅助功能未开启", "processPath": "…" }
```

---

## 7. Acceptance checklist

| # | Check |
|---|--------|
| 1 | Fresh config → wizard shows before main IA |
| 2 | Mic dialog appears on step 1 action |
| 3 | AX step opens System Settings; enabling correct binary turns green without guessing |
| 4 | Two Accessibility rows explained in UI (path shown) |
| 5 | Say-a-word meter moves; threshold unlocks Next |
| 6 | Existing SenseVoice cache selectable without re-download |
| 7 | Download works offline-fail with error |
| 8 | Ollama list/pull or OpenAI-compat from env or skip |
| 9 | Hotkey recorder works; bad chords rejected with reason |
| 10 | E2E hold → text in practice field (and external app if AX on) |
| 11 | `onboarding.completed` persists; no wizard on next launch |
| 12 | Incomplete → badge; Settings can re-open wizard |

---

## 8. Out of scope (v1 wizard)

- Cloud membership / login  
- Mouse-button triggers  
- Multi-provider cloud ASR signup  
- Auto-update of models  
- Input Monitoring separate pane (only if EventTap requires it on a specific OS — then add as AX sub-note)

---

## 9. Immediate user recovery (today, before wizard ships)

1. Quit all Lumen processes.  
2. System Settings → Privacy & Security → **Accessibility**.  
3. Enable **both** Lumen-related rows if unsure; prefer the path matching how you start the app.  
4. If still denied: toggle **off → on**, or remove + re-add the debug binary.  
5. Relaunch **the same** binary you enabled.  
6. Settings → 权限 should show accessibility granted; then retest hold hotkey in iTerm.

Developer tip: for stable AX during development, always run the **`.app` bundle** (`open "target/release/bundle/macos/Lumen ASR.app"`) and enable that one entry only.
