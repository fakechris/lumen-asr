## macOS 安装说明

请根据 Mac 类型下载对应的 DMG：

- Apple Silicon（M1 及后续机型）：`arm64.dmg`
- Intel Mac：`x64.dmg`

双击 DMG，将 Lumen ASR 拖入 Applications。首次启动会被 macOS 拦截；请前往“系统设置 → 隐私与安全性”，找到 Lumen ASR 后点击“仍要打开”，再按提示确认。

本应用采用 ad-hoc 代码签名，没有经过 Apple Developer ID 公证，因此首次运行必须手工放行。请只从本项目的 GitHub Releases 下载，并使用 `SHA256SUMS.txt` 验证文件完整性：

```bash
# Apple Silicon
grep 'arm64\.dmg$' SHA256SUMS.txt | shasum -a 256 --check

# Intel
grep 'x64\.dmg$' SHA256SUMS.txt | shasum -a 256 --check
```

安装后还需要按应用引导授予麦克风和辅助功能权限。
