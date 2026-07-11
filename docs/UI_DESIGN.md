# Lumen ASR — Desktop UI Design (macOS-first)

> Shell IA and interaction patterns for a local-first dictation app.

## 1. Framework (what we use)

| Layer | Choice | Why |
|-------|--------|-----|
| Shell | **Tauri 2** (Rust + WebView) | Small binary, Rust platform code, multi-window |
| UI | React | Settings + history density; not “fake web page” chrome |
| Global hotkey | **CGEventTap** (primary) + fallbacks | Reliable press/hold/release, including modifier-only chords |
| Window chrome | **System titlebar (Visible)** | Native drag, traffic lights, double-click maximize, Mission Control — Overlay + custom drag is fragile |

**Non-goal for MVP:** pure SwiftUI rewrite. Keep OS chrome and click-to-record shortcuts so the shell feels native.

## 2. Interaction principles

### Anti-patterns we remove

1. **Hand-typed shortcut strings** (`CommandOrControl+Shift+Space`) — unfriendly and error-prone  
2. **Custom Overlay titlebar without `data-tauri-drag-region` + `allow-start-dragging`** — window cannot move  
3. **Default ⌘Space / easy Spotlight conflicts** — never ship Spotlight/IME defaults

## 3. Window policy (locked)

```
titleBarStyle: Visible   (system decorations)
hiddenTitle: false
no custom full-window drag region required
```

Result: drag anywhere on the system titlebar; sidebar does not steal drag; looks like System Settings / Notes.

## 4. Hotkey policy (locked)

### Default (new installs)

`Alt+Space` → display **⌥Space**

Why:

- Avoids Spotlight (`⌘Space`)
- Avoids common IME / input source chords when possible
- Easy one-hand reach on Mac keyboards
- Event tap supports modifier + key and modifier-only chords (true bare `Fn` later)

Users with an existing `config.toml` keep their saved chord until they re-record.

### Capture UX

```
┌─────────────────────────────────────────────┐
│  录音热键                                    │
│  ┌──────────────────────┐  ┌─────────────┐ │
│  │  ⌥Space              │  │ 点击录制    │ │
│  └──────────────────────┘  └─────────────┘ │
│  录制中: 请按下新组合键… Esc 取消           │
│  常用: [⌥Space] [⌃⇧Space] [⌘⇧D]            │
└─────────────────────────────────────────────┘
```

1. Click **点击录制** (or the kbd chip itself)  
2. Backend **pauses** global shortcuts (so the old chord is not fired)  
3. User presses modifiers + key  
4. UI shows pretty label; write config; **re-register**  
5. **Esc** cancels → resume previous registration  

Never require typing `CommandOrControl+…`.

## 5. Information architecture

```
┌──────────────────────────────────────────────────┐
│  ● ● ●   Lumen ASR          (system titlebar)    │
├──────────┬───────────────────────────────────────┤
│ 录音     │  Content                              │
│ 历史     │                                       │
│ 词典     │                                       │
│ 学习     │                                       │
│ 设置     │                                       │
│ 概览     │                                       │
├──────────┴───────────────────────────────────────┤
│  status · model · current hotkey (pretty)        │
└──────────────────────────────────────────────────┘
```

### Settings order

1. Permissions  
2. **Hotkey** (recorder)  
3. Insert  
4. Corrector  
5. Learning  

## 6. Visual tokens

From **Lumen 设计系统** (icon + product UX packs):

| Token | Light | Dark |
|-------|-------|------|
| Accent (system blue) | `#007AFF` | `#0A84FF` |
| Lumen warm (highlight) | `#FF9F0A` → `#FFC24B` | same |
| Success | `#34C759` | `#30D158` |
| Error / rec | `#FF3B30` | `#FF453A` |
| BG / sidebar / card | `#F2F2F7` / `#E8E8ED` / `#fff` | `#1C1C1E` / `#2C2C2E` |
| Radius | 11px cards, 8px controls | |
| Icons | 24×24 SF-linear, stroke 1.7, `currentColor` — `src/Icons.tsx` + `assets/icons/` | |
| App icon | Warm-core sonic waves on blue squircle — `src-tauri/icons/` | |

See `styles.css` for full CSS variables (`--lumen-warm*`, `--shadow-lift`, …).

## 7. Text insert

| Step | Behavior |
|------|----------|
| Primary | CGEvent Unicode type at current key focus |
| Fallback | Clipboard + ⌘V (with restore) |
| Focus | Cache frontmost app at record start; re-activate only if Lumen stole frontmost |

Implementation notes:

1. Wait until physical Alt/Shift/Ctrl are up before synthesizing keys (hotkey chord would turn ⌘V into ⌥⇧⌘V).
2. Do **not** force-activate the typing target when it is already frontmost — relaunch/activate can drop the caret.
3. Capsule is non-focusable feedback only.
4. Capture focus **synchronously** at press (NSWorkspace), before showing UI.

## 8. Later (not blocking this pass)

- Bare `Fn` / right-⌘  
- True `NSPanel` non-activating HUD  
- Menu bar extra  

