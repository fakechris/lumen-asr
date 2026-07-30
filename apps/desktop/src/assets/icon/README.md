# Lumen ASR — App Icon assets

> **Single source of truth:** `../product-icons/lumen-asr.svg` (Lumen Design
> System mark). Everything in this folder and in `src-tauri/icons/` is generated
> from it by `scripts/macos/regen-icons.sh` — never hand-edit the PNGs/.icns.

## Files (all generated)
- `AppIcon.svg`, `AppIcon-small.svg` — copies of the DS mark (the mark is one
  flat glyph, so there is no separate "full detail" / small variant anymore)
- `AppIcon-512.png`, `AppIcon-1024.png` — standalone marketing PNGs
- `Lumen.iconset/` — full macOS iconset (16→512 + @2x)
- `Lumen.icns` — built from the iconset

## Regenerate everything
```sh
./scripts/macos/regen-icons.sh
```
This rewrites the Tauri bundle icons (`src-tauri/icons/*`), these app-icon
masters, and `docs/images/app-icon*.png` from the one SVG. Commit the results
together.

## Brand (Lumen Design System)
- Tile: rounded square r≈23%, **flat espresso `#231a13`**, identical on every
  product. Keyline: 1.5px inset stroke, white @ 8%. **No gradient, no glow.**
- Glyph: one geometric mark, categorical hue per product — ASR = waveform in
  `#e08a4f`.
- The old blue gradient/glow "sound-wave" icon is **deprecated** — do not
  reintroduce it.
