# Lumen ASR — Desktop UI Design (macOS-first)

> Shell IA and interaction patterns aligned with Wispr Flow / Typeless / 闪电说.

## 1. Framework (what we use)

| Layer | Choice | Why |
|-------|--------|-----|
| Shell | **Tauri 2** (Rust + WebView) | Same class as 闪电说 / OpenLess — not Electron for binary size |
| UI | React | Settings + history density; not “fake web page” chrome |
| Global hotkey | `tauri-plugin-global-shortcut` | Chord register; capture UI pauses registration while recording |
| Window chrome | **System titlebar (Visible)** | Native drag, traffic lights, double-click maximize, Mission Control — Overlay + custom drag is fragile |

**Non-goal for MVP:** pure SwiftUI rewrite. Competitors with Electron/Tauri still feel native when they keep OS chrome and use click-to-record shortcuts.

## 2. Competitor patterns we copy

| Product | Default hotkey | Set hotkey UX | Window |
|---------|----------------|---------------|--------|
| **Typeless** | `Fn` (push-to-talk); extra chords in Settings | Click / add shortcut — press keys | OS + Electron chrome, fully draggable |
| **闪电说** | `Fn` / right ⌘ free mode | Config + pause/resume global hooks while UI edits | Tauri multi-window, native-feeling shell |
| **Wispr Flow** | Often right-modifier / Fn class | Click field → press new chord | Native Mac window affordances |

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
- Works with global-shortcut (true bare `Fn` needs lower-level hooks; later)

Users with an existing `config.toml` keep their saved chord until they re-record.

### Capture UX (must match competitors)

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

System light/dark, SF / `-apple-system`, accent ≈ `#007aff` / `#0a84ff`. See `styles.css`.

## 7. Later (not blocking this pass)

- Bare `Fn` / right-⌘ push-to-talk via CGEventTap (闪电说-class)  
- Hold vs toggle modes  
- Menu bar extra  
- Transparent titlebar + vibrancy without breaking drag  
