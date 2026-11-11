# ASR 质量回归评测（`lumen-eval`）

一套固定的评测语料 + 归一化错误率 + 基线门禁，让听写引擎与清理规则的质量变化可测、可比、可拦截（INV-1805）。设计参考业界「合成 clip 集 + 指标门禁」的做法：**仓库只入库文本语料，音频在你本机合成**（macOS `say`，license 安全），模型用你已安装的本地引擎。

## 组成

| 位置 | 职责 |
|---|---|
| `crates/lumen-eval` | 纯指标库：归一化（全半角/大小写/中文数字→阿拉伯/英文数词/标点与语气词免计）、CER/WER（中文按字、英文按词、混合双语）、词条命中、清理检查（语气词残留/丢句/凭空发明/语言漂移）、说话人数守卫（防日记单说话人塌缩）、基线比对 |
| `crates/lumen-eval/data/asr-corpus.json` | 23 段语料：zh-filler(5) / zh-plain(3) / en(4) / mix(4) / dict(5) / long(2，由源 clip 以 0.8s 静音拼接) |
| `crates/lumen-eval/data/baselines.json` | 各引擎门禁阈值（当前为维护者机器上的 provisional 基线） |
| `lumen-asr-desktop eval` | CLI：`synth` / `asr` / `selftest` |

## 本地完整流程

```bash
cd apps/desktop && npm run build && cd -   # 或任意已构建状态
cargo build -p lumen-asr-desktop

# 1. 合成语料音频（macOS only；列出声音：say -v ?）
./target/debug/lumen-asr-desktop eval synth --out-dir eval-clips

# 2. 跑分（JSON 报告到 stdout；--out 另存文件）
./target/debug/lumen-asr-desktop eval asr --engine sensevoice --clips-dir eval-clips

# 3. 首次/引擎变化后：录制基线，review 后粘进 crates/lumen-eval/data/baselines.json
./target/debug/lumen-asr-desktop eval asr --engine sensevoice --clips-dir eval-clips \
  --record-baseline baselines.local.json
```

- 引擎：`sensevoice` | `qwen` | `whisper` | `mlx-whisper`（复用 `meeting process` 的引擎装配）。
- 退出码：`0` 通过（含 report-only），`1` 门禁超标，`2` 用法错误。
- 无基线的引擎是 report-only：只报数不拦截；`--set <name>` 可只跑某分集。

## 指标语义

- **错误率**：hypothesis 与该 clip 的**期望清理文本**（`clean`）做归一化后按 token（中文=字，英文=词）算编辑距离。语气词、标点、全半角、数字写法在归一化中免计；自我修正残留（如说完改口的“下周二”）计入——清理层应当去掉它们。
- **词条命中**：clip `terms` 里有多少在 hypothesis 中精确（归一化后）出现。
- **清理检查**：`must` 短语必须保留、`must_not` 必须消失；凭空发明 >1 个 token、中文占比漂移 >30 个百分点都算失败。原始 ASR 输出（无清理层）会自然残留语气词，所以 CLI 模式的基线把观测到的残留数记为 `allow_check_failures`；端到端（含清理）评测应为 0。
- **说话人数守卫**：clip 声明 `speakers: N` 时比对管线报告的发现数（INV-153 单说话人塌缩的回归防线）。

## CI

`ci-macos.yml` 运行 `cargo run -p lumen-asr-desktop -- eval selftest`：校验语料与基线文件，并注入两个故障（清理规则回归、说话人塌缩）证明门禁能拦。音频+模型的完整跑分在本地执行（CI 不装模型）。

## 新增 clip

在 `asr-corpus.json` 加一条：`text`（说出的原文）、`clean`（完美清理后的样子）、`must`/`must_not`（分语种短语，拉丁短语按词边界匹配）、`terms`（词典词条）、可选 `speakers`。长音频用 `source_clip_ids` + `gap_seconds` 引用其他 clip。改完跑 `eval selftest`——语料自洽性（must 在 clean 中、must_not 不在）有测试兜底。

## 当前基线要点（维护者机器，provisional）

- SenseVoice：zh-plain 最好（mean err ≈0.09 门限），en/mix/dict 中等；长音频是弱项。
- Qwen3-ASR (sherpa)：**>100s 拼接长音频错误率 ≈1.0（引擎只回 "language"）**——这正是 INV-1807（长口述停顿切段后台识别）要解决的问题，基线如实记录，改进会直接体现在该门限下。
