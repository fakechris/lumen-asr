# Lumen ASR 跨平台与 Windows 移植评估

评估日期：2026-07-13

## 结论

Lumen ASR 不是从零开始做 Windows 版。识别、修正、词典、SQLite 存储、提示词和文本注入策略编排等核心模块大体可复用；React + Tauri 也可以继续作为 Windows GUI。

但当前桌面产品仍是 **macOS-first，而不是可直接构建运行的跨平台应用**。Windows 的主要缺口不在 ASR 算法，而在系统集成：

- Windows 构建目前有明确的条件编译阻断；
- 默认纯修饰键按住说话在非 macOS 上会静默失效；
- 前台应用识别、焦点恢复和文本注入全部绑定 macOS 实现；
- 权限模型和 onboarding 把 macOS Accessibility 当成通用能力；
- 数据路径、模型下载、上下文加密与 sidecar 使用了 Unix/macOS 假设；
- Tauri 只配置了 DMG，GitHub Release 流水线也只等待并发布两个 macOS DMG；
- 当前没有面向最终用户的 TUI，主要 dictation 生命周期又耦合在 Tauri 命令层。

建议将第一阶段目标限定为 **Windows 10/11 x64 Alpha：普通组合键、录音、离线 ASR、修正、历史、复制到剪贴板、NSIS 安装包**。在这个闭环稳定之后，再加入纯修饰键 hold、自动插入、焦点恢复和完整上下文捕获。

## 当前能力矩阵

| 能力 | Windows 可复用度 | 当前状态 | 主要工作 |
| --- | --- | --- | --- |
| 核心 session/types | 高 | 基本平台无关 | 抽出真正的应用运行时，避免由 Tauri 驱动领域流程 |
| SQLite、词典、历史 | 高 | `rusqlite bundled`，无明显 OS 绑定 | 修正数据目录并跑 Windows 文件锁/迁移测试 |
| 文本修正、远程 ASR | 高 | HTTP/Rust 实现为主 | 网络、代理、证书和取消流程验证 |
| 本地 SenseVoice/Whisper | 中高（x64） | `sherpa-onnx 1.13.4` 有 Windows x64 预编译库 | 在 `windows-latest` 实际构建；明确不先承诺 ARM64 |
| 麦克风录音 | 中高 | CPAL 架构可复用，Windows 走 WASAPI | 设备热插拔、默认设备切换、独占模式、休眠唤醒、采样格式测试 |
| React/Tauri GUI | 中高 | 视图可复用，文案/标题栏/onboarding 偏 macOS | 平台化 UI 文案、Windows 图标、WebView2、overlay 行为 |
| 普通全局组合键 | 中高 | Tauri global-shortcut 支持 Windows | 使用包含主键的组合，例如 `Ctrl+Shift+Space` |
| 纯修饰键按住说话 | 低 | 非 macOS 读取修饰键状态恒为 0，实际不会触发 | Windows 低级键盘钩子或改变默认交互 |
| 前台应用与焦点恢复 | 低 | 直接依赖 `lumen-platform-macos` | `GetForegroundWindow` 等 Windows adapter；接受系统激活限制 |
| 自动文本插入 | 低 | Mac clipboard/CGEvent/AX backend | Windows clipboard + `SendInput`，可选 UI Automation fallback |
| 权限/能力检测 | 低 | DTO、trait、文案全部围绕 Accessibility/TCC/codesign | 改为 capability model；Windows 麦克风、UIPI、签名分别表达 |
| Capsule 浮层 | 中 | Tauri 通用 API 为主，但透明窗口只在 macOS 开启 | Windows 无焦点透明浮层、DPI、多显示器、全屏应用测试 |
| 上下文抓取/OCR | 很低 | AX、ScreenCaptureKit、Vision、Keychain、Unix socket | Windows UI Automation、Graphics Capture、OCR、安全存储、Named Pipe |
| 安装与 Release | 低 | 仅 DMG 和 macOS release workflow | NSIS/MSI、`.ico`、Windows 签名、单一 Release 聚合任务 |
| TUI | 尚未开始 | 只有 benchmark CLI，不是产品 TUI | 先抽 `lumen-runtime`，再加 Ratatui/Crossterm adapter |

这里的“可复用度”是基于代码结构的工程判断，不是运行覆盖率。

## 仓库中的关键证据

### 1. 平台抽象存在，但接口本身仍是 macOS 语义

`crates/lumen-platform/src/lib.rs` 已经定义 `Permissions`、`FrontmostApp` 和 `HotkeyListener`，这是良好起点。但 `PermissionStatus` 固定包含 `accessibility`，`can_inject()` 也等价于 Accessibility 已授权，方法名直接是 `open_accessibility_settings()`。Windows 没有对应的 macOS TCC Accessibility 开关。

数据目录在所有非 macOS 平台都取 `$HOME/.lumen-asr`，取不到 `HOME` 时回退 `/tmp`。这不是 Windows 的 Known Folder/AppData 语义，应改用 Tauri path API 或 `directories::ProjectDirs`，将变化数据放入 Local AppData，用户配置按产品选择 Local 或 Roaming AppData。

### 2. 非 macOS 的纯修饰键监听是静默失效

`apps/desktop/src-tauri/src/mod_chord.rs` 在非 macOS 上的 `read_mod_flags()` 恒定返回 `0`。上层 `register_fallback()` 仍会启动线程并返回“修饰键监听已注册”的提示。因此默认 `Alt+Shift` 一类 hold chord 在 Windows 上既不触发，也不可靠地暴露错误。

Tauri 的 global-shortcut 插件正式支持 Windows，可以先用 `Ctrl+Shift+Space` 这类包含主键的组合。若产品必须保留“只按两个修饰键、按住说话、松开停止”，需要实现 Windows `WH_KEYBOARD_LL`/Raw Input adapter，并处理左右键、自动重复、注入事件过滤、睡眠唤醒和钩子线程消息循环。

### 3. 桌面 dictation 和注入直接依赖 macOS backend

`dictation.rs`、`inject_cmd.rs`、`permissions_cmd.rs` 和 `lib.rs` 都直接引用 `lumen_platform_macos`：

- 前台 App 捕获、目标恢复使用 `NSWorkspace`/bundle id 语义；
- 注入使用 macOS clipboard、CGEvent `Cmd+V`、Unicode event 和 AX；
- 是否允许插入直接检查 `is_accessibility_trusted()`；
- 权限页面还执行 `codesign` 并解析 `.app/Contents/MacOS`。

macOS crate 提供的非 macOS stub 会让部分代码“能够解析依赖”，但常见结果是恒定 false/None/error。Windows 上当前 dictation 会把自动插入判断成无 Accessibility，退回 copy-only；并且用户会看到错误的 macOS System Settings 指引。

Windows backend 推荐分层实现：

1. Windows clipboard 写入与可选恢复；
2. `SendInput` 发送 `Ctrl+V`；
3. `KEYEVENTF_UNICODE` 作为字符级 fallback；
4. UI Automation 的 Value/TextPattern 作为可选高级 fallback，而不是 MVP 硬依赖。

Windows 的 `SendInput` 受 UIPI 限制：普通权限进程不能把输入注入到更高完整性级别的目标，例如以管理员身份运行的编辑器。产品要明确显示“目标 App 权限级别更高”，而不是伪装成通用 Accessibility 授权问题。

### 4. Windows 构建存在 context 的明确阻断

锁定的 `lumen-context` 中，`NativeBrowserBridgeConfig`、`NativeBrowserProvider` 和 native browser host 入口仅在 `cfg(unix)` 下导出，但 `apps/desktop/src-tauri/src/lib.rs` 和 `src/bin/lumen-asr-context-browser-host.rs` 无条件调用它们。Windows 构建会在类型/函数解析阶段失败。

此外，Context 默认实现依赖：

- macOS AX 获取可见文本与焦点；
- ScreenCaptureKit 截图；
- Apple Vision OCR；
- macOS Keychain 保存 context sealing key；
- Unix domain socket 作为浏览器桥接。

非 macOS 的多个 source 返回空集合或 unsupported，`from_macos_keychain()` 也无法初始化，所以即使先绕过编译，Context 也会被实质禁用。

建议 Windows Alpha 将 Context 设为 feature-gated、默认关闭。完整对齐另立项目：UI Automation + Windows Graphics Capture + OCR provider + Credential Manager/DPAPI + Named Pipe/Windows native messaging 安装。

### 5. 音频和本地 ASR 是最可复用的部分，但必须实机验证

`AudioCapture` 已经使用 CPAL 的通用 host/device/stream API。CPAL 在 Windows 上提供 WASAPI backend，因此不需要重写整个录音模块。代码中针对 CoreAudio zombie callback 的时序处理可以保留，但不能替代 Windows 的设备切换、休眠、Bluetooth、采样格式和长时间录音测试。

`sherpa-onnx-sys 1.13.4` 的构建脚本明确映射了 `windows + x86_64` 静态和动态预编译包，官方也发布 Windows x64 库。当前脚本没有 Windows ARM64 archive 映射，因此第一版只承诺 x64 更稳妥。

本机没有安装 Windows Rust target；`cargo check --target x86_64-pc-windows-msvc` 在加载标准库前停止，不能当成项目本身的编译结果。`cargo tree --target x86_64-pc-windows-msvc` 能解析出 CPAL/WASAPI、Tauri/WebView2 和 sherpa 依赖，但仍需要 `windows-latest` CI 才是有效构建证据。

### 6. 模型安装与文件路径需要平台化

`asr_models.rs` 直接启动外部 `curl` 和 `tar`，并使用 `/tmp` fallback。现代 Windows 环境可能带这些命令，但产品安装包不应把它们视为稳定契约。仓库已经依赖 `reqwest`，应使用 Rust HTTP streaming 下载，配合 Rust archive/bzip2 解压、临时文件原子替换、校验和和真正的 cancel。

`lumen-asr/src/paths.rs` 同样通过 `HOME` 拼 `.coli/models`。这些开发机 fallback 应与产品 AppData 路径分开，并使用 `PathBuf::join` 和标准目录 API。

### 7. GUI 可继续使用，平台体验需要条件化

Tauri 正式支持 Windows，并使用 WebView2；React 视图与大部分状态页面无需重写。但当前 UI 中有 macOS traffic lights、`⌘/⌥`、System Settings、Accessibility 和 `~/Library/Application Support/...` 等内容。`hotkeyFormat.ts` 也把 Meta 统一显示为 `⌘`。

Tauri config 当前包含 `macOSPrivateApi`、仅 `dmg` bundle、macOS signing/minimum version/Info.plist，图标目录没有 Windows `.ico`。建议：

- base `tauri.conf.json` 只保留跨平台值；
- macOS 值移到 `tauri.macos.conf.json`；
- Windows 使用 `tauri.windows.conf.json`，首选 `nsis`，后续按企业需求补 `msi`；
- Windows x64 默认 per-user 安装，避免安装器和运行进程不必要地提权；
- 明确 WebView2 安装策略；在线公开测试版可用默认 bootstrapper，离线企业包再考虑 offline installer；
- 生成 `.ico` 和 Windows Store/installer 所需图标。

Capsule 需要专门验证无焦点显示、透明、always-on-top、任务栏隐藏、多显示器和 DPI。当前透明/background-color 分支只在 macOS 执行。

### 8. Release pipeline 不能简单再加一个互相竞争的 workflow

当前 `.github/workflows/release-macos.yml` 在 tag `v*` 上构建 arm64/x64 两个 DMG，聚合校验后创建并发布 Release。它只接受 macOS 资产，发布后还会认为“两个 DMG + SHA256SUMS”就是完整 Release。

如果另外加一个独立的 Windows tag workflow，两边可能同时创建/修改同一 Release，Windows 资产可能在 macOS job 发布后才到达，`SHA256SUMS.txt` 也可能互相覆盖。

推荐改成一个 tag 驱动的 `release.yml`：

1. macOS arm64、macOS x64、Windows x64 三个 build job/matrix entry；
2. 每个 job 只上传 workflow artifact，不直接发布 Release；
3. 一个唯一的 aggregate job 等待所有必需平台；
4. 统一生成资产清单和 SHA-256；
5. 创建 draft、一次性上传完整资产、验证后发布；
6. 测试 tag（例如 `v0.2.0-beta.1`）设置 GitHub prerelease；
7. Windows 签名未配置时，资产名和 Release notes 明确标注 unsigned beta。

Tauri Windows installer可生成 NSIS `-setup.exe` 或 MSI。MSI 必须在 Windows 上构建；因此即使开发主要在 Mac，Release job 也应使用 `windows-latest`，不要把 Mac 跨编译作为主路径。

Windows 没有 macOS “ad-hoc signing”的直接同义机制。公开测试可以先发 unsigned installer，但浏览器下载后会出现更强的 SmartScreen 警告，企业策略甚至可能阻止。正式公开版应稳定使用受信任的代码签名身份；Microsoft Store 是另一条减少 SmartScreen 下载警告的渠道。

## 推荐的目标架构

不要继续在 Tauri command 里到处增加 `#[cfg(windows)]`。建议形成四层：

```text
crates/lumen-core + asr + corrector + dictionary + store + prompts
                             │
                    crates/lumen-runtime
       DictationService / SessionController / RuntimeEvent
                             │
            crates/lumen-platform (能力与 ports)
                ┌────────────┴────────────┐
    lumen-platform-macos       lumen-platform-windows
                └────────────┬────────────┘
                  apps/desktop / apps/tui
```

`lumen-runtime` 负责开始/停止录音、ASR、修正、持久化、注入策略和事件状态机，不依赖 `tauri::AppHandle`。Tauri adapter 只把 `RuntimeEvent` 转成 window event；TUI adapter 只把它绘制到终端。

平台接口应从“权限”改成“能力状态”，例如：

- microphone capture；
- global key chord；
- modifier-only hold hotkey；
- foreground target capture；
- clipboard copy；
- text injection；
- context text capture；
- screen capture；
- secure storage。

每个能力返回 `supported / available / blocked / degraded`、原因和 remediation。这样 macOS Accessibility、Windows UIPI、麦克风隐私和 Linux portal 才不必挤进一个布尔字段。

平台依赖也应 target-gated：macOS app build 不加载 Windows backend，Windows build 不加载 macOS backend。Context 先作为 feature/capability，而不是阻断整个桌面应用。

## TUI 评估

如果这里的 TUI 是“终端 UI”，目前仓库还没有产品 TUI；`lumen-bench` 只是参数式 benchmark CLI。

TUI 本身并不难。Ratatui 默认使用 Crossterm backend，适合 Windows Terminal、macOS 和 Linux。真正的工作在于把 dictation 运行时从 Tauri 中抽离。

建议两档：

### 前台 TUI（优先）

- TUI 聚焦时按 Space 开始/停止；
- 显示输入设备、音量、ASR/修正阶段和最终文本；
- 支持复制结果、查看最近 session、切换 provider/model；
- 不承诺后台全局热键与向其他 GUI App 自动注入。

在 `lumen-runtime` 抽出后，这部分约是一个较小 adapter。

### 后台伴随式 TUI

- 终端不聚焦时仍响应全局热键；
- 捕获原前台窗口并向其中插入文本；
- TUI 退出时正确释放 hook/audio/clipboard；
- Windows Terminal 的 key press/release 事件差异也要处理。

这不是“纯 TUI”能力，仍然依赖与 GUI 相同的 Windows 平台服务。不要在 TUI 内再实现第二套热键和注入逻辑。

如果原意其实是 **Tauri GUI**，结论是前端不用换技术栈；要重构的是 Rust backend 和平台能力模型。

## 分阶段交付路线

### Phase 0：Windows build green

- 新增 `windows-latest` 的 `cargo check`/frontend build；
- target-gate `lumen-platform-macos` 和 macOS Tauri feature；
- 将 context/browser sidecar 在 Windows 关闭或条件编译；
- 标准化 AppData、临时目录、sidecar `.exe` 路径；
- 替换 `curl`/`tar`；
- 添加 `tauri.windows.conf.json` 和 `.ico`；
- 暂时使用普通含主键 hotkey + copy-only。

完成标准：Windows x64 干净机器上能安装、选择麦克风、录音、离线识别、修正、复制、保存历史。

### Phase 1：可用 Windows Beta

- `lumen-platform-windows`；
- 前台窗口捕获；
- Windows clipboard + `SendInput` paste/type；
- UIPI 和 elevated target 的可解释降级；
- 纯修饰键 hold hook，或明确采用普通 chord UX；
- Capsule 多显示器/DPI/全屏/无焦点测试；
- Windows-specific onboarding；
- NSIS 安装、完整 tag Release 聚合、checksums；
- unsigned prerelease 或正式代码签名。

### Phase 2：运行时抽取与 TUI

- 新建 `lumen-runtime`；
- Tauri command 变成薄 adapter；
- 加前台 Ratatui/Crossterm TUI；
- GUI/TUI 共用 session、热键、注入和事件测试。

这一步也可以提前到 Phase 1 中做。若 Windows 和 TUI 都确定要做，越早抽运行时，后续重复越少。

### Phase 3：Context parity

- UI Automation 文本/焦点/secure-field 判断；
- Windows Graphics Capture；
- OCR provider；
- Credential Manager/DPAPI sealing key；
- Named Pipe/native messaging Windows 安装；
- 独立的隐私、性能和多屏测试矩阵。

## 粗略工作量

以下为一名熟悉 Rust/Tauri、具备 Win32 调试能力的工程师的区间估算，不含代码签名主体认证等待时间：

| 目标 | 估算 |
| --- | --- |
| Windows x64 copy-only Alpha，CI 可构建安装 | 1–2 周 |
| 普通/hold 热键、自动插入、焦点、Capsule、NSIS、Release 的可用 Beta | 累计 3–6 周 |
| 抽 `lumen-runtime` 后的前台 TUI | 约 3–7 个工作日 |
| 完整 Windows Context/OCR/浏览器桥接对齐 | 额外 4–8 周 |

最大不确定性不是 React，也不是 SQLite；是 Windows 输入/焦点的长尾兼容、ASR native packaging 实机验证和 Context parity。

## 建议的测试矩阵

- Windows 11 x64：主支持；Windows 10 x64：最低兼容验证；
- 内置麦克风、USB、Bluetooth、设备在录音期间切换/拔出；
- 中英日韩等模型支持语言、emoji、非 BMP 字符、中文输入法开启时注入；
- Notepad、Office、VS Code、Chrome/Edge、Windows Terminal、管理员权限目标；
- 普通 DPI、125%/150%/200%、双屏不同缩放、全屏应用；
- sleep/resume、锁屏、RDP；
- WebView2 已有/缺失、在线/离线安装；
- unsigned beta 和 signed build 的 SmartScreen/Smart App Control 体验；
- 热键冲突、连续快速按压、按下后 sleep、App crash 后 hook 释放。

## 主要官方资料

- [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/)
- [Tauri platform-specific configuration files](https://v2.tauri.app/develop/configuration-files/)
- [Tauri Windows installer: NSIS/MSI and WebView2](https://v2.tauri.app/distribute/windows-installer/)
- [Tauri global shortcut plugin supported platforms](https://v2.tauri.app/plugin/global-shortcut/)
- [Tauri GitHub Action: Windows/macOS/Linux and Release assets](https://github.com/tauri-apps/tauri-action)
- [CPAL backend table: Windows WASAPI/ASIO](https://docs.rs/crate/cpal/latest/source/README.md)
- [sherpa-onnx 1.13.4 Windows x64 prebuilt libraries](https://k2-fsa.github.io/sherpa/onnx/install/windows/generated/download/windows_x64.html)
- [Microsoft SendInput and UIPI restriction](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)
- [Microsoft KEYBDINPUT / KEYEVENTF_UNICODE](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-keybdinput)
- [Microsoft GetForegroundWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getforegroundwindow)
- [Microsoft SetForegroundWindow restrictions](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow)
- [Microsoft UI Automation](https://learn.microsoft.com/en-us/windows/win32/winauto/entry-uiauto-win32)
- [Microsoft Windows Graphics Capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)
- [Microsoft Windows camera/microphone privacy](https://support.microsoft.com/en-US/Windows/privacy/windows-camera-microphone-and-privacy)
- [Microsoft Known Folders](https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid)
- [Microsoft SmartScreen reputation for app developers](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation)
- [Ratatui installation and Crossterm default backend](https://ratatui.rs/installation/)
