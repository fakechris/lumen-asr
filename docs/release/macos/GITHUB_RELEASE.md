# macOS GitHub Release

Lumen ASR 的公开测试版采用：

```text
ad-hoc 签名 → 分架构 DMG → SHA-256 校验 → GitHub Release
```

这套发布方式不需要 Apple Developer 账号、证书、Team ID、Provisioning Profile 或公证。代价是用户首次打开时必须在“系统设置 → 隐私与安全性”中手工放行。

## Pipeline 做了什么

推送形如 `vMAJOR.MINOR.PATCH` 的 tag 后，[release-macos.yml](../../../.github/workflows/release-macos.yml) 会：

1. 从 tag 解析版本，并把它注入 Tauri App bundle；
2. 在 Apple Silicon runner 上构建 `aarch64-apple-darwin`；
3. 在 Intel runner 上构建 `x86_64-apple-darwin`；
4. 挂载最终 DMG，检查其中 `.app` 的版本、主程序、CPU 架构、签名完整性和 `Signature=adhoc`；
5. 生成两个命名稳定的 DMG 和 `SHA256SUMS.txt`；
6. 只有两个构建都成功后，才创建 GitHub Release 并上传全部文件。

最终 Release 包含：

```text
Lumen-ASR-v0.1.0-arm64.dmg
Lumen-ASR-v0.1.0-x64.dmg
SHA256SUMS.txt
```

Git tag 是发布版本的唯一来源；CI 中不需要提交临时的版本号修改。当前只接受稳定版本 tag，例如 `v0.1.0`，不接受 `v0.1`、`0.1.0` 或预发布 tag。

## 发布一个版本

先确认要发布的 commit 已推送并通过常规检查，然后创建 annotated tag：

```bash
git tag -a v0.1.0 -m "Lumen ASR v0.1.0"
git push origin v0.1.0
```

随后在 GitHub 的 Actions 页面查看 `Release macOS DMGs`。成功后，Release 会自动出现在仓库的 Releases 页面。

Workflow 使用仓库自带的 `GITHUB_TOKEN`，不需要新增 secrets。它只给构建 job `contents: read`，仅发布 job 获得 `contents: write`。如果组织策略禁止 Actions 写 Release，需要由仓库管理员放开相应策略。

已经公开的版本不要移动或复用 tag。需要修复时递增 patch 版本，例如从 `v0.1.0` 发布 `v0.1.1`。

## 下载后的验证与安装

将所选 DMG 和 `SHA256SUMS.txt` 放在同一目录，并只校验对应架构的条目：

```bash
# Apple Silicon
grep 'arm64\.dmg$' SHA256SUMS.txt | shasum -a 256 --check

# Intel
grep 'x64\.dmg$' SHA256SUMS.txt | shasum -a 256 --check
```

然后：

1. Apple Silicon 用户下载 `arm64.dmg`，Intel 用户下载 `x64.dmg`；
2. 双击 DMG，将 Lumen ASR 拖入 Applications；
3. 尝试启动一次；
4. 打开“系统设置 → 隐私与安全性”，点击 Lumen ASR 对应的“仍要打开”；
5. 再次启动，并按引导授予麦克风和辅助功能权限。

Gatekeeper 对 ad-hoc 版本的默认拒绝是预期行为。`codesign --verify` 验证的是包是否被篡改；它不等同于 Developer ID 信任或 Apple 公证。

发布后的最终验收必须使用浏览器从 GitHub Release 重新下载 DMG，最好在另一台 Mac、干净用户或虚拟机中测试。直接运行本地构建产物不能完整模拟下载文件的 quarantine 行为。
