# ASR 上下文能力与引擎选型

> 定位：Lumen 上游 ASR 引擎与服务的工程选型，不是竞品分析。
>
> 核验日期：2026-07-13。云服务接口和模型版本变化较快，接入前必须按具体模型 ID、区域和 endpoint 重新验证。

## 结论

Lumen 需要把“上下文辅助 ASR”拆成三种能力，不能因为一个模型使用了 Transformer 或被称为“大模型”，就假定它能接收任意文本上下文：

1. **热词/词表偏置**：传入少量词或短语，提高专有名词、产品名、人名和缩写的识别概率。
2. **前文续接**：传入上一段转写或最近若干轮会话，改善跨分段连贯性、标点和同音词判断。
3. **自由文本 Prompt**：把领域说明、窗口标题、光标附近正文或页面主题与音频一起输入模型。这才接近 Vision LLM 的“模态输入 + 文本 Prompt”。

当前主流专用 ASR 公共接口主要提供第一类；第三类更多出现在 Audio/Omni LLM，或少数明确开放 `prompt` 的新一代转写模型中。

对 Lumen 的近期选择建议是：

- 保留 SenseVoiceSmall 作为低成本、本地、隐私优先的 audio-only baseline；不要把整段窗口文本强行塞给它。
- 从已采集上下文生成小型动态热词表，供支持 hotword 的引擎消费。
- 在明确支持自由文本的 ASR 上验证短 `prompt`，首选 OpenAI-compatible transcription prompt；同时评估 Qwen3.5-Omni 或明确支持 Prompt 的 Fun-ASR 版本。
- ASR 上下文与当前后置 LLM corrector 上下文保持两个独立开关和预算，默认均不应因为采集开启而自动外发。
- 选型必须通过消融实验，不以厂商“上下文理解”宣传代替接口和效果证据。

## 能力定义

### 热词偏置

典型执行路径：

```text
音频 → 声学/语音模型 → token 概率或候选
                         + 热词权重
                              ↓
                         最终转写
```

热词适合动态实体，不适合承载窗口全文。词表过长或权重过高会产生误植：上下文里出现、但用户没有说出的词被强行识别出来。

### 前文续接

前文可以是上一音频分段的转写，也可以是服务端维护的最近若干轮会话。它能改善跨分段语义，但不等于业务方可以注入任意屏幕正文。

### 自由文本 Prompt

真正的文本条件识别通常需要模型和接口同时具备文本输入通道：

```text
音频特征或音频 token ─┐
                       ├→ Cross Attention / Multimodal Transformer → 转写
文本 Prompt token ─────┘
```

SenseVoice、Paraformer、Conformer 等模型即使使用注意力或 Transformer，也可能只有音频输入通道。架构使用 Transformer 不是支持文本 Prompt 的充分条件。

## 引擎与服务矩阵

| 引擎或服务 | 动态热词 | 自由文本 Prompt | 前文/会话上下文 | Lumen 选型判断 |
|---|---|---|---|---|
| 豆包 BigASR | 支持词表和请求内热词 | 公共 BigASR 文档未证明支持任意正文 | RTC 接入支持指定最近会话轮次 | 可作为热词/会话上下文引擎；不能把 `context` 字段当作自由 Prompt |
| SenseVoiceSmall | 官方标准推理路径未开放；存在第三方扩展 | 不支持 | 不支持 | 本地 audio-only baseline，继续配合后置 corrector |
| Fun-ASR-Nano | 支持 `hotwords` | 公开标准用法未证明支持任意段落 | 流式状态不等于外部文本 | 值得作为本地/私有化热词模型测试 |
| Qwen3-ASR 专用模型 | 资料和版本存在差异 | 当前标准开源推理未稳定暴露 | 未明确 | 必须按目标模型快照和 endpoint 实测，不能泛化承诺 |
| Qwen3.5-Omni | 可通过 Prompt 给出术语和背景 | 支持 | 支持多轮消息 | 最接近 Vision LLM 的音频 + 文本条件模型，需评估延迟、成本和幻觉 |
| Fun-ASR Flash Prompt 版本 | 官方选型页标为 Prompt 上下文 | 支持的具体范围需实测 | 未明确 | 候选非实时 Prompt ASR |
| 讯飞大模型 ASR | 支持控制台或会话级热词 | 公共参数未发现任意正文 Prompt | 未发现通用外部前文参数 | 适合热词和领域参数，不按 Audio LLM 设计 |
| GPT-4o Transcribe | Prompt 可包含术语 | 支持自由文本 | 支持续接上一段 | 当前最明确的专用 ASR Prompt API 候选 |
| Azure Speech | 支持运行时 phrase list 和权重 | 不属于自由段落 Prompt | 取决于具体服务 | 典型传统动态词表参考 |

## 各服务的接口判断

### 豆包 BigASR

豆包文档中的 `context` 名称容易产生误解。公开请求示例是 JSON 字符串形式的热词或纠错词：

```json
{
  "context": "{\"hotwords\":[{\"word\":\"Lumen\"},{\"word\":\"SenseVoice\"}]}"
}
```

因此这个字段当前应归类为请求内 hotword/correction 容器，而不是任意窗口正文 Prompt。RTC/语音聊天接入另有 `context_history_length`，可把最近指定轮数的会话送入 ASR；它证明模型可以利用会话历史，但不证明普通 BigASR 请求可以直接接收业务方的完整屏幕上下文。

工程结论：豆包适配器应消费 `hotwords`；若以后接入其 RTC 会话能力，再独立映射 `previous_transcript`，不要把两者混成一个字符串。

官方资料：

- [豆包 RTC 流式 ASR 参数：context 与 context_history_length](https://www.volcengine.com/docs/6348/1807452?lang=zh)
- [豆包热词概述与限制](https://www.volcengine.com/docs/6561/155739?lang=zh)
- [豆包大模型流式识别 SDK context 示例](https://www.volcengine.com/docs/6561/1395846?lang=zh)

### SenseVoiceSmall 与 Fun-ASR-Nano

SenseVoiceSmall 是非自回归端到端语音理解模型。官方标准推理主要输入音频、语言和 ITN 等选项，没有开放任意文本 Prompt。官方仓库列出的 SenseVoice Hotword、CPPN 等属于第三方工作，不应当成原版 SenseVoiceSmall 的稳定接口。

Fun-ASR-Nano 属于 LLM-based ASR，官方示例明确支持：

```python
model.generate(
    input=[wav_path],
    hotwords=["Lumen", "Accessibility", "Context Capture"],
)
```

它是测试“本地 ASR + 动态上下文术语”的更直接候选，但现有公开示例仍是词组列表，不是任意窗口段落。

官方资料：

- [SenseVoice 官方仓库](https://github.com/FunAudioLLM/SenseVoice)
- [Fun-ASR-Nano 官方用法](https://github.com/FunAudioLLM/Fun-ASR/blob/main/README.md)

### Qwen3-ASR、Fun-ASR 与 Qwen3.5-Omni

千问体系需要按具体产品线区分：

- 当前百炼选型文档把 Qwen3-ASR 专用系列的“精度增强”标为不支持。
- 同一文档明确把 Qwen3.5-Omni 列为支持 Prompt 上下文的方案，并说明它不是传统 ASR，而是能理解音频的大语言模型。
- `fun-asr-flash-2026-06-15` 被列为支持 Prompt 上下文的非实时模型。
- 官方 Qwen3-ASR Toolkit 仍提供 `--context`，并把文本放入 system message、把音频放入 user message；但最新开源 Qwen3-ASR 标准 `transcribe()` 示例只暴露音频和语言。

这些资料表明能力与模型版本、云端 endpoint 和调用协议相关。Lumen 不应定义笼统的“Qwen 支持上下文”，而应为每个具体 provider/model 声明并验证 capability。

官方资料：

- [阿里云百炼 ASR 模型选型与 Prompt 上下文说明](https://help.aliyun.com/zh/model-studio/asr-model/)
- [Qwen3-ASR 官方仓库](https://github.com/QwenLM/Qwen3-ASR)
- [Qwen3-ASR Toolkit 的 context 参数](https://github.com/QwenLM/Qwen3-ASR-Toolkit)
- [Qwen-Audio 音频 + 文本消息格式](https://help.aliyun.com/zh/model-studio/audio-language-model)

### 讯飞大模型 ASR

讯飞公开的大模型转写接口支持控制台个性化热词、会话级热词和 `pd` 领域参数，例如科技、医疗、金融等。目前公开请求参数没有证明可以把任意当前窗口正文作为自由文本 Prompt。

工程结论：按 hotword/domain provider 接入；若未来出现明确的 text prompt 参数，再提升 capability，不根据“大模型”名称提前假设。

官方资料：

- [讯飞实时语音转写大模型](https://www.xfyun.cn/doc/spark/asr_llm/rtasr_llm.html)
- [讯飞语音转写会话级热词](https://www.xfyun.cn/doc/asr/lfasr/API.html)

### GPT-4o Transcribe

OpenAI transcription API 明确提供 `prompt`，用途包括指导输出风格和续接上一音频分段。Realtime transcription 对 GPT-4o Transcribe 系列还明确允许自由文本提示，例如说明预期出现科技领域词汇；Whisper 的 prompt 语义更接近关键词列表。

这是 Lumen 现有 OpenAI-compatible batch transcription adapter 最直接的 Prompt 实验目标。

官方资料：

- [OpenAI Audio transcription API](https://platform.openai.com/docs/api-reference/audio/speech-audio-done-event?lang=curl)
- [OpenAI Realtime transcription prompt](https://platform.openai.com/docs/api-reference/realtime-server-events/conversation/item/input_audio_transcription/completed?lang=node)

## Lumen 当前接入状态

Lumen 的公共请求已经有 `hotwords` 字段，但当前调用点始终传空数组：

- [`AsrRequest.hotwords`](../../crates/lumen-asr/src/lib.rs#L46)
- [桌面听写请求当前传 `hotwords: vec![]`](../../apps/desktop/src-tauri/src/dictation.rs#L860)

现有 OpenAI-compatible adapter 只上传 `file`、`model` 和可选 `language`，还没有上传 `prompt`：

- [OpenAI-compatible transcription adapter](../../crates/lumen-asr/src/cloud_openai.rs#L42)

现有 SenseVoice adapter 只把音频交给 sherpa-onnx，`AsrRequest.hotwords` 不会进入 recognizer：

- [SenseVoice decode path](../../crates/lumen-asr/src/sensevoice.rs#L115)

另外，配置中预置的 `aliyun_qwen`、`volcengine` 等 provider 目前只有 endpoint 选项，完整流式客户端尚未接入。因此“配置里出现 provider 名”不等于已经具备相应上下文能力。

## 建议的 Lumen 抽象

采集层继续保存完整、结构化的 `ContextManifest`；进入 ASR 前增加独立、受预算约束的投影，而不是把 AX/DOM/OCR tree 直接发给 provider：

```rust
pub struct AsrContextProjection {
    /// 10～30 个去重后的动态实体或短语。
    pub hotwords: Vec<String>,

    /// 约 200～500 字的领域、主题和光标附近正文。
    pub prompt: Option<String>,

    /// 最近 1～3 次用户口述或上一分段转写。
    pub previous_transcript: Option<String>,
}

pub struct AsrContextCapabilities {
    pub hotwords: bool,
    pub free_text_prompt: bool,
    pub previous_transcript: bool,
    pub weighted_phrases: bool,
    pub max_prompt_chars: Option<usize>,
}
```

provider adapter 只消费自己明确声明的字段：

```text
SenseVoiceSmall
  └─ 暂不消费 ASR context；使用 audio-only + 后置 LLM corrector

Fun-ASR-Nano / 豆包 / 讯飞
  └─ hotwords

GPT-4o Transcribe / 明确支持 Prompt 的 Qwen 或 Fun-ASR
  └─ prompt + previous_transcript；必要时再附 hotwords
```

建议新增与 `[context].send_to_corrector` 分离的开关，例如：

```toml
[context]
enabled = true
send_to_corrector = false
send_to_asr = false
```

`enabled` 只控制本地采集；`send_to_asr` 和 `send_to_corrector` 分别控制两个不同的外发边界。

## 上下文投影建议

### Hotword 投影

优先选择：

1. 当前选中文字中的实体；
2. 光标附近的产品名、人名、缩写、代码标识符；
3. 页面或窗口标题中的低频术语；
4. 用户个人词典；
5. 最近几次口述中稳定出现的实体。

初始限制建议为 10～30 个去重词组。不要把整段 OCR 或 Visible Text 按空格切开后全部当作热词。

### Prompt 投影

初始预算建议为 200～500 个 Unicode 字符，按以下顺序组合：

1. 一句领域或任务描述；
2. 光标前后最邻近的句子；
3. 选中文字；
4. 少量候选术语；
5. 上一段转写。

不发送截图、坐标、OCR box、AX/DOM tree、来源诊断、完整 URL、不可见页面全文或 secure field 内容。

ASR prompt 的预算应明显小于后置 corrector 当前最多 2,000 字符的窗口上下文预算，因为 ASR 的首要任务是听写；过多可见文本会增加误植和延迟风险。

## 选型实验

固定同一批音频、同一上下文快照和同一模型版本，至少比较：

| 条件 | ASR 输入 | Corrector 输入 | 回答的问题 |
|---|---|---|---|
| A | 音频 | 关闭 | 纯 ASR baseline |
| B | 音频 + 动态热词 | 关闭 | 词表是否提高实体召回 |
| C | 音频 + 短 Prompt | 关闭 | 自由文本是否改善声学歧义 |
| D | 音频 | 安全窗口上下文 | 现有后置修正收益 |
| E | 音频 + 最佳 ASR context | 安全窗口上下文 | 两阶段组合收益与重复风险 |

至少记录：

- Content CER/WER；
- 专有实体 precision、recall 和 exact match；
- 数字、版本、路径和中英混合 exact match；
- 上下文误植率：模型输出了上下文存在、但音频没有说出的词；
- 幻觉、遗漏和否定反转；
- 首 token/最终延迟、成本和失败率；
- 不同 provider 对 prompt 长度和热词数量的敏感性。

任何模型接入前都必须做 A/B/C 消融；只有 E 变好不能证明 ASR context 本身有效，因为收益可能全部来自后置 corrector。

## 当前决策

1. **保留 SenseVoiceSmall**：继续作为默认本地 baseline，不为自由 Prompt 改造其标准推理路径。
2. **实现 provider capability，而不是全局布尔假设**：不同模型精确声明 hotwords、prompt 和 previous transcript 支持。
3. **优先打通现有 OpenAI-compatible `prompt`**：代码改动最小，可直接验证自由文本条件 ASR。
4. **并行评估 Fun-ASR-Nano 热词**：回答“上下文抽取成实体是否已经足够”的问题。
5. **把 Qwen3.5-Omni/Prompt Fun-ASR 作为 Audio LLM 候选**：先做非实时效果、误植和延迟评测，再决定是否进入产品路径。
6. **豆包和讯飞先按热词 provider 设计**：没有明确自由 Prompt 证据前，不把完整窗口正文发送到其 `context` 或 hotword 字段。
7. **所有 ASR context 外发默认关闭**：本地采集保持独立，用户显式开启后才发送。

这份文档记录的是当前可验证的接口能力和 Lumen 接入决策。实际模型质量与每种上下文的贡献仍需由上述消融实验确定。
