//! Built-in LLM / ASR provider catalogs (endpoints + default models).
//!
//! LLM presets use OpenAI-compatible chat completions unless noted.
//! Cloud ASR: `openai_audio` + local engines are fully wired; other entries may be config-only.

use serde::Serialize;

// ── Corrector / LLM ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmProviderPreset {
    pub id: String,
    pub label: String,
    /// openai_compatible | ollama | none
    pub kind: String,
    pub base_url: String,
    pub default_model: String,
    /// Optional secondary models shown in UI.
    pub models: Vec<String>,
    pub needs_api_key: bool,
    pub notes: String,
}

pub fn llm_presets() -> Vec<LlmProviderPreset> {
    vec![
        LlmProviderPreset {
            id: "ollama".into(),
            label: "Ollama（本地）".into(),
            kind: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            default_model: "qwen3.5:9b".into(),
            models: vec!["qwen3.5:9b".into(), "qwen2.5:7b".into(), "llama3.2".into()],
            needs_api_key: false,
            notes: "自动注入 think:false 关闭思考链；需本机 ollama serve".into(),
        },
        LlmProviderPreset {
            id: "lm_studio".into(),
            label: "LM Studio（本地）".into(),
            kind: "openai_compatible".into(),
            base_url: "http://127.0.0.1:1234/v1".into(),
            default_model: "local-model".into(),
            models: vec![],
            needs_api_key: false,
            notes: "OpenAI 兼容本地服务".into(),
        },
        LlmProviderPreset {
            id: "openai".into(),
            label: "OpenAI".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.openai.com/v1".into(),
            default_model: "gpt-4o-mini".into(),
            models: vec![
                "gpt-4o-mini".into(),
                "gpt-4o".into(),
                "gpt-4.1-mini".into(),
                "gpt-4.1".into(),
            ],
            needs_api_key: true,
            notes: "官方 Chat Completions".into(),
        },
        LlmProviderPreset {
            id: "anthropic".into(),
            label: "Anthropic Claude（OpenAI 兼容网关）".into(),
            kind: "openai_compatible".into(),
            // Many gateways expose Claude as OpenAI-compatible; native /messages later.
            base_url: "https://api.anthropic.com/v1".into(),
            default_model: "claude-sonnet-4-6".into(),
            models: vec![
                "claude-sonnet-4-6".into(),
                "claude-opus-4-6".into(),
                "claude-3-5-haiku-latest".into(),
            ],
            needs_api_key: true,
            notes: "Anthropic Messages API 可后续增强；当前可按兼容层填写".into(),
        },
        LlmProviderPreset {
            id: "gemini".into(),
            label: "Google Gemini".into(),
            kind: "openai_compatible".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            default_model: "gemini-2.0-flash".into(),
            models: vec!["gemini-2.0-flash".into(), "gemini-2.5-flash".into()],
            needs_api_key: true,
            notes: "Google OpenAI-compatible endpoint".into(),
        },
        LlmProviderPreset {
            id: "deepseek".into(),
            label: "DeepSeek".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            default_model: "deepseek-chat".into(),
            models: vec!["deepseek-chat".into(), "deepseek-v4-flash".into()],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "qwen".into(),
            label: "通义千问（DashScope）".into(),
            kind: "openai_compatible".into(),
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
            default_model: "qwen-turbo".into(),
            models: vec![
                "qwen-turbo".into(),
                "qwen-plus".into(),
                "qwen3.5-flash".into(),
                "qwen3.6-flash".into(),
            ],
            needs_api_key: true,
            notes: "阿里云兼容模式".into(),
        },
        LlmProviderPreset {
            id: "zhipu".into(),
            label: "智谱 GLM".into(),
            kind: "openai_compatible".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            default_model: "glm-4-flash".into(),
            models: vec!["glm-4-flash".into(), "glm-4.7".into(), "glm-5".into()],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "kimi".into(),
            label: "Kimi（Moonshot）".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            default_model: "kimi-k2.5".into(),
            models: vec!["kimi-k2.5".into(), "kimi-k2.6".into(), "moonshot-v1-8k".into()],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "minimax".into(),
            label: "MiniMax".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.minimaxi.com/v1".into(),
            // Dictation: M3 + thinking disabled ≈ 3× faster end-to-end than M2.7-highspeed
            // (M2.x cannot turn off CoT; "highspeed" still burns hundreds of think tokens).
            default_model: "MiniMax-M3".into(),
            models: vec![
                "MiniMax-M3".into(),
                "MiniMax-M2.7".into(),
                "MiniMax-M2.7-highspeed".into(),
                "MiniMax-M2.5".into(),
            ],
            needs_api_key: true,
            notes: "听写请用 MiniMax-M3（请求侧自动 thinking=OFF）。M2.x/highspeed 无法关思考，纠错会慢且浪费 token。".into(),
        },
        LlmProviderPreset {
            id: "volcengine".into(),
            label: "火山引擎 / 豆包".into(),
            kind: "openai_compatible".into(),
            base_url: "https://ark.cn-beijing.volces.com/api/v3".into(),
            default_model: "doubao-seed-2-0-lite".into(),
            models: vec![
                "doubao-seed-2-0-lite".into(),
                "doubao-seed-2-0-lite-260428".into(),
            ],
            needs_api_key: true,
            notes: "方舟 ARK API Key；模型名为接入点 ID 或公开模型名".into(),
        },
        LlmProviderPreset {
            id: "siliconflow".into(),
            label: "硅基流动".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.siliconflow.cn/v1".into(),
            default_model: "deepseek-ai/DeepSeek-V3.2".into(),
            models: vec![
                "deepseek-ai/DeepSeek-V3.2".into(),
                "Qwen/Qwen2.5-7B-Instruct".into(),
            ],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            kind: "openai_compatible".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: "openai/gpt-4o-mini".into(),
            models: vec![
                "openai/gpt-4o-mini".into(),
                "anthropic/claude-sonnet-4".into(),
            ],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "stepfun".into(),
            label: "阶跃星辰 StepFun".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.stepfun.com/v1".into(),
            default_model: "step-3.7-flash".into(),
            models: vec!["step-3.7-flash".into(), "step-3.7".into()],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "mimo".into(),
            label: "小米 MiMo".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.xiaomimimo.com/v1".into(),
            default_model: "mimo-v2.5".into(),
            models: vec!["mimo-v2.5".into()],
            needs_api_key: true,
            notes: String::new(),
        },
        LlmProviderPreset {
            id: "openai_compatible".into(),
            label: "自定义 OpenAI 兼容".into(),
            kind: "openai_compatible".into(),
            base_url: "https://api.example.com/v1".into(),
            default_model: String::new(),
            models: vec![],
            needs_api_key: true,
            notes: "任意 /v1/chat/completions 兼容服务".into(),
        },
        LlmProviderPreset {
            id: "none".into(),
            label: "关闭（仅规则预处理）".into(),
            kind: "none".into(),
            base_url: String::new(),
            default_model: String::new(),
            models: vec![],
            needs_api_key: false,
            notes: String::new(),
        },
    ]
}

pub fn llm_preset_by_id(id: &str) -> Option<LlmProviderPreset> {
    llm_presets().into_iter().find(|p| p.id == id)
}

// ── Cloud ASR ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrProviderPreset {
    pub id: String,
    pub label: String,
    /// local | openai_audio | websocket | http_batch
    pub kind: String,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub needs_api_key: bool,
    /// wired | config_only（UI 可选，云端实现逐步补齐）
    pub status: String,
    pub notes: String,
}

pub fn asr_presets() -> Vec<AsrProviderPreset> {
    vec![
        AsrProviderPreset {
            id: "local_sensevoice".into(),
            label: "本地 SenseVoice".into(),
            kind: "local".into(),
            base_url: String::new(),
            default_model: "sensevoice-small".into(),
            models: vec!["sensevoice-small".into()],
            needs_api_key: false,
            status: "wired".into(),
            notes: "默认；离线 sherpa-onnx".into(),
        },
        AsrProviderPreset {
            id: "local_qwen".into(),
            label: "本地 Qwen3-ASR 0.6B 8-bit（实验）".into(),
            kind: "local".into(),
            base_url: String::new(),
            default_model: "Qwen3-ASR-0.6B-8bit".into(),
            models: vec!["Qwen3-ASR-0.6B-8bit".into()],
            needs_api_key: false,
            status: "wired".into(),
            notes: "实验性离线 MLX 引擎；模型常驻复用，识别后沿用当前文本整理流程。".into(),
        },
        AsrProviderPreset {
            id: "local_whisper".into(),
            label: "本地 Whisper".into(),
            kind: "local".into(),
            base_url: String::new(),
            default_model: "whisper".into(),
            models: vec![],
            needs_api_key: false,
            status: "wired".into(),
            notes: "若本机已配置 whisper 目录".into(),
        },
        AsrProviderPreset {
            id: "openai_audio".into(),
            label: "OpenAI Audio / Whisper".into(),
            kind: "openai_audio".into(),
            base_url: "https://api.openai.com/v1".into(),
            default_model: "whisper-1".into(),
            models: vec!["whisper-1".into(), "gpt-4o-mini-transcribe".into()],
            needs_api_key: true,
            status: "wired".into(),
            notes: "POST /audio/transcriptions；文件批式".into(),
        },
        AsrProviderPreset {
            id: "aliyun_qwen".into(),
            label: "阿里云 Qwen ASR".into(),
            kind: "websocket".into(),
            base_url: "wss://dashscope.aliyuncs.com/api-ws/v1/realtime".into(),
            default_model: "qwen-audio-asr".into(),
            models: vec![],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "DashScope realtime WebSocket；流式客户端待接".into(),
        },
        AsrProviderPreset {
            id: "volcengine".into(),
            label: "火山引擎 ASR".into(),
            kind: "websocket".into(),
            base_url: String::new(),
            default_model: String::new(),
            models: vec![],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "需要 app_id + access_token；流式客户端待接".into(),
        },
        AsrProviderPreset {
            id: "soniox".into(),
            label: "Soniox".into(),
            kind: "websocket".into(),
            base_url: "wss://stt-rt.soniox.com/transcribe-websocket".into(),
            default_model: "stt-rt-v4".into(),
            models: vec!["stt-rt-v4".into()],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "实时 WebSocket；流式客户端待接".into(),
        },
        AsrProviderPreset {
            id: "stepfun".into(),
            label: "阶跃星辰 StepFun ASR".into(),
            kind: "http_batch".into(),
            base_url: "https://api.stepfun.com/v1".into(),
            default_model: "stepaudio-2.5-asr".into(),
            models: vec!["stepaudio-2.5-asr".into()],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "批式 HTTP；模型 stepaudio-2.5-asr".into(),
        },
        AsrProviderPreset {
            id: "mimo".into(),
            label: "小米 MiMo ASR".into(),
            kind: "http_batch".into(),
            base_url: "https://api.xiaomimimo.com/v1".into(),
            default_model: "mimo-v2.5-asr".into(),
            models: vec!["mimo-v2.5-asr".into()],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "批式；audio/wav".into(),
        },
        AsrProviderPreset {
            id: "custom".into(),
            label: "自定义 ASR".into(),
            kind: "openai_audio".into(),
            base_url: String::new(),
            default_model: String::new(),
            models: vec![],
            needs_api_key: true,
            status: "config_only".into(),
            notes: "兼容 OpenAI transcriptions 形状的第三方".into(),
        },
    ]
}

pub fn asr_preset_by_id(id: &str) -> Option<AsrProviderPreset> {
    asr_presets().into_iter().find(|p| p.id == id)
}
