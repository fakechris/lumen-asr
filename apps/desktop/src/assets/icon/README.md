# Lumen ASR — App Icon assets

## Files
- `AppIcon.svg` — master vector (1024, full detail)
- `AppIcon-small.svg` — simplified variant for 16/32px (fewer wave rings, larger core)
- `AppIcon-512.png`, `AppIcon-1024.png` — standalone marketing PNGs
- `Lumen.iconset/` — full macOS iconset (16→512 + @2x)

## Build .icns (macOS)
```sh
iconutil -c icns assets/icon/Lumen.iconset -o Lumen.icns
```
Then point Tauri at it in `tauri.conf.json` → `bundle.icon`.

## Notes
- Squircle corner radius ≈ 22.4% (rx 229 @ 1024).
- Palette: bg gradient #3B86FF→#0E4ECB→#082C82; warm glow #FFD27A→#FFB020; core #FFFDF5.
- 16/32px use the simplified variant so the core + waves stay legible.
