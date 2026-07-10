# Lumen ASR — Desktop UI Design (macOS-first)

> Product shell for the MVP loop. Settings + global hotkey are first-class, not afterthoughts.

## 1. Product surfaces

| Surface | Role | Backend status |
|---------|------|----------------|
| **Main window** | Dictation playground, history, dictionary, learn, settings | M1–M6 wired |
| **Floating capsule** | Non-activating overlay while listening / processing | M5 done |
| **Global hotkey** | Toggle record → ASR → correct → paste | M5 done (`CommandOrControl+Shift+Space`) |
| **Settings** | Permissions, hotkey, inject, corrector, learning | M3–M6 done |

## 2. Information architecture

```
┌─────────────────────────────────────────────────────────┐
│  ░░ titlebar (overlay / drag)              Lumen ASR    │
├──────────┬──────────────────────────────────────────────┤
│ 录音     │  Content pane (title + primary actions)      │
│ 历史     │                                              │
│ 词典     │  Cards / forms / lists                       │
│ 学习     │                                              │
│ 设置     │                                              │
├──────────┴──────────────────────────────────────────────┤
│  status: DB · model · hotkey hint · permission chips    │
└─────────────────────────────────────────────────────────┘
```

### Nav (sidebar)

| Item | Default content |
|------|-----------------|
| **录音** | Device, engine, start/stop, transcript edit, insert, inline learn candidates |
| **历史** | Session list + detail + edit events |
| **词典** | Terms / replacements CRUD |
| **学习** | Before/after → candidates → confirm |
| **设置** | Permissions → Hotkey → Insert → Corrector → Learning |

### Capsule (separate window)

- Phases: idle (hidden) · listening · processing · error flash
- Stop button while listening
- Drag-friendly; no activation of main app when possible

## 3. Native chrome (macOS)

| Choice | Why |
|--------|-----|
| Sidebar + content | Matches System Settings / Notes density |
| Overlay titlebar + drag region | Feels like a real Mac app, not a browser tab strip |
| SF / system font stack | Zero custom display fonts |
| Light default + `prefers-color-scheme: dark` | Respect system appearance |
| Accent ≈ system blue `#007aff` | Familiar primary actions |
| Status bar with live hotkey string | User always sees how to dictate without opening Settings |

## 4. Settings layout (priority order)

1. **Permissions** — mic + accessibility (hard gates for record / inject)
2. **Hotkey** — enable, chord string, capsule toggle (save re-registers immediately)
3. **Insert** — auto-insert, clipboard restore, mode
4. **Corrector** — Ollama / OpenAI-compatible, probe button
5. **Learning** — auto-promote N, post-paste capture

Each section saves independently (fail-soft; no giant single form).

## 5. Hotkey UX rules

- Default: `CommandOrControl+Shift+Space` (display as **⌘⇧Space**)
- Save → persist TOML → `unregister_all` + re-register
- Listening state always reflected in capsule + main status
- If registration fails, show banner with the OS error (do not silently drop)

## 6. Visual tokens

| Token | Light | Dark |
|-------|-------|------|
| bg | `#f2f2f7` | `#1c1c1e` |
| sidebar | `#eaeaef` | `#2c2c2e` |
| card | `#ffffff` | `#2c2c2e` |
| text | `#1d1d1f` | `#f5f5f7` |
| muted | `#6e6e73` | `#98989d` |
| accent | `#007aff` | `#0a84ff` |
| danger | `#ff3b30` | `#ff453a` |
| ok | `#34c759` | `#30d158` |
| radius | 10–12px cards, 6px controls | same |

## 7. Out of scope for this shell pass

- Full SwiftUI rewrite (Tauri WebView is the product shell for MVP)
- Menu bar extra / LaunchAgent
- Onboarding wizard carousel (permissions live in Settings for now)
- Windows chrome (keep layout portable; styles already dual-theme)

## 8. Build / test checklist

- [ ] App launches; sidebar navigation works
- [ ] Settings → Permissions open System Settings
- [ ] Settings → Hotkey save re-binds chord
- [ ] Hotkey toggles capsule + dictation when models ready
- [ ] Record tab start/stop and insert path still work
- [ ] Dark/light follows system
