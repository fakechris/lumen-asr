# Lumen ASR

<p align="center">
  <img src="docs/images/app-icon.png" alt="Lumen ASR" width="96" height="96" />
</p>

<p align="center">
  <a href="https://github.com/fakechris/lumen-asr/releases/latest"><img src="https://img.shields.io/github/v/release/fakechris/lumen-asr?include_prereleases&label=release" alt="Latest release" /></a>
  <a href="https://github.com/fakechris/lumen-asr/actions/workflows/ci-macos.yml"><img src="https://github.com/fakechris/lumen-asr/actions/workflows/ci-macos.yml/badge.svg" alt="CI macOS" /></a>
  <a href="https://github.com/fakechris/lumen-asr/actions/workflows/ci-windows.yml"><img src="https://github.com/fakechris/lumen-asr/actions/workflows/ci-windows.yml/badge.svg" alt="CI Windows" /></a>
  <img src="https://img.shields.io/badge/platform-macOS%2012%2B%20%7C%20Windows%2010%2F11-blue" alt="Platform: macOS 12+ | Windows 10/11" />
  <img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="License: GPL-3.0" />
</p>

<p align="center">
  <strong>Speak. Polish. Paste.</strong><br />
  Local-first voice dictation and meeting notes for macOS and Windows — hold a hotkey, talk, get clean text in the app you’re already using; record a meeting, get a speaker-attributed transcript and minutes you can search and ask questions of.
</p>

<p align="center">
  <strong>Latest release: <a href="https://github.com/fakechris/lumen-asr/releases/tag/v0.4.0">v0.4.0</a></strong> (2026-11-24) · <a href="./docs/RELEASE_NOTES.md">What’s new</a> · <a href="https://github.com/fakechris/lumen-asr/releases">All releases</a>
</p>

<p align="center">
  <a href="#english">English</a> · <a href="#中文">中文</a>
</p>

---

<a id="english"></a>

## English

### Quick start

1. **Install** — download the latest Apple Silicon DMG or Windows x64 installer from [GitHub Releases](https://github.com/fakechris/lumen-asr/releases) (verify with `SHA256SUMS.txt`); to build from source on macOS instead, run `./dev-install.sh --open`
2. **Grant permissions** — microphone, plus Accessibility on macOS (needed to paste into other apps)
3. **Hold the hotkey, speak, release** — default **Alt+Space** on macOS, **Ctrl+Shift+Space** on Windows. Text is inserted into the focused field; if a target refuses input, the text stays on the clipboard and Lumen tells you
4. **Meetings** — open the Meetings tab and start a recording (or import a file); when it ends you get a transcript with speakers, plus structured minutes when an AI corrector is configured (a local recap otherwise)

Details and engine choices below.

### What’s new in v0.4.0

- **Meetings** — dual-track recording (mic + system audio), speaker diarization with a voiceprint library, structured minutes from templates, **draft minutes within seconds** of stopping, a **local recap** when no AI is configured, crash-safe live transcript journal, serial processing queue with resume after restart
- **Ask your meetings** — full-text search over titles, transcripts, minutes and notes (Chinese included), and natural-language questions answered from your meeting library with clickable citations to the exact moment
- **Realtime dictation** (opt-in, OpenAI / Volcengine) with a live preview in the capsule and hotwords from your dictionary
- **More reliable insertion** — Esc cancels a dictation, Lumen verifies the paste actually landed, and your original clipboard (images and rich text included) is restored only when nobody else wrote to it
- **Safer AI cleanup** — output integrity guard, truncation detection and relevance-bounded dictionary injection; API keys move to the macOS Keychain in release builds

Full notes: [docs/RELEASE_NOTES.md](./docs/RELEASE_NOTES.md).

### Why Lumen

Most “voice typing” stops at raw speech-to-text. Lumen is built for **writing into real work**:

1. **Hold** your dictation hotkey and speak
2. **Release** — local speech recognition turns audio into text
3. Optional **AI cleanup** fixes punctuation, fillers, and structure — it organizes what you said and never answers or acts on it
4. Text is **pasted into the focused field** (editor, chat, browser, IDE, terminal…)
5. Sessions stay in **History**; replay audio, edit, export, and grow a personal dictionary

And for calls and interviews, **Meetings** records both sides, separates speakers, writes minutes, and keeps a searchable library you can ask questions of.

**Local by default.** Recognition runs on your Mac with SenseVoice, Qwen3-ASR, or Whisper; cleanup can stay on-device (e.g. Ollama). When quality matters more than privacy, switch ASR to a cloud provider or the corrector to a cloud OpenAI-compatible API.

| You want… | Use |
|-----------|-----|
| Privacy + lightweight offline ASR | Local SenseVoice (default) |
| Higher-accuracy local ASR | Local Qwen3-ASR |
| Another fully local ASR path | Local Whisper |
| Cloud transcription | OpenAI Audio, Volcengine (豆包), MiniMax, or a custom OpenAI-compatible endpoint |
| Words appearing while you speak | Realtime mode on OpenAI Audio or Volcengine (opt-in) |
| Light cleanup, no cloud | Local LLM (Ollama / LM Studio) |
| Stronger rewrite / translation | Cloud corrector (e.g. MiniMax-M3, other OpenAI-compatible APIs) |
| Just the raw transcript | Cleanup level **None** / raw intent |

### Features

**Dictation**

- **Push-to-talk** (default) or toggle; **Esc** cancels an active dictation (Esc is only captured while recording or processing)
- **Intent hotkeys** — default cleanup vs **translate** (Alt+Shift+T) with formal / casual / social / custom styles
- **Floating capsule** that never steals focus; live preview in realtime mode
- **Cleanup strength** — none / light / medium / strong, with an output integrity guard and truncation fallback that insert your raw words instead of a broken or off-task rewrite
- **Personal dictionary** — terms and replacements, relevance-ranked into the cleanup prompt and used as realtime hotwords; learn from attributed post-insert edits (always human-confirmed)
- **Context-aware cleanup** (opt-in) — bounded text near the cursor; the full captured context stays encrypted locally
- **Reliable insertion** — paste, verified when the field exposes its value, retried once, otherwise left on the clipboard with a notice; the original clipboard (all types) is restored only if unchanged
- **Built-in mic over Bluetooth** — when the default input is a Bluetooth headset and you haven’t picked a device, Lumen records from the built-in mic so AirPods stay in high-quality mode
- **Silence auto-stop** (RMS or Silero VAD), sound cues, optional output ducking, model prewarm on record
- **Terminal pane observation** — follow edits in Herdr, tmux, and Zellij instead of depending only on Accessibility text

**Meetings**

- **Recording** — mic + system audio as separate tracks (per-app capture where available), echo suppression, Ogg-Opus by default; silence auto-stop (15 min), max-duration cap (8 h), low-battery / sleep warnings, idle-sleep prevention
- **Detection** — prompts when a call starts in a meeting app and suggests stopping when it ends; it never records or stops on its own
- **Speakers** — offline diarization, live speaker labels, a global voiceprint library (enroll, rename, merge), and annotation spread to unlabeled segments
- **Minutes** — templates (built-in + your own `minutes-templates/*.md`), long meetings summarized in windows; **draft minutes** from the live transcript right after you stop; a keyword-based **local recap** when no AI is configured
- **Library** — full-text search (⌘F, ↑/↓, Enter, Esc), **问会议 / ask your meetings** with cited answers, retry for failed meetings, export to Markdown minutes / Markdown transcript / SRT / JSON and audio export (MP3 / OGG / WAV)
- **Robust processing** — one meeting processed at a time, resumed after restart; crash-safe live transcript journal; per-stage timing in the log
- **File import** — choose or drop wav/mp3/m4a/mp4/opus/ogg; same pipeline as a live recording
- **Cloud meeting transcription** (opt-in) — stream both tracks to a realtime provider

**Privacy & data**

- API keys in the **macOS Keychain** for official release builds (dev builds keep them in a 0600 config file)
- **History** export (Markdown / JSONL) and an optional retention policy; optional retention window for sealed context snapshots
- **Auditable pipeline records** — immutable attempts, context provenance, and edit-observation outcomes
- See [PRIVACY.md](./PRIVACY.md) for exactly what stays local and what is sent where

**Tools**

- **Offline file CLI** — diarize + per-turn ASR on whole recordings; see [docs/OFFLINE_FILE_TRANSCRIPT.md](./docs/OFFLINE_FILE_TRANSCRIPT.md)
- **ASR quality regression harness** — corpus + normalized CER/WER gate; see [docs/EVAL.md](./docs/EVAL.md)

### Offline file transcription (CLI)

Headless mode on the desktop binary (no GUI, no meeting library writes):

```bash
./target/release/lumen-asr-desktop meeting process ./recording.m4a \
  --engine mlx-whisper --lang es \
  --format bilingual --translate zh
```

Engines: `sensevoice` (CJK dictation), `qwen` (sherpa-onnx multi-lingual, auto-detects language), `mlx-whisper` (Metal Whisper turbo), `whisper` (sherpa CPU only). Full flags and model layout: [docs/OFFLINE_FILE_TRANSCRIPT.md](./docs/OFFLINE_FILE_TRANSCRIPT.md).

### Requirements

- macOS 12+ on Apple Silicon, or Windows 10/11 x64
- **Microphone** — to record
- **Accessibility on macOS** — to paste into other apps (without it, Lumen
  copies to the clipboard)
- **System audio recording on macOS** — only for meetings that capture the other side of a call; requested when such a meeting starts
- **Calendar on macOS** (optional) — names meetings and adds attendees from the current event, read-only
- **Windows insertion** — Unicode typing and Ctrl+V paste into the focused
  window. Copy-only is the fallback when the target refuses input (for
  example an elevated app). Application context capture is not available yet
- Optional: [Ollama](https://ollama.com) or any OpenAI-compatible API key for AI cleanup and minutes

### Install & run

Each tagged release publishes an ad-hoc signed DMG for Apple Silicon, a Windows
x64 NSIS installer, and `SHA256SUMS.txt` on
[GitHub Releases](https://github.com/fakechris/lumen-asr/releases). Intel Mac and
Windows x86 installers are not provided. macOS requires a one-time manual
approval under **System Settings → Privacy & Security**. See
[docs/MACOS_GITHUB_RELEASE.md](./docs/MACOS_GITHUB_RELEASE.md) for verification
and installation, and [docs/MACOS_RELEASE_NOTES.md](./docs/MACOS_RELEASE_NOTES.md)
for first-launch notes.

The project is applying to SignPath Foundation for trusted direct Windows
downloads. **Free code signing provided by
[SignPath.io](https://signpath.io/), certificate by
[SignPath Foundation](https://signpath.org/).** Until acceptance and workflow
integration, Windows assets are clearly identified as unsigned.

Windows developers can create a free self-signed MSIX for local testing. This
requires explicitly trusting the local development certificate and does not
replace Store or SignPath signing. See
[docs/WINDOWS_LOCAL_SIGNING.md](./docs/WINDOWS_LOCAL_SIGNING.md).

To build locally instead:

```bash
git clone https://github.com/fakechris/lumen-asr.git
cd lumen-asr

# Build release app, install into .app, codesign, open
./dev-install.sh --open
```

Dev loop (hot reload UI):

```bash
cd apps/desktop
npm install
npm run tauri dev
```

> **Signing tip:** after every release build, reinstall with `./dev-install.sh` so the `.app` signature stays valid. Prefer a trusted local Code Signing certificate or free Apple Development identity so Mic / Accessibility grants survive rebuilds. Details: [docs/MACOS_LOCAL_SIGNING.md](./docs/MACOS_LOCAL_SIGNING.md).

### First-time setup (step by step)

#### 1. Launch Lumen ASR

Open `Lumen ASR.app` (from the DMG, `./dev-install.sh --open`, or your install path).
A downloaded release is blocked on first launch: open it once, then allow it under **System Settings → Privacy & Security → Open Anyway**.

#### 2. Complete onboarding

The wizard is four steps (about a minute). The dictation model downloads in the background.

| Step | What to do | Why |
|------|------------|-----|
| Welcome | Continue | What Lumen does |
| Permissions | Allow microphone. On macOS, open Accessibility and **drag the floating Lumen icon into the list**, then flip the switch | Recording + paste into other apps |
| Hotkey | Keep the default, or change it | Hold to speak |
| Ready | Optional try-dictation, then start | First success |

You can re-open onboarding later from Settings if permissions were skipped.

#### 3. Grant permissions in System Settings

**System Settings → Privacy & Security**

- **Microphone** → enable **Lumen ASR**
- **Accessibility** → enable **Lumen ASR**

Quit Lumen fully and reopen after toggling Accessibility.

#### 4. Choose a speech recognition engine

The default remains **SenseVoice** via sherpa-onnx. Lumen keeps a separate model path for each local engine, so switching engines does not overwrite the previous selection.

| Engine | Best for | What it needs |
|--------|----------|---------------|
| **SenseVoice** | Lower resource use, fast local dictation | `model.int8.onnx` (or `model.onnx`) + `tokens.txt` |
| **Qwen3-ASR** | Higher local accuracy; auto-detects the spoken language | sherpa-onnx model dir (`conv_frontend.onnx`, `encoder.int8.onnx`, `decoder.int8.onnx`, `tokenizer/`) — downloadable in Settings |
| **Whisper** | Alternative local ONNX pipeline | Encoder ONNX + decoder ONNX + tokens text file |
| **OpenAI Audio** | Cloud transcription; realtime mode available | Compatible endpoint, model, and API key |
| **Volcengine (豆包)** | Cloud transcription for Chinese; realtime mode available | API key (new console), or App ID + access token |
| **MiniMax** | Cloud transcription; also usable for cloud meeting transcription | API key |
| **Custom** | Any OpenAI-compatible transcription endpoint | Endpoint, model, API key |

If a cloud request times out or cannot connect and a local SenseVoice or Whisper model is installed, dictation falls back to it; other cloud errors are reported as errors.

Shared local model root:

1. `LUMEN_MODELS_DIR` (environment override)
2. `~/Library/Application Support/Lumen/models/`

Engine-specific overrides such as `LUMEN_SENSEVOICE_DIR` and `LUMEN_WHISPER_DIR` take priority. Lumen also discovers ready models in older Lumen/Navi and coli locations without moving them. Downloaded models are verified against pinned SHA-256 hashes.

In the app, open **Settings → Speech recognition** to select an engine, choose a discovered model, or validate a custom directory.

Qwen3-ASR runs in-process via sherpa-onnx (no Python runtime); Settings → Speech recognition offers a one-click download of the pinned model package.

For English dictation, the same engine can load the optional [Oruk Fourier 2B](https://huggingface.co/oruk/oruk-fourier-2b) INT8 pack. The 2.41 GB download is separate from the app and licensed by Oruk under [CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/). With the [Hugging Face CLI](https://huggingface.co/docs/huggingface_hub/guides/cli) installed:

```sh
hf download oruk/oruk-fourier-2b --revision 3fda023e157ce7986f137f76fcf326ed6e08fa73 \
    --include 'onnx/sherpa-int8/*' --local-dir "$HOME/Models/oruk-fourier-2b"
```

In **Settings → Speech recognition**, select the local Qwen provider. Paste the absolute path to `onnx/sherpa-int8` inside the downloaded directory and click **验证并使用** (Verify and use). Keep both `.onnx.data` files next to their graphs. The original multilingual pack remains selectable.

#### 5. Choose AI cleanup (recommended path)

**Principle:** local first; go online when quality matters more than privacy.

| Mode | Settings | When |
|------|----------|------|
| **Local** | Corrector → Ollama, model you pulled (e.g. `qwen3.5:9b`) | Everyday notes, private text |
| **Cloud** | Corrector → MiniMax / OpenAI-compatible, API key + model (prefer **MiniMax-M3** for cleanup; thinking is turned off automatically) | Better rewrite & translation latency/quality |
| **Off** | Cleanup **None**, or corrector disabled | Raw ASR only |

The same corrector writes meeting minutes and answers 问会议 questions.

Config file (advanced):
`~/Library/Application Support/LumenAsr/config.toml`

> **Context privacy:** Lumen can keep a bounded, encrypted local snapshot for audit and provenance. Cursor-near text is included in a corrector request only when **Use current app and nearby text** is enabled. With a cloud corrector, that bounded projection leaves the Mac; the full captured snapshot does not.

#### 6. Learn the hotkeys

| Action | Default (customize in Settings) |
|--------|----------------------------------|
| Dictation (hold) | **Alt+Space** on macOS (⌥Space), **Ctrl+Shift+Space** on Windows |
| Translate intent (hold) | **Alt+Shift+T** |
| Cancel the current dictation | **Esc** (only while recording or processing) |

**Flow**

1. Click into any text field (Notes, Slack, browser, IDE…).
2. **Hold** the dictation hotkey — capsule appears.
3. Speak.
4. **Release** — Lumen recognizes, cleans up, pastes.
5. Open **History** to replay audio, copy text again, or export.

#### 7. Tune output quality

- **Cleanup level** — Medium is a good default (clear, light cleanup).
- **Dictionary** — add product names / jargon so ASR + cleanup keep them.
- **History → edit** — improve a line and save learn candidates when offered.

### Meetings

1. Open the **Meetings** tab → **Start meeting** (or accept the prompt when a call starts), or **Import file**.
2. While recording, the live transcript shows speaker labels; you can rename the title and speakers on the fly.
3. When you stop, **draft minutes** from the live transcript appear within seconds if a corrector is configured; the offline pipeline then produces the authoritative transcript (diarization, per-turn ASR, cleanup) and, with a corrector configured, the final minutes, replacing the draft.
4. In the library, search everything with ⌘F, or switch to **问会议** and ask e.g. *“What did we decide about the budget last Tuesday?”* — every claim cites a meeting and time; click to jump there.

Minutes need a configured corrector. Without one you still get the transcript and a keyword-based local recap labelled as such. Recording other participants may require their consent where you live.

### Terminal pane edit learning

After inserting dictated text, Lumen can observe a correction through the terminal tool’s pane API. It pins the outer terminal surface and the inner client/session/pane identity, then attributes only a unique edit to the inserted line. This avoids screen selection tricks and survives unrelated TUI redraws.

| Environment | Read path | Support |
|-------------|-----------|---------|
| **Herdr** | Locked Herdr pane ID + unwrapped recent render buffer | Direct pane observation |
| **tmux** | Locked client/session/pane + `capture-pane -J` | Direct pane observation |
| **Zellij** | Locked client/pane + targeted `dump-screen --pane-id` | Direct on **Zellij 0.44+**; older versions fall back to Accessibility |
| **Ghostty** | Stable outer terminal identity | Hosts the inner Herdr/tmux/Zellij adapter; Ghostty itself is not read as a screen source |

Pane attribution is intentionally limited to a uniquely rendered single line. Multiline or ambiguous renders fall back to Accessibility rather than guessing. If Lumen cannot prove that the same pane is still active, it also fails closed to the Accessibility observer. Raw pane snapshots are not logged or stored; only hashes and the attributed edited span enter the learning/audit records. Enable post-insert observation under **Settings → Edit learning**.

### Day-to-day tips

- Prefer **hold** mode until the gesture feels natural.
- If a paste doesn’t land, Lumen leaves the text on the clipboard and says so; check Accessibility and whether the target app accepts paste.
- Press **Esc** to throw away a dictation you didn’t mean.
- Cloud correctors: use models that support **disabling thinking** for dictation speed; Lumen sends no-thinking flags where the API allows it.

### Project layout (for contributors)

```
apps/desktop/     Tauri + React app
crates/           Rust workspace (ASR, corrector, meeting, store, inject, eval, …)
scripts/          Local build, codesign, install, CI boundary checks, release helpers
docs/             Design, platform, release and evaluation notes
```

More detail: [PRODUCT.md](./PRODUCT.md) · [ARCHITECTURE.md](./ARCHITECTURE.md) · [docs/EVAL.md](./docs/EVAL.md) · [docs/MACOS_GITHUB_RELEASE.md](./docs/MACOS_GITHUB_RELEASE.md)

### License

Lumen ASR is licensed under the [GNU General Public License v3.0
only](./LICENSE). Release integrity and signing requirements are documented in
the [Code signing policy](./CODE_SIGNING_POLICY.md). See the
[Privacy policy](./PRIVACY.md) for local storage and optional network
processing.

---

### Lower playback volume during dictation (macOS)

To enable output ducking, edit the existing `[audio]` section in your
Application Support/LumenAsr `config.toml`, then restart Lumen ASR:

```toml
[audio]
duck_output = true
duck_volume = 0.15
```

This option is off by default. `duck_volume` is an absolute volume ceiling
between `0.0` and `1.0`; already quieter output stays quiet. It lowers all audio
on the output device selected when dictation starts, and restores that device
when capture stops or is cancelled, before transcription finishes. Meeting
recording does not use this option. Devices without a writable master volume
are skipped; dictation continues normally.

An observed volume adjustment during recording is preserved instead of being
overwritten on restore. Device changes do not transfer the saved volume to the
new device. Volume changes racing restoration cannot be fully excluded, and
force-quitting or power loss cannot restore volume automatically. Playback is
not paused; this option does not change the system mute switch.

<a id="中文"></a>

## 中文

<p align="right"><a href="#english">English</a> · <a href="#中文">中文</a></p>

**最新版本：[v0.4.0](https://github.com/fakechris/lumen-asr/releases/tag/v0.4.0)**（2026-11-24）· [更新内容](./docs/RELEASE_NOTES.md) · [全部版本](https://github.com/fakechris/lumen-asr/releases)

### 快速开始

1. **安装** —— 从 [GitHub Releases](https://github.com/fakechris/lumen-asr/releases) 下载最新的 Apple Silicon DMG 或 Windows x64 安装包（用 `SHA256SUMS.txt` 校验）；macOS 上也可以源码构建（`./dev-install.sh --open`）
2. **授权** —— 麦克风，macOS 上再加辅助功能（用于向其他应用粘贴）
3. **按住快捷键说话，松开** —— macOS 默认 **Alt+Space**（⌥空格），Windows 默认 **Ctrl+Shift+Space**。文本插入到当前焦点输入框；目标拒绝输入时，文本留在剪贴板并提示你
4. **会议** —— 打开「会议」页开始录音（或导入文件），结束后得到带说话人的逐字稿；配置了 AI 修正器时还有结构化纪要，否则是本地摘要

细节与引擎选择见下文。

### v0.4.0 新增

- **会议**：麦克风 + 系统音频双轨录音；说话人分离与声纹库；按模板生成结构化纪要；停止后**几秒内出草稿纪要**；未配置 AI 时生成**本地摘要**；实时稿防崩溃日志；会后处理串行排队、重启后继续
- **问会议**：标题、逐字稿、纪要、笔记全文搜索（支持中文）；用自然语言提问，答案只依据会议库，并附可点击跳转到具体时间点的出处
- **实时听写**（可选，OpenAI / 火山引擎）：胶囊实时预览，词库作为热词
- **插入更可靠**：Esc 取消听写；确认文字真的落进输入框；原剪贴板（含图片、富文本）只在没被别人改动时才恢复
- **AI 整理更安全**：输出完整性防护、截断检测、词库按相关度注入；正式发布版把 API Key 存进 macOS 钥匙串

完整说明见 [docs/RELEASE_NOTES.md](./docs/RELEASE_NOTES.md)。

### Lumen 是什么

**按住快捷键说话 → 松手 → 干净文本出现在当前输入框。**

Lumen 面向真实写作场景：本地语音识别 + 可选 AI 整理/翻译 + 自动粘贴到当前 App，并保留历史与个人词库。整理只负责把你说的话理顺，不会回答问题或执行指令。

开会、访谈时，**会议**模式同时录下双方、区分说话人、生成纪要，并沉淀为可以搜索、可以提问的会议库。

**默认本地。** 识别可使用本机 SenseVoice、Qwen3-ASR 或 Whisper；整理可使用本机大模型（如 Ollama）。效果比隐私更重要时，可以把 ASR 切到云端，或把修正器切到在线 OpenAI 兼容接口。

| 需求 | 建议 |
|------|------|
| 隐私、轻量离线识别 | 本地 SenseVoice（默认） |
| 更高精度的本地识别 | 本地 Qwen3-ASR |
| 另一条全本地识别路径 | 本地 Whisper |
| 在线语音转写 | OpenAI Audio、火山引擎（豆包）、MiniMax，或自定义 OpenAI 兼容接口 |
| 边说边出字 | OpenAI Audio 或火山引擎的实时模式（需手动开启） |
| 轻度整理、不上云 | 本地 LLM（Ollama / LM Studio） |
| 更强改写 / 翻译 | 在线修正（如 MiniMax-M3 等兼容接口） |
| 只要原始转写 | 整理强度选「无」 |

### 功能一览

**听写**

- 默认 **按住说话**，也可切换为开关模式；**Esc** 取消正在进行的听写（只在录音或处理期间占用 Esc）
- **意图快捷键**：默认整理 vs **翻译**（Alt+Shift+T），翻译风格可选正式 / 口语 / 社交 / 自定义
- 说话时 **悬浮胶囊**，不抢当前输入焦点；实时模式下显示实时预览
- 整理强度：无 / 轻 / 中 / 强；输出完整性防护与截断回退——整理结果跑偏或被截断时插入原话
- **个人词库**：术语与替换，按相关度注入整理提示词，并作为实时热词；可从已归因的插入后修改中学习（始终需要你确认）
- **上下文辅助整理**（需手动开启）：可发送光标附近的有界文本，完整上下文只在本机加密保存
- **可靠插入**：粘贴后在能读取输入框内容时确认落地，失败重试一次，仍失败则留在剪贴板并提示；原剪贴板（所有类型）只在未被改动时恢复
- **蓝牙耳机时用内置麦克风**：默认输入是蓝牙耳机且你没手动选设备时，改用内置麦克风，AirPods 不会被切到通话音质
- 静音自动停止（RMS 或 Silero VAD）、提示音、可选的播放音量避让、录音开始即预热模型
- **终端 pane 观察**：直接跟踪 Herdr、tmux、Zellij 中的修改，不只依赖辅助功能文本

**会议**

- **录音**：麦克风与系统音频分轨（可按应用采集），回声抑制，默认 Ogg-Opus；静音 15 分钟自动停止、最长 8 小时、低电量 / 休眠提醒、录音时阻止系统空闲休眠
- **自动检测**：会议应用开始通话时弹出提示，通话结束时建议停止；从不自行开始或停止录音
- **说话人**：离线说话人分离、实时说话人标签、全局声纹库（注册、改名、合并），手动标注可扩散到未标注片段
- **纪要**：内置模板 + 自定义模板（`minutes-templates/*.md`），长会议分段汇总；停止后立即用实时稿生成**草稿纪要**；未配置 AI 时生成基于关键词的**本地摘要**
- **会议库**：全文搜索（⌘F、↑/↓、Enter、Esc）、**问会议**（答案附出处）、失败会议一键重新处理；导出 Markdown 纪要 / Markdown 逐字稿 / SRT / JSON，以及 MP3 / OGG / WAV 音频
- **稳健处理**：一次只处理一场，重启后继续；实时稿防崩溃日志；日志记录各阶段耗时
- **文件导入**：选择或拖入 wav/mp3/m4a/mp4/opus/ogg，走与现场录音相同的流程
- **云端会议转写**（需手动开启）：把双轨音频实时发送到支持实时转写的服务

**隐私与数据**

- 正式发布版把 API Key 存入 **macOS 钥匙串**（开发构建保存在权限 0600 的配置文件中）
- **历史**可导出（Markdown / JSONL），可设置保留期限；加密上下文快照也可设置保留期限
- **可审计流水线记录**：不可变尝试、上下文来源与编辑观察结果
- 哪些数据留在本机、哪些会发到哪里，见 [PRIVACY.md](./PRIVACY.md)

**工具**

- **整段录音 CLI**：对整段录音做说话人分段 + 逐段 ASR，详见 [docs/OFFLINE_FILE_TRANSCRIPT.md](./docs/OFFLINE_FILE_TRANSCRIPT.md)
- **ASR 质量回归评测**：固定语料 + 归一化 CER/WER 门禁，详见 [docs/EVAL.md](./docs/EVAL.md)
- **本地模型独立配置**：切换 SenseVoice、Qwen3-ASR、Whisper 时保留各自路径与整理配置

### 整段录音离线转写（CLI）

桌面二进制的 headless 模式（不启 GUI、不写入会议库）：

```bash
./target/release/lumen-asr-desktop meeting process ./recording.m4a \
  --engine mlx-whisper --lang es \
  --format bilingual --translate zh
```

引擎：`sensevoice`（听写/CJK）、`qwen`（sherpa-onnx 多语，自动识别语种）、`mlx-whisper`（Metal Whisper）、`whisper`（sherpa CPU）。完整参数与模型目录见 [docs/OFFLINE_FILE_TRANSCRIPT.md](./docs/OFFLINE_FILE_TRANSCRIPT.md)。

### 环境要求

- Apple Silicon 上的 macOS 12+，或 Windows 10/11 x64
- **麦克风**：录音
- **macOS 辅助功能**：向其他 App 粘贴（未授权时仅复制到剪贴板）
- **macOS 系统音频录制**：只在需要录下通话另一方的会议中使用，开始这类会议时才申请
- **macOS 日历**（可选）：读取当前日程，为会议命名并记录参会人，只读
- **Windows 插入**：用 Unicode 输入和 Ctrl+V 写入当前窗口；目标拒绝输入时复制到剪贴板。暂不采集应用上下文
- 可选：Ollama 或任意 OpenAI 兼容 API，用于 AI 整理与会议纪要

### 安装与启动

每个 tag 版本都会在 [GitHub Releases](https://github.com/fakechris/lumen-asr/releases)
发布 Apple Silicon 的 ad-hoc 签名 DMG、Windows x64 NSIS 安装包和
`SHA256SUMS.txt`；不提供 Intel Mac 或 Windows x86 安装包。macOS 首次启动需要前往
**系统设置 → 隐私与安全性**手工放行。校验与安装方法见
[docs/MACOS_GITHUB_RELEASE.md](./docs/MACOS_GITHUB_RELEASE.md)，首次启动说明见
[docs/MACOS_RELEASE_NOTES.md](./docs/MACOS_RELEASE_NOTES.md)。

项目正在申请 SignPath Foundation 的 Windows 可信签名。免费签名服务由
[SignPath.io](https://signpath.io/) 提供，证书由
[SignPath Foundation](https://signpath.org/) 提供。获批并接入流水线前，
Windows 下载会明确标记为未签名。

Windows 开发者可以为本机测试免费创建自签名 MSIX；该流程需要显式信任
本地开发证书，不能替代 Store 或 SignPath 的正式签名。详见
[docs/WINDOWS_LOCAL_SIGNING.md](./docs/WINDOWS_LOCAL_SIGNING.md)。

如需从源码构建：

```bash
git clone https://github.com/fakechris/lumen-asr.git
cd lumen-asr

# 编译 release、装入 .app、签名并打开
./dev-install.sh --open
```

开发热更新：

```bash
cd apps/desktop
npm install
npm run tauri dev
```

> **签名提示：** 每次 release 编译后请用 `./dev-install.sh` 重新安装，否则 `.app` 签名会失效。建议使用受信任的本地代码签名证书或免费 Apple Development 证书，以便麦克风/辅助功能授权在重装后尽量保留。详见 [docs/MACOS_LOCAL_SIGNING.md](./docs/MACOS_LOCAL_SIGNING.md)。

### 第一次使用（分步）

#### 1. 打开应用

启动 **Lumen ASR**。从 Releases 下载的版本首次启动会被拦截：先打开一次，然后在 **系统设置 → 隐私与安全性** 点击「仍要打开」。

#### 2. 走完引导

| 步骤 | 操作 | 目的 |
|------|------|------|
| 欢迎 | 继续 | 一句话说明 |
| 权限 | 允许麦克风。macOS 打开辅助功能后，把浮层图标拖进列表并打开开关 | 录音 + 插入其它应用 |
| 热键 | 保持默认，或改成你习惯的组合 | 按住说话 |
| 就绪 | 可选试听，然后开始使用 | 第一次成功 |

跳过引导后可在设置中再次打开。

#### 3. 系统权限

**系统设置 → 隐私与安全性**

- **麦克风** → 勾选 **Lumen ASR**
- **辅助功能** → 把浮层里的 Lumen 图标拖进列表并打开开关（不必点「+」去找应用）

修改辅助功能后请完全退出再打开 Lumen。

#### 4. 选择语音识别引擎

默认仍是 sherpa-onnx 驱动的 **SenseVoice**。每个本地引擎都有独立的模型路径，来回切换不会覆盖之前的选择。

| 引擎 | 适用场景 | 所需内容 |
|------|----------|----------|
| **SenseVoice** | 资源占用较低、快速本地听写 | `model.int8.onnx`（或 `model.onnx`）+ `tokens.txt` |
| **Qwen3-ASR** | 更高精度的本地识别；自动识别语种 | sherpa-onnx 模型目录（`conv_frontend.onnx`、`encoder.int8.onnx`、`decoder.int8.onnx`、`tokenizer/`），可在设置页一键下载 |
| **Whisper** | 另一条本地 ONNX 路线 | encoder ONNX + decoder ONNX + tokens 文本 |
| **OpenAI Audio** | 在线转写；支持实时模式 | 兼容接口、模型与 API Key |
| **火山引擎（豆包）** | 中文在线转写；支持实时模式 | API Key（新版控制台），或 App ID + Access Token |
| **MiniMax** | 在线转写；也可用于云端会议转写 | API Key |
| **自定义** | 任意 OpenAI 兼容转写接口 | 接口地址、模型、API Key |

云端请求超时或连接失败、且本机装有 SenseVoice 或 Whisper 模型时，听写会回退到本地引擎；其他云端错误会直接报错。

本地模型共享根目录：

1. 环境变量 `LUMEN_MODELS_DIR`
2. `~/Library/Application Support/Lumen/models/`

`LUMEN_SENSEVOICE_DIR`、`LUMEN_WHISPER_DIR` 等引擎变量优先。Lumen 也会发现旧版 Lumen/Navi 与 coli 目录中的可用模型，但不会自动移动它们。下载的模型会按钉住的 SHA-256 校验。

在应用内打开 **设置 → 语音识别**，可以切换引擎、选择已发现模型，或验证自定义目录。

Qwen3-ASR 通过 sherpa-onnx 在进程内运行（无需 Python runtime）；设置 → 语音识别里可一键下载钉住哈希的模型包。

#### 5. 配置 AI 整理（推荐策略）

**原则：默认本地；效果不够再上云。**

| 方式 | 设置 | 适用 |
|------|------|------|
| **本地** | AI 修正 → Ollama + 已拉取模型 | 日常、隐私优先 |
| **云端** | MiniMax / 其他兼容接口 + API Key（听写建议 **MiniMax-M3**，请求侧会关闭 thinking） | 更强改写与翻译 |
| **关闭** | 整理强度「无」或关闭修正 | 只要 ASR 原文 |

同一个修正器也负责生成会议纪要和回答「问会议」。

高级配置：`~/Library/Application Support/LumenAsr/config.toml`

> **上下文隐私：** Lumen 可以把有长度上限的上下文加密保存在本机，用于审计与来源记录。只有开启“用当前应用和光标附近文字辅助纠错”后，才会把光标附近的有界投影加入修正请求；使用云端修正器时，这份投影会离开本机，完整快照不会上传。

#### 6. 快捷键与使用流程

| 动作 | 默认（可在设置中改） |
|------|----------------------|
| 听写（按住） | macOS **Alt+Space**（⌥空格），Windows **Ctrl+Shift+Space** |
| 翻译意图（按住） | **Alt+Shift+T** |
| 取消当前听写 | **Esc**（只在录音或处理期间） |

**标准流程**

1. 光标点进任意输入框
2. **按住**听写键 — 出现胶囊
3. 说话
4. **松开** — 识别 → 整理 → 粘贴
5. 在 **历史** 中回放录音、再次复制或导出

#### 7. 提高成品质量

- 整理强度默认 **中** 即可
- 在 **词库** 加入专有名词
- 在历史中改稿，有学习建议时按需入库

### 会议

1. 打开 **会议** 页 → **开始会议**（或在通话开始时接受提示），也可以 **导入文件**。
2. 录音中，实时稿会显示说话人标签；可以随时改会议标题和说话人名字。
3. 停止后，如果配置了修正器，几秒内先出一版基于实时稿的 **草稿纪要**；随后离线流程生成权威逐字稿（说话人分离、逐段识别、整理）；配置了修正器时再生成正式纪要，自动替换草稿。
4. 在会议库里用 ⌘F 搜索一切内容，或切到 **问会议** 直接提问，比如“上周二关于预算定了什么？”——每条结论都标注出处会议和时间点，点击即可跳转。

生成纪要需要配置修正器；未配置时仍会得到逐字稿，以及明确标注的本地摘要。录下其他参会者的声音前，请按当地法规取得对方同意。

### 终端 pane 编辑学习

听写文本插入后，Lumen 可以通过终端工具自身的 pane API 观察用户修改。它会锁定外层终端界面和内层 client/session/pane 身份，只在插入行能够唯一归因时记录修改，不需要通过后台选中文字来读取屏幕，也不会被无关的 TUI 重绘误导。

| 环境 | 读取路径 | 支持状态 |
|------|----------|----------|
| **Herdr** | 锁定 Herdr pane ID，读取未软换行的近期渲染缓冲区 | 直接 pane 观察 |
| **tmux** | 锁定 client/session/pane，使用 `capture-pane -J` | 直接 pane 观察 |
| **Zellij** | 锁定 client/pane，定向调用 `dump-screen --pane-id` | **Zellij 0.44+** 直接观察；旧版本回退辅助功能 |
| **Ghostty** | 锁定稳定的外层 terminal 身份 | 读取内层 Herdr/tmux/Zellij；不把 Ghostty 自身当作 screen 数据源 |

pane 归因目前只处理能够唯一定位的单行文本。遇到多行或画面歧义时会回退辅助功能，不会猜测。如果无法证明当前仍是同一个 pane，同样会安全回退。pane 原始快照不会写入日志或数据库，只保存哈希与归因后的修改片段。可在 **设置 → 编辑学习 → 粘贴后监听目标输入框改动** 中启用。

### 日常提示

- 先用 **按住** 模式建立肌肉记忆
- 粘贴没落地时，文本会留在剪贴板并提示；检查辅助功能与目标 App 是否接受粘贴
- 说错了就按 **Esc** 丢掉这次听写
- 云端模型：听写场景尽量选 **可关闭 thinking** 的型号，延迟更低
- 重新编译后请执行 `./dev-install.sh`，不要只拷贝二进制进 `.app`

### 仓库结构（给开发者）

```
apps/desktop/     桌面端（Tauri + React）
crates/           Rust 工作区（ASR、修正器、会议、存储、插入、评测……）
scripts/          本地编译与签名、CI 边界检查、发布脚本
docs/             设计、平台、发布与评测说明
```

更多：[PRODUCT.md](./PRODUCT.md) · [ARCHITECTURE.md](./ARCHITECTURE.md) · [docs/EVAL.md](./docs/EVAL.md) · [docs/MACOS_GITHUB_RELEASE.md](./docs/MACOS_GITHUB_RELEASE.md)

### 听写期间降低播放音量（macOS）

在 Application Support/LumenAsr 的 `config.toml` 中，将现有 `[audio]`
段的 `duck_output` 设为 `true`、`duck_volume` 设为 `0.15`，重启生效。
默认关闭；音量值范围为 0–1，是绝对上限，不会调高本来更低的音量。
停止采集或取消后恢复原设备，检测到用户调整音量时保留其选择。
不支持主音量调节的设备会跳过；会议不受影响；不暂停播放或改变静音开关。
恢复瞬间的并发音量调整无法完全排除；强制退出或断电无法保证自动恢复。

### 许可证

Lumen ASR 使用 [GNU General Public License v3.0
only](./LICENSE) 授权。发布完整性与签名要求见
[代码签名政策](./CODE_SIGNING_POLICY.md)，本地存储与可选网络处理见
[隐私政策](./PRIVACY.md)。
