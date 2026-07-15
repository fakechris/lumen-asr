# macOS local signing (no paid Apple Developer Program)

> This document covers the daily development loop on one machine. Public test builds use
> **ad-hoc signing + DMG + GitHub Release** instead; see
> [MACOS_GITHUB_RELEASE.md](./MACOS_GITHUB_RELEASE.md). A stable local identity preserves TCC
> permissions during development, while an ad-hoc identity is intentionally used for portable CI
> builds that require no certificate.

## What’s going wrong

Two separate issues get mixed together:

1. **Signature invalid after compile** — you `cargo build` then overwrite `Lumen ASR.app/Contents/MacOS/…` without re-signing. The embedded signature no longer matches the binary → “签名失效”.
2. **Permissions die every rebuild** — signing with **ad-hoc** (`codesign -s -`) mints a new **cdhash** every time. Mic / Accessibility TCC rows bind to that hash → look “broken” until re-granted.

| Method | Needs | Gatekeeper / share | Rebuild TCC (Mic / Accessibility) |
|--------|--------|--------------------|-----------------------------------|
| **Ad-hoc** `codesign -s -` | nothing | rejected if shared | **Breaks every rebuild** (new cdhash) |
| **Self-signed “Code Signing” cert** | one-time Keychain trust | local only | **Stable** (same cert requirement) |
| **Apple Development** (free Personal Team) | free Apple ID + yearly refresh | local | Stable while cert valid |
| **Developer ID + notarize** | **\$99/yr program** + Team ID | distribute outside Store | Stable |

### Your machine (snapshot)

- Free Personal Team: Apple ID linked in Xcode → Team ID **`69P8KHFNAT`** (Chris Song Personal Team)
- Old `Apple Development: chris__song@hotmail.com (UB3UKXAMG7)` → **expired 2024-04-16** (`CSSMERR_TP_CERT_EXPIRED`)
- Valid codesign identities were otherwise company/iPhone certs — **do not** use those for Lumen
- Until a trusted local or Apple Development cert exists, scripts fall back to ad-hoc

**You do not need a paid Team ID** for local testing. Free Personal Team ID is enough for Development certs; self-signed needs no Team ID at all.

Also: never “only copy the binary” after build — always run `dev-install.sh` / `sign-app.sh`.

## Recommended daily workflow

```bash
# from repo root
./scripts/macos/dev-install.sh --open
```

What it does:

1. `cargo build -p lumen-asr-desktop --release`
2. Copies binary into `target/release/bundle/macos/Lumen ASR.app`
3. Ensures keychain identity **`Lumen Local Codesign`** (creates once if missing)
4. `codesign --force --deep` with that identity (stable across builds)
5. Optionally `open` the app

Re-sign only:

```bash
./scripts/macos/sign-app.sh
```

Create identity only:

```bash
./scripts/macos/ensure-local-identity.sh
```

### Environment overrides

| Env | Default | Meaning |
|-----|---------|---------|
| `LUMEN_CODESIGN_IDENTITY` | `Lumen Local Codesign` | codesign `-s` name |
| `LUMEN_CODESIGN_HARDENED` | `0` | set `1` for hardened runtime |
| `LUMEN_CODESIGN_ENTITLEMENTS` | `scripts/macos/entitlements.dev.plist` | entitlements file |

## Optional: free Apple Development (Personal Team)

No \$99 program required. Once a year:

1. Xcode → **Settings → Accounts** → add `chris__song@hotmail.com`
2. Select **Personal Team** (`69P8KHFNAT`) → **Manage Certificates…** → **+** → **Apple Development**
3. Confirm:

   ```bash
   security find-identity -v -p codesigning | grep "Apple Development"
   ```

4. Use it:

   ```bash
   export LUMEN_CODESIGN_IDENTITY="Apple Development: chris__song@hotmail.com (XXXXXXXX)"
   ./scripts/macos/dev-install.sh --open
   ```

Notes:

- Free team **cannot** produce Developer ID / notarized DMG for random Macs.
- Certs expire (~1 year); renew in Xcode when `CSSMERR_TP_CERT_EXPIRED` appears.

## What you still cannot do without \$99

- **Developer ID Application** certificate  
- **Notarization** (staple) so strangers double-click without right-click → Open  
- App Store distribution  

For friends testing: zip the ad-hoc/self-signed app + “right-click → Open”, or they build from source.

## TCC (Accessibility / Microphone) checklist

1. Always run the **same** `.app` path:  
   `…/target/release/bundle/macos/Lumen ASR.app`
2. Prefer **self-signed or Apple Development**, never permanent ad-hoc for daily use.
3. After switching identity (ad-hoc → local cert), remove stale **Lumen ASR** rows in  
   System Settings → Privacy & Security → Accessibility / Microphone, then re-enable once.
4. Full quit + relaunch after granting.

## Tauri note

`tauri.conf.json` sets `bundle.macOS.signingIdentity` to `-` so release builds made by Tauri are
ad-hoc signed by default. The daily install script then re-signs that app with the stable local
identity:

- direct Tauri / CI release build = ad-hoc
- `scripts/macos/dev-install.sh` = stable self-signed or free Development identity

```bash
# full tauri bundle then our stable sign
cd apps/desktop && npm run tauri -- build --bundles app
../../scripts/macos/sign-app.sh ../../target/release/bundle/macos/Lumen\ ASR.app
```

(Exact bundle output path may be workspace `target/release/bundle/macos/`.)
