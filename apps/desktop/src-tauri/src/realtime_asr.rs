//! Realtime (streaming) cloud ASR sessions for dictation: audio streams up
//! while the user speaks, partial transcripts stream back, and the final
//! text is ready ~immediately after speech ends — no post-stop batch round
//! trip and no LLM correction pass (hotwords bias the recognizer itself).
//!
//! Two providers today:
//! - OpenAI Realtime **transcription session** (`gpt-realtime-transcribe`):
//!   WebSocket JSON events, `input_audio_buffer.append` with base64 pcm16,
//!   server VAD, per-utterance `transcription.completed` (plus `partial`
//!   events when the session emits them).
//! - Volcengine（豆包）大模型流式 ASR (`sauc/bigmodel`): the binary v3
//!   WebSocket protocol — 4-byte header + JSON payloads, per-packet
//!   responses, hotwords in the initial full request, punctuation + ITN on
//!   by default, final signaled by a negative-sequence audio packet.
//!
//! MiniMax has no realtime ASR (file batch + "fake streaming" SSE only) and
//! is deliberately unsupported here.
//!
//! Lifecycle: `RealtimeSession::open` → `send_audio` (≈100 ms pcm16 chunks)
//! → `finish` (end-of-stream, drain to the final text). `next_event`
//! surfaces partials for live UI between sends.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

/// Providers with a realtime implementation.
pub const REALTIME_PROVIDERS: [&str; 2] = ["openai_audio", "volcengine"];

pub fn provider_supports_realtime(provider: &str) -> bool {
    REALTIME_PROVIDERS.contains(&provider)
}

/// Target PCM16 sample rate the provider expects.
fn target_rate(provider: &str) -> u32 {
    // OpenAI Realtime pcm16 input is 24 kHz mono; Volcengine sauc is 16 kHz.
    match provider {
        "openai_audio" => 24_000,
        _ => 16_000,
    }
}

#[derive(Debug, Clone)]
pub struct RealtimeConfig {
    pub provider: String,
    pub api_key: String,
    /// Volcengine 旧版控制台 App ID（新版控制台留空）.
    pub app_id: String,
    pub base_url: String,
    /// Optional BCP-47 language hint.
    pub language: String,
    /// Hotwords biasing the recognizer (replaces the LLM correction pass).
    pub hotwords: Vec<String>,
    pub model: String,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            provider: "openai_audio".into(),
            api_key: String::new(),
            app_id: String::new(),
            base_url: String::new(),
            language: String::new(),
            hotwords: Vec::new(),
            model: String::new(),
        }
    }
}

/// One streamed update from the recognizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    /// Live partial text (the current utterance so far).
    Partial { text: String },
    /// A finalized utterance; more may follow.
    Utterance { text: String },
    /// The session is done; no further events.
    Done,
    /// The provider reported an error.
    Failed { message: String },
}

/// Result of a finished session.
#[derive(Debug, Clone, Default)]
pub struct RealtimeOutcome {
    /// All finalized utterances joined with a space (Chinese included — the
    /// providers already emit punctuation).
    pub text: String,
    /// The last partial snapshot (empty when every utterance finalized).
    pub trailing_partial: String,
}

/// A live realtime session. Drop closes the socket (best-effort).
pub struct RealtimeSession {
    provider: String,
    /// Writer half owned by the caller; reader runs in a task.
    writer: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    events: mpsc::Receiver<RealtimeEvent>,
    /// Volcengine only: the last on-wire message sequence. The initial full
    /// request consumes `1`; audio packets continue from `2`.
    seq: i32,
}

impl RealtimeSession {
    pub async fn open(config: &RealtimeConfig) -> Result<Self, String> {
        match config.provider.as_str() {
            "openai_audio" => Self::open_openai(config).await,
            "volcengine" => Self::open_volcengine(config).await,
            other => Err(format!("provider {other} has no realtime ASR")),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        target_rate(&self.provider)
    }

    /// Send one chunk of interleaved-free mono PCM16 little-endian bytes.
    pub async fn send_audio(&mut self, pcm16: &[u8]) -> Result<(), String> {
        if pcm16.is_empty() {
            return Ok(());
        }
        match self.provider.as_str() {
            "openai_audio" => {
                let audio = base64_encode(pcm16);
                self.writer
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "input_audio_buffer.append",
                            "audio": audio,
                        })
                        .to_string(),
                    ))
                    .await
                    .map_err(|e| format!("ws send: {e}"))
            }
            "volcengine" => {
                self.seq += 1;
                let msg = encode_audio_packet(pcm16, self.seq);
                self.writer
                    .send(Message::Binary(msg))
                    .await
                    .map_err(|e| format!("ws send: {e}"))
            }
            _ => unreachable!("constructor validated the provider"),
        }
    }

    /// End the audio stream and drain events until the final text arrives
    /// (or the deadline passes). Returns the assembled outcome.
    pub async fn finish(mut self) -> Result<RealtimeOutcome, String> {
        match self.provider.as_str() {
            "openai_audio" => {
                // Commit flushes the trailing utterance immediately (the
                // server VAD would otherwise wait out its silence window).
                // The socket is NOT closed yet: closing first truncates the
                // committed response still in flight.
                self.writer
                    .send(Message::Text(
                        serde_json::json!({ "type": "input_audio_buffer.commit" }).to_string(),
                    ))
                    .await
                    .map_err(|e| format!("ws commit: {e}"))?;
            }
            "volcengine" => {
                // End-of-stream: an empty packet whose sequence is the
                // NEGATED next sequence (live-verified: after packet 74 the
                // server expects -75, not -1).
                let last = encode_last_packet(-(self.seq + 1));
                self.writer
                    .send(Message::Binary(last))
                    .await
                    .map_err(|e| format!("ws send last: {e}"))?;
            }
            _ => unreachable!(),
        }
        let mut outcome = RealtimeOutcome::default();
        let mut text_parts: Vec<String> = Vec::new();
        // First event allowance, then idle-between-events: once the provider
        // has said something, a quiet window means it is finished (closing
        // early would truncate the committed trailing utterance).
        // Before the first event: a generous deadline. After at least one
        // utterance: a short idle window bounds the NEXT wait — an arrived
        // event is always processed first, never discarded.
        let first_deadline = Duration::from_secs(10);
        let idle = Duration::from_secs(2);
        loop {
            let wait = if text_parts.is_empty() {
                first_deadline
            } else {
                idle
            };
            let event = match tokio::time::timeout(wait, self.events.recv()).await {
                Ok(event) => event,
                Err(_) => {
                    if text_parts.is_empty() {
                        return Err("realtime ASR timed out waiting for the transcript".into());
                    }
                    break;
                }
            };
            let Some(event) = event else { break };
            match event {
                RealtimeEvent::Partial { text } => outcome.trailing_partial = text,
                RealtimeEvent::Utterance { text } => {
                    if !text.trim().is_empty() {
                        text_parts.push(text.trim().to_string());
                    }
                }
                RealtimeEvent::Done => break,
                RealtimeEvent::Failed { message } => return Err(message),
            }
        }
        outcome.text = text_parts.join(" ");
        Ok(outcome)
    }

    // ── OpenAI Realtime transcription session ───────────────────────────

    async fn open_openai(config: &RealtimeConfig) -> Result<Self, String> {
        let base = if config.base_url.trim().is_empty() {
            "wss://api.openai.com/v1/realtime".to_string()
        } else {
            let trimmed = config.base_url.trim().trim_end_matches('/');
            if trimmed.starts_with("wss://") {
                // An explicit realtime endpoint passes through untouched.
                trimmed.to_string()
            } else {
                // A batch REST base URL (e.g. https://api.openai.com/v1) maps
                // onto the realtime endpoint by host only — keeping the /v1
                // path would produce wss://…/v1/v1/realtime.
                let host = trimmed
                    .strip_prefix("https://")
                    .or_else(|| trimmed.strip_prefix("http://"))
                    .unwrap_or(trimmed)
                    .split('/')
                    .next()
                    .unwrap_or(trimmed);
                format!("wss://{host}/v1/realtime")
            }
        };
        // GA: the session model must be a realtime model; the transcription
        // model rides inside audio.input.transcription (gpt-live-transcribe).
        let model = if config.model.trim().is_empty() {
            "gpt-realtime-mini"
        } else {
            config.model.trim()
        };
        let url = format!("{base}?model={model}");
        let mut request = url.into_client_request().map_err(|e| format!("url: {e}"))?;
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            format!("Bearer {}", config.api_key.trim())
                .parse()
                .map_err(|_| "invalid authorization header".to_string())?,
        );
        // GA endpoints negotiate the "realtime" WS subprotocol.
        headers.insert("Sec-WebSocket-Protocol", "realtime".parse().unwrap());

        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("realtime connect: {e}"))?;
        let (writer, mut reader) = ws.split();
        let (tx, rx) = mpsc::channel(64);

        // Configure the session: pcm16 input, server VAD, transcription
        // model + language/prompt biasing (hotwords ride the prompt).
        let mut transcription = serde_json::json!({ "model": "gpt-live-transcribe" });
        if !config.language.trim().is_empty() {
            transcription["language"] = serde_json::json!(config.language.trim());
        }
        if !config.hotwords.is_empty() {
            transcription["prompt"] = serde_json::json!(config.hotwords.join(", "));
        }
        // GA schema: audio.input.{format,transcription,turn_detection};
        // create_response=false keeps the session transcription-only (no
        // synthesized audio output, no response latency or cost).
        let session_update = serde_json::json!({
            "type": "session.update",
            "session": {
                "type": "realtime",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": 24000 },
                        "transcription": transcription,
                        "turn_detection": { "type": "server_vad", "create_response": false },
                    }
                }
            }
        });
        let mut writer = writer;
        writer
            .send(Message::Text(session_update.to_string()))
            .await
            .map_err(|e| format!("session update send: {e}"))?;

        tokio::spawn(async move {
            while let Some(frame) = reader.next().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = tx
                            .send(RealtimeEvent::Failed {
                                message: format!("ws read: {error}"),
                            })
                            .await;
                        break;
                    }
                };
                let text = match frame {
                    Message::Text(text) => text.to_string(),
                    Message::Close(_) => {
                        let _ = tx.send(RealtimeEvent::Done).await;
                        break;
                    }
                    _ => continue,
                };
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "session.created"
                    | "session.updated"
                    | "transcription_session.created"
                    | "transcription_session.updated" => {}
                    "input_audio_buffer.speech_started" => {}
                    "input_audio_buffer.speech_stopped" => {}
                    "conversation.item.input_audio_transcription.partial" => {
                        if let Some(t) = value.get("transcript").and_then(|v| v.as_str()) {
                            let _ = tx
                                .send(RealtimeEvent::Partial {
                                    text: t.to_string(),
                                })
                                .await;
                        }
                    }
                    "conversation.item.input_audio_transcription.completed" => {
                        if let Some(t) = value.get("transcript").and_then(|v| v.as_str()) {
                            let _ = tx
                                .send(RealtimeEvent::Utterance {
                                    text: t.to_string(),
                                })
                                .await;
                        }
                    }
                    "conversation.item.input_audio_transcription.failed" => {
                        let error = value
                            .pointer("/error/message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("transcription failed");
                        let _ = tx
                            .send(RealtimeEvent::Failed {
                                message: error.to_string(),
                            })
                            .await;
                        break;
                    }
                    "error" => {
                        let error = value
                            .pointer("/error/message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown realtime error");
                        let _ = tx
                            .send(RealtimeEvent::Failed {
                                message: error.to_string(),
                            })
                            .await;
                        break;
                    }
                    _ => {}
                }
            }
            let _ = tx.send(RealtimeEvent::Done).await;
        });

        Ok(RealtimeSession {
            provider: "openai_audio".into(),
            writer,
            events: rx,
            seq: 0,
        })
    }

    // ── Volcengine sauc bigmodel (binary v3) ────────────────────────────

    async fn open_volcengine(config: &RealtimeConfig) -> Result<Self, String> {
        // The config's base_url for volcengine is the batch HTTP endpoint
        // (…/auc/bigmodel/recognize/flash) — useless for WebSocket. Only an
        // explicit wss:// override replaces the default streaming endpoint.
        let base = match config.base_url.trim() {
            url if url.starts_with("wss://") => url.to_string(),
            _ => "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel".to_string(),
        };
        let mut request = base
            .into_client_request()
            .map_err(|e| format!("url: {e}"))?;
        let headers = request.headers_mut();
        headers.insert(
            "X-Api-Resource-Id",
            "volc.bigasr.sauc.duration".parse().unwrap(),
        );
        headers.insert(
            "X-Api-Request-Id",
            uuid::Uuid::new_v4().to_string().parse().unwrap(),
        );
        if config.app_id.trim().is_empty() {
            headers.insert(
                "X-Api-Key",
                config
                    .api_key
                    .trim()
                    .parse()
                    .map_err(|_| "invalid key header".to_string())?,
            );
        } else {
            headers.insert("X-Api-App-Key", config.app_id.trim().parse().unwrap());
            headers.insert(
                "X-Api-Access-Key",
                config
                    .api_key
                    .trim()
                    .parse()
                    .map_err(|_| "invalid key header".to_string())?,
            );
        }

        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("realtime connect: {e}"))?;
        let (writer, mut reader) = ws.split();
        let (tx, rx) = mpsc::channel(64);

        let mut request_json = serde_json::json!({
            "user": { "uid": "lumen-dictation" },
            "audio": { "format": "pcm", "codec": "raw", "rate": 16000, "bits": 16, "channel": 1 },
            "request": {
                "model_name": "bigmodel",
                "enable_punc": true,
                "enable_itn": true,
                "result_type": "full",
                "enable_ddc": false,
                "show_utterances": true,
            }
        });
        if !config.hotwords.is_empty() {
            request_json["request"]["hotwords"] = serde_json::json!(config
                .hotwords
                .iter()
                .map(|w| serde_json::json!({ "word": w }))
                .collect::<Vec<_>>());
        }
        let mut writer = writer;
        writer
            .send(Message::Binary(encode_full_request(
                &request_json.to_string(),
            )))
            .await
            .map_err(|e| format!("full request send: {e}"))?;

        tokio::spawn(async move {
            while let Some(frame) = reader.next().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = tx
                            .send(RealtimeEvent::Failed {
                                message: format!("ws read: {error}"),
                            })
                            .await;
                        break;
                    }
                };
                let bytes = match frame {
                    Message::Binary(bytes) => bytes,
                    Message::Close(_) => {
                        let _ = tx.send(RealtimeEvent::Done).await;
                        break;
                    }
                    _ => continue,
                };
                let parsed = match decode_server_frame(&bytes) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        let _ = tx.send(RealtimeEvent::Failed { message: error }).await;
                        break;
                    }
                };
                match parsed.kind {
                    ServerFrameKind::Error => {
                        let _ = tx
                            .send(RealtimeEvent::Failed {
                                message: parsed.error,
                            })
                            .await;
                        break;
                    }
                    ServerFrameKind::Result => {
                        // `result.text` is the cumulative transcript (the
                        // utterances array stays non-definite until the
                        // end), so it drives the live partial; the final
                        // frame's text is the definitive transcript.
                        let text = parsed
                            .payload
                            .pointer("/result/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if parsed.is_last {
                            if !text.trim().is_empty() {
                                let _ = tx.send(RealtimeEvent::Utterance { text }).await;
                            }
                            let _ = tx.send(RealtimeEvent::Done).await;
                            break;
                        } else if !text.trim().is_empty() {
                            let _ = tx.send(RealtimeEvent::Partial { text }).await;
                        }
                    }
                }
            }
            let _ = tx.send(RealtimeEvent::Done).await;
        });

        Ok(RealtimeSession {
            provider: "volcengine".into(),
            writer,
            events: rx,
            seq: 1,
        })
    }
}

// ── Volcengine binary v3 framing ─────────────────────────────────────

const PROTOCOL_VERSION: u8 = 0b0001;
const HEADER_SIZE: u8 = 0b0001;
const MSG_FULL_CLIENT: u8 = 0b0001;
const MSG_AUDIO_ONLY: u8 = 0b0010;
const MSG_FULL_SERVER: u8 = 0b1001;
const MSG_ERROR_SERVER: u8 = 0b1011;
/// Live-observed error frame type (decode-failure diagnostics).
const MSG_LAST_SERVER: u8 = 0b1111;
const FLAG_SEQ: u8 = 0b0001;
const FLAG_LAST: u8 = 0b0010;
const FLAG_GZIP: u8 = 0b0100;
const SERIAL_JSON: u8 = 0b0001;
const COMPRESSION_NONE: u8 = 0b0000;

fn header_byte0() -> u8 {
    (PROTOCOL_VERSION << 4) | HEADER_SIZE
}

/// The initial full client request: header + 4-byte size + JSON payload
/// (no sequence — the whole request is one shot).
fn encode_full_request(json: &str) -> Vec<u8> {
    let payload = json.as_bytes();
    let mut frame = Vec::with_capacity(4 + 4 + payload.len());
    frame.push(header_byte0());
    frame.push(MSG_FULL_CLIENT << 4);
    frame.push((SERIAL_JSON << 4) | COMPRESSION_NONE);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// An audio-only packet: header | sequence | payload size | payload. The
/// sequence increments per message on the wire — the initial full request
/// consumed `1`, so audio packets start at `2`. `sequence < 0` marks the
/// final packet.
fn encode_audio_packet(pcm16: &[u8], sequence: i32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + 4 + 4 + pcm16.len());
    frame.push(header_byte0());
    frame.push((MSG_AUDIO_ONLY << 4) | FLAG_SEQ);
    // Audio-only packets carry raw payload (no serialization byte meaning;
    // byte2 keeps 0 per protocol examples).
    frame.push(0);
    frame.push(0);
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&(pcm16.len() as u32).to_be_bytes());
    frame.extend_from_slice(pcm16);
    frame
}

/// The end-of-stream marker: an empty audio packet carrying the LAST flag
/// on top of the normal sequence.
fn encode_last_packet(sequence: i32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(12);
    frame.push(header_byte0());
    frame.push((MSG_AUDIO_ONLY << 4) | FLAG_SEQ | FLAG_LAST);
    frame.push(0);
    frame.push(0);
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame
}

#[derive(Debug, PartialEq)]
enum ServerFrameKind {
    Result,
    Error,
}

struct ServerFrame {
    kind: ServerFrameKind,
    is_last: bool,
    payload: serde_json::Value,
    error: String,
}

fn decode_server_frame(bytes: &[u8]) -> Result<ServerFrame, String> {
    if bytes.len() < 8 {
        return Err(format!("short server frame ({} bytes)", bytes.len()));
    }
    let msg_type = bytes[1] >> 4;
    let flags = bytes[1] & 0x0f;
    let compression = bytes[2] & 0x0f;
    // The 0xF diagnostics frame does not follow the size framing at all (its
    // length word carries junk); handle it before any size math.
    if msg_type == MSG_LAST_SERVER {
        let start = bytes
            .iter()
            .skip(4)
            .position(|b| *b == b'{')
            .map(|p| p + 4)
            .ok_or("error frame without a JSON payload")?;
        let payload: serde_json::Value = serde_json::from_slice(&bytes[start..])
            .map_err(|_| "malformed server frame payload".to_string())?;
        let error = payload
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("provider error")
            .to_string();
        return Ok(ServerFrame {
            kind: ServerFrameKind::Error,
            is_last: true,
            payload,
            error,
        });
    }
    // Server frames put the sequence BEFORE the payload size (verified
    // against the live ACK frame: header | seq | size | payload), the
    // opposite of the client audio packet layout.
    let (payload_size, _sequence) = if flags & FLAG_SEQ != 0 {
        if bytes.len() < 12 {
            return Err("short sequenced server frame".into());
        }
        let seq = i32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let size = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        (size, seq)
    } else {
        (
            u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize,
            0,
        )
    };
    let offset = if flags & FLAG_SEQ != 0 { 12 } else { 8 };
    if bytes.len() < offset + payload_size {
        return Err("truncated server frame payload".into());
    }
    let kind = if msg_type == MSG_ERROR_SERVER || msg_type == MSG_LAST_SERVER {
        ServerFrameKind::Error
    } else {
        ServerFrameKind::Result
    };
    // 0xB error frames prefix the JSON with a length word; parse from the
    // first '{' so both shapes decode.
    let mut payload_bytes = &bytes[offset..offset + payload_size];
    if kind == ServerFrameKind::Error {
        if let Some(start) = payload_bytes.iter().position(|b| *b == b'{') {
            payload_bytes = &payload_bytes[start..];
        }
    }
    let payload: serde_json::Value = if compression == FLAG_GZIP {
        // Gzip-inflating without a new dependency: reuse flate2 via the
        // workspace? Keep it simple — the sauc responses we consume are
        // small; we request no compression. Fail loudly if it ever appears.
        return Err("gzip server frames are not expected here".into());
    } else {
        serde_json::from_slice(payload_bytes)
            .map_err(|_| "malformed server frame payload".to_string())?
    };
    let error = payload
        .get("error")
        .or_else(|| payload.get("status_message"))
        .or_else(|| payload.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("provider error")
        .to_string();
    Ok(ServerFrame {
        kind,
        is_last: flags & FLAG_LAST != 0,
        payload,
        error,
    })
}

// Suppress dead-code warnings for framing constants used only in tests.
const _: (u8, u8) = (MSG_FULL_SERVER, SERIAL_JSON);

/// Standard-alphabet base64 (shared shape with volcengine_asr; kept local so
/// this module has no import coupling).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_request_frame_matches_the_documented_layout() {
        let frame = encode_full_request(r#"{"user":{"uid":"u"}}"#);
        assert_eq!(frame[0], 0x11);
        assert_eq!(frame[1], 0x10); // full client request, no flags
        let size = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
        assert_eq!(size as usize, frame.len() - 8);
        assert_eq!(&frame[8..], br#"{"user":{"uid":"u"}}"#);
    }

    #[test]
    fn audio_packet_carries_its_sequence_and_last_marker() {
        let frame = encode_audio_packet(b"abcd", 3);
        assert_eq!(frame[0], 0x11);
        assert_eq!(frame[1], 0x21); // audio-only + seq flag
                                    // Sequence first (offset 4), then the payload size — the live
                                    // protocol rejects the opposite order.
        assert_eq!(
            i32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]),
            3
        );
        let size = u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
        assert_eq!(size, 4);
        assert_eq!(&frame[12..], b"abcd");

        let last = encode_audio_packet(&[], -1);
        assert_eq!(i32::from_be_bytes([last[4], last[5], last[6], last[7]]), -1);
    }

    #[test]
    fn server_frames_decode_result_error_and_last_flag() {
        let payload = r#"{"result":{"text":"你好","utterances":[]}}"#.as_bytes().to_vec();
        // No-seq frame: header | size | payload.
        let mut frame = vec![0x11, 0x90, 0x10, 0x00];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        let parsed = decode_server_frame(&frame).unwrap();
        assert_eq!(parsed.kind, ServerFrameKind::Result);
        assert!(!parsed.is_last);
        assert_eq!(
            parsed
                .payload
                .pointer("/result/text")
                .and_then(|v| v.as_str()),
            Some("你好")
        );

        let mut last = vec![0x11, 0x92, 0x10, 0x00];
        last.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        last.extend_from_slice(&payload);
        assert!(decode_server_frame(&last).unwrap().is_last);

        // Sequenced server frame: header | seq | size | payload (the live
        // ACK layout — sequence first, then the payload size).
        let mut seq_frame = vec![0x11, 0x91, 0x10, 0x00];
        seq_frame.extend_from_slice(&1i32.to_be_bytes());
        seq_frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        seq_frame.extend_from_slice(&payload);
        let parsed = decode_server_frame(&seq_frame).unwrap();
        assert_eq!(parsed.kind, ServerFrameKind::Result);
        assert_eq!(
            parsed
                .payload
                .pointer("/result/text")
                .and_then(|v| v.as_str()),
            Some("你好")
        );

        let err_payload = br#"{"status_code":45000001,"status_message":"bad param"}"#.to_vec();
        let mut err = vec![0x11, 0xB0, 0x10, 0x00];
        err.extend_from_slice(&(err_payload.len() as u32).to_be_bytes());
        err.extend_from_slice(&err_payload);
        let parsed = decode_server_frame(&err).unwrap();
        assert_eq!(parsed.kind, ServerFrameKind::Error);
        assert_eq!(parsed.error, "bad param");
    }

    #[test]
    fn last_packet_sets_the_last_flag_on_the_negated_sequence() {
        let last = encode_last_packet(-75);
        assert_eq!(last[0], 0x11);
        assert_eq!(last[1], 0x23); // audio-only + seq + last
        assert_eq!(
            i32::from_be_bytes([last[4], last[5], last[6], last[7]]),
            -75
        );
        assert_eq!(
            u32::from_be_bytes([last[8], last[9], last[10], last[11]]),
            0
        );
        assert_eq!(last.len(), 12);
    }

    #[test]
    fn realtime_support_matrix_matches_the_providers() {
        assert!(provider_supports_realtime("openai_audio"));
        assert!(provider_supports_realtime("volcengine"));
        assert!(!provider_supports_realtime("minimax"));
        assert!(!provider_supports_realtime("local_sensevoice"));
        assert_eq!(target_rate("openai_audio"), 24_000);
        assert_eq!(target_rate("volcengine"), 16_000);
    }

    #[test]
    fn base64_matches_standard_alphabet() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    /// Live smoke test (OpenAI): run with a real key and a spoken wav.
    /// Prep:  say -o /tmp/rt.aiff "你好，这是一段实时识别测试" && \
    ///        afconvert -f WAVE -d LEI16@24000 -c 1 /tmp/rt.aiff /tmp/rt.wav
    /// Run:   OPENAI_API_KEY=… REALTIME_WAV=/tmp/rt.wav \
    ///          cargo test -p lumen-asr-desktop realtime_openai_smoke --ignored -- --nocapture
    #[tokio::test]
    #[ignore = "live network test; needs OPENAI_API_KEY and REALTIME_WAV"]
    async fn realtime_openai_smoke() {
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
        let wav_path = std::env::var("REALTIME_WAV").expect("REALTIME_WAV");
        let pcm16 = std::fs::read(&wav_path).expect("wav");
        let data = wav_payload(&pcm16);
        let config = RealtimeConfig {
            provider: "openai_audio".into(),
            api_key: key,
            ..RealtimeConfig::default()
        };
        let mut session = RealtimeSession::open(&config).await.expect("open");
        assert_eq!(session.sample_rate(), 24_000);
        for chunk in data.chunks(4800) {
            session.send_audio(chunk).await.expect("send");
            // Realtime pacing (100 ms of audio per 60 ms tick keeps the
            // upload slightly ahead of playout). Events are left queued for
            // finish() to collect — consuming them here would starve it.
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let outcome = session.finish().await.expect("finish");
        println!("FINAL: [{}]", outcome.text);
        assert!(!outcome.text.trim().is_empty(), "expected a transcript");
    }

    /// Live smoke test (Volcengine): run with real credentials and a 16 kHz
    /// spoken wav. REALTIME_WAV must be 16 kHz mono pcm16 wav.
    #[tokio::test]
    #[ignore = "live network test; needs VOLC_APP_ID/VOLC_KEY and REALTIME_WAV"]
    async fn realtime_volcengine_smoke() {
        let key = std::env::var("VOLC_KEY").expect("VOLC_KEY");
        let app_id = std::env::var("VOLC_APP_ID").unwrap_or_default();
        let wav_path = std::env::var("REALTIME_WAV").expect("REALTIME_WAV");
        let pcm16 = std::fs::read(&wav_path).expect("wav");
        let data = wav_payload(&pcm16);
        let config = RealtimeConfig {
            provider: "volcengine".into(),
            api_key: key,
            app_id,
            ..RealtimeConfig::default()
        };
        let mut session = RealtimeSession::open(&config).await.expect("open");
        assert_eq!(session.sample_rate(), 16_000);
        // Drain any handshake error frames before streaming.
        tokio::time::sleep(Duration::from_millis(700)).await;
        while let Ok(event) = session.events.try_recv() {
            println!("pre-audio event: {event:?}");
        }
        for chunk in data.chunks(3200) {
            session.send_audio(chunk).await.expect("send");
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let outcome = session.finish().await.expect("finish");
        println!("FINAL: [{}]", outcome.text);
        assert!(!outcome.text.trim().is_empty(), "expected a transcript");
    }

    /// Skip a RIFF/WAVE header and return the data chunk (naive but fine for
    /// smoke-test fixtures produced by afconvert).
    fn wav_payload(bytes: &[u8]) -> Vec<u8> {
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]) as usize;
            if id == b"data" {
                let end = (offset + 8 + size).min(bytes.len());
                return bytes[offset + 8..end].to_vec();
            }
            offset += 8 + size + (size % 2);
        }
        panic!("no data chunk in wav fixture");
    }
}

// ── Streaming pump (tap → resample → session) ────────────────────────

/// Incremental linear resampler with cross-chunk phase continuity.
pub struct LinearResampler {
    in_rate: f64,
    out_rate: f64,
    last_input: f32,
    /// Fractional position between `last_input` and the next input sample,
    /// carried across chunks so concatenation is seamless.
    pos: f64,
    /// False until the first chunk primes `last_input`, so the very first
    /// output never interpolates against a phantom zero.
    primed: bool,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: f64::from(in_rate.max(1)),
            out_rate: f64::from(out_rate.max(1)),
            last_input: 0.0,
            pos: 0.0,
            primed: false,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() || self.in_rate == self.out_rate {
            return input.to_vec();
        }
        // Distance between output positions, in input-sample units: the
        // inverse ratio (in/out) — each input sample advances the phase by
        // one, and every `in/out` input samples one output is produced.
        let step = self.in_rate / self.out_rate;
        let mut out =
            Vec::with_capacity(((input.len() as f64) * self.out_rate / self.in_rate) as usize + 2);
        if !self.primed {
            if let Some(&first) = input.first() {
                self.last_input = first;
            }
            self.primed = true;
        }
        for &sample in input {
            // Emit every output position that falls before this input sample.
            while self.pos < 1.0 {
                out.push(self.last_input + (sample - self.last_input) * self.pos as f32);
                self.pos += step;
            }
            self.pos -= 1.0;
            self.last_input = sample;
        }
        out
    }
}

/// Live-partial callback (capsule preview); `None` disables it.
type PartialSink = Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>;

/// Handle to one live realtime dictation run. The owning task pumps the mic
/// tap into the session and finishes it on `end`.
pub struct RealtimeHandle {
    pub provider: String,
    pub started_at: std::time::Instant,
    /// Latest cumulative partial transcript (live preview).
    pub partial: std::sync::Arc<parking_lot::Mutex<String>>,
    end: tokio::sync::oneshot::Sender<()>,
    outcome: tokio::sync::oneshot::Receiver<Result<RealtimeOutcome, String>>,
}

impl RealtimeHandle {
    /// Signal end-of-speech and await the final transcript. Consumes the
    /// handle (the session cannot outlive its recording).
    pub async fn finish(
        self,
    ) -> Result<Result<RealtimeOutcome, String>, tokio::sync::oneshot::error::RecvError> {
        let _ = self.end.send(());
        self.outcome.await
    }
}

/// Spawn the pump task for one recording. `tap` yields device-rate mono
/// chunks; the task resamples to the provider rate, forwards ~100 ms PCM16
/// chunks, tracks the latest partial, and on `end` flushes and finishes the
/// session.
pub fn spawn_realtime_run(
    config: RealtimeConfig,
    tap: std::sync::mpsc::Receiver<lumen_asr::LiveChunk>,
    on_partial: PartialSink,
) -> RealtimeHandle {
    let provider = config.provider.clone();
    let partial = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let (end_tx, end_rx) = tokio::sync::oneshot::channel::<()>();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let partial_for_task = std::sync::Arc::clone(&partial);
    let collected = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let collected_for_task = std::sync::Arc::clone(&collected);
    let collected_for_merge = std::sync::Arc::clone(&collected_for_task);
    let on_partial = on_partial.clone();
    tokio::spawn(async move {
        let mut outcome = run_pump(
            config,
            tap,
            end_rx,
            partial_for_task,
            collected_for_task,
            on_partial,
        )
        .await;
        if let Ok(res) = outcome.as_mut() {
            // The recognizer finalized some sentences while the user was
            // still speaking (server VAD); merge them ahead of anything the
            // final drain produced — the two event sets are disjoint.
            let mut parts: Vec<String> = std::mem::take(&mut *collected_for_merge.lock());
            if !res.text.trim().is_empty() {
                parts.push(res.text.trim().to_string());
            }
            if parts.is_empty() && !res.trailing_partial.trim().is_empty() {
                // Speech stopped mid-utterance: the trailing partial is the
                // transcript.
                parts.push(res.trailing_partial.trim().to_string());
            }
            res.text = parts.join(" ");
        }
        let _ = outcome_tx.send(outcome);
    });
    RealtimeHandle {
        provider,
        started_at: std::time::Instant::now(),
        partial,
        end: end_tx,
        outcome: outcome_rx,
    }
}

async fn run_pump(
    config: RealtimeConfig,
    tap: std::sync::mpsc::Receiver<lumen_asr::LiveChunk>,
    mut end: tokio::sync::oneshot::Receiver<()>,
    partial: std::sync::Arc<parking_lot::Mutex<String>>,
    collected: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    on_partial: PartialSink,
) -> Result<RealtimeOutcome, String> {
    // Bridge the blocking std tap onto the async runtime with a dedicated
    // thread, so the pump never blocks a Tokio worker on recv_timeout for
    // the length of the recording.
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<lumen_asr::LiveChunk>(64);
    let bridge = std::thread::spawn(move || {
        while let Ok(chunk) = tap.recv() {
            if chunk_tx.blocking_send(chunk).is_err() {
                break;
            }
        }
    });
    let outcome = run_pump_inner(
        config,
        &mut chunk_rx,
        &mut end,
        partial,
        collected,
        on_partial,
    )
    .await;
    drop(chunk_rx);
    let _ = bridge.join();
    outcome
}

async fn run_pump_inner(
    config: RealtimeConfig,
    chunk_rx: &mut tokio::sync::mpsc::Receiver<lumen_asr::LiveChunk>,
    end: &mut tokio::sync::oneshot::Receiver<()>,
    partial: std::sync::Arc<parking_lot::Mutex<String>>,
    collected: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    on_partial: PartialSink,
) -> Result<RealtimeOutcome, String> {
    let mut session = match RealtimeSession::open(&config).await {
        Ok(session) => session,
        Err(error) => return Err(format!("realtime connect: {error}")),
    };
    let target_rate = session.sample_rate();
    let mut resampler = LinearResampler::new(44_100, target_rate);
    let mut pcm16_buffer: Vec<u8> = Vec::new();
    // ~100 ms of PCM16 per send (matches the providers' recommendation).
    let chunk_bytes = (target_rate as usize / 10) * 2;
    let mut input_rate = 44_100u32;

    loop {
        tokio::select! {
            chunk = chunk_rx.recv() => match chunk {
                Some(chunk) => {
                    if chunk.sample_rate > 0 {
                        input_rate = chunk.sample_rate;
                    }
                    if chunk.sample_rate != resampler_input_rate(&resampler) {
                        resampler = LinearResampler::new(input_rate, target_rate);
                    }
                    for sample in resampler.process(&chunk.samples) {
                        push_pcm16(&mut pcm16_buffer, sample);
                    }
                    while pcm16_buffer.len() >= chunk_bytes {
                        let rest = pcm16_buffer.split_off(chunk_bytes);
                        let chunk = std::mem::replace(&mut pcm16_buffer, rest);
                        if let Err(error) = session.send_audio(&chunk).await {
                            return Err(format!("realtime send: {error}"));
                        }
                    }
                }
                None => break, // capture stopped (tap dropped)
            },
            _ = &mut *end => break,
        }
        // Server VAD finalizes sentences mid-speech: keep them.
        while let Ok(event) = session.events.try_recv() {
            match event {
                RealtimeEvent::Partial { text } => {
                    let changed = {
                        let mut guard = partial.lock();
                        let changed = *guard != text;
                        *guard = text.clone();
                        changed
                    };
                    if changed {
                        if let Some(sink) = on_partial.as_ref() {
                            sink(&text);
                        }
                    }
                }
                RealtimeEvent::Utterance { text } => {
                    if !text.trim().is_empty() {
                        collected.lock().push(text.trim().to_string());
                    }
                }
                RealtimeEvent::Done => {}
                RealtimeEvent::Failed { message } => {
                    return Err(format!("realtime failed: {message}"));
                }
            }
        }
    }
    // Flush the tail and collect the final transcript.
    if !pcm16_buffer.is_empty() {
        if let Err(error) = session.send_audio(&pcm16_buffer).await {
            return Err(format!("realtime send tail: {error}"));
        }
    }
    session.finish().await
}

fn resampler_input_rate(resampler: &LinearResampler) -> u32 {
    // The resampler only needs its source rate for step computation; expose
    // it through a tiny helper to detect device-rate changes.
    resampler.in_rate as u32
}

fn push_pcm16(buffer: &mut Vec<u8>, sample: f32) {
    let clamped = sample.clamp(-1.0, 1.0);
    let value = (clamped * i16::MAX as f32).round() as i16;
    buffer.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod pump_tests {
    use super::*;

    #[test]
    fn resampler_output_length_matches_the_ratio() {
        let mut r = LinearResampler::new(44_100, 16_000);
        let out = r.process(&vec![0.5f32; 44_100]);
        // 1s in → ~1s out (±2 samples for phase carry).
        assert!((out.len() as i64 - 16_000).abs() < 2, "{}", out.len());

        let mut r = LinearResampler::new(44_100, 24_000);
        let out = r.process(&vec![0.5f32; 44_100]);
        assert!((out.len() as i64 - 24_000).abs() < 2, "{}", out.len());
    }

    #[test]
    fn resampler_is_continuous_across_chunks() {
        // Constant input stays constant regardless of chunking.
        let mut r = LinearResampler::new(44_100, 16_000);
        let a = r.process(&vec![0.25f32; 1_000]);
        let b = r.process(&vec![0.25f32; 1_000]);
        for sample in a.iter().chain(b.iter()) {
            assert!((sample - 0.25).abs() < 1e-6);
        }
    }

    #[test]
    fn pcm16_push_clamps_and_encodes_le() {
        let mut buffer = Vec::new();
        push_pcm16(&mut buffer, 0.0);
        push_pcm16(&mut buffer, 1.0);
        push_pcm16(&mut buffer, -2.0); // clamps to -1.0 → -32767 (x i16::MAX)
        assert_eq!(&buffer[0..2], &0i16.to_le_bytes());
        assert_eq!(&buffer[2..4], &i16::MAX.to_le_bytes());
        assert_eq!(&buffer[4..6], &(-32767i16).to_le_bytes());
    }
}
