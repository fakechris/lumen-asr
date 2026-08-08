//! Local Whisper via **mlx-whisper** (Apple Silicon Metal) — product worker
//! path, parallel to [`QwenAsr`].
//!
//! The sherpa-onnx Whisper engine remains available for tiny/CPU fallbacks;
//! large multi-lingual production work should use this MLX path.

use async_trait::async_trait;
use lumen_asr_engine::{
    model_identity_from_path, AsrEngine, AsrEngineId, AsrError, AsrRequest, AsrResult,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// Embedded worker script (same pattern as Qwen's PRODUCT_WORKER).
pub const PRODUCT_WORKER: &str = include_str!("mlx_whisper_worker.py");

const MAX_WORKER_RESPONSE_BYTES: usize = 1 << 20;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Default HF model id for multi-lingual production Whisper on MLX.
pub const DEFAULT_MLX_WHISPER_MODEL: &str = "mlx-community/whisper-large-v3-turbo";

#[derive(Debug, Clone)]
pub struct MlxWhisperConfig {
    pub python_executable: PathBuf,
    /// Empty → use embedded [`PRODUCT_WORKER`].
    pub worker_script: PathBuf,
    /// HF repo id or local snapshot path for mlx-whisper.
    pub model: String,
    pub language: Option<String>,
    pub timeout: Duration,
}

impl MlxWhisperConfig {
    pub fn product(
        python_executable: impl Into<PathBuf>,
        model: impl Into<String>,
        language: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            python_executable: python_executable.into(),
            worker_script: PathBuf::new(),
            model: model.into(),
            language,
            timeout,
        }
    }
}

#[derive(Clone)]
pub struct MlxWhisperAsr {
    config: MlxWhisperConfig,
    worker: Arc<Mutex<Option<Worker>>>,
    active: Arc<AtomicBool>,
    lifecycle_generation: Arc<AtomicU64>,
}

impl MlxWhisperAsr {
    pub fn new(config: MlxWhisperConfig) -> Self {
        Self {
            config,
            worker: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(true)),
            lifecycle_generation: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn python_executable(&self) -> &Path {
        &self.config.python_executable
    }

    pub fn activate(&self) {
        self.lifecycle_generation.fetch_add(1, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
    }

    pub fn unload(&self) -> bool {
        self.active.store(false, Ordering::SeqCst);
        self.lifecycle_generation.fetch_add(1, Ordering::SeqCst);
        let Ok(mut guard) = self.worker.try_lock() else {
            return true;
        };
        if let Some(worker) = guard.take() {
            schedule_worker_stop(worker);
        }
        true
    }

    async fn start_worker(&self) -> Result<Worker, AsrError> {
        let mut command = Command::new(&self.config.python_executable);
        command.arg("-u");
        if self.config.worker_script.as_os_str().is_empty() {
            command.arg("-c").arg(PRODUCT_WORKER);
        } else {
            command.arg(&self.config.worker_script);
        }
        command.arg("--model").arg(&self.config.model);
        if let Some(language) = self
            .config
            .language
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            command.arg("--language").arg(language);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| AsrError::NotConfigured(format!("mlx-whisper worker: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AsrError::Inference("mlx-whisper worker stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AsrError::Inference("mlx-whisper worker stdout unavailable".into()))?;
        Ok(Worker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn transcribe_path(
        &self,
        generation: u64,
        request_id: u64,
        path: &Path,
        language: Option<String>,
    ) -> Result<(WorkerResponse, bool), AsrError> {
        let mut guard = self.worker.lock().await;
        if self.lifecycle_generation.load(Ordering::SeqCst) != generation
            || !self.active.load(Ordering::SeqCst)
        {
            return Err(AsrError::NotConfigured(
                "mlx-whisper engine was deselected before transcription started".into(),
            ));
        }
        let worker_reused = guard.is_some();
        if !worker_reused {
            *guard = Some(self.start_worker().await?);
        }
        let worker = guard.as_mut().expect("worker initialized");
        let request = WorkerRequest {
            id: request_id,
            audio_path: path.display().to_string(),
            language,
        };
        let mut encoded = serde_json::to_vec(&request)
            .map_err(|error| AsrError::Inference(format!("encode mlx-whisper request: {error}")))?;
        encoded.push(b'\n');

        let exchange = async {
            worker.stdin.write_all(&encoded).await?;
            worker.stdin.flush().await?;
            let mut line = Vec::new();
            let bytes = (&mut worker.stdout)
                .take((MAX_WORKER_RESPONSE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
                .await?;
            if bytes == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "mlx-whisper worker exited",
                ));
            }
            if bytes > MAX_WORKER_RESPONSE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mlx-whisper worker response exceeded 1 MiB",
                ));
            }
            Ok::<Vec<u8>, std::io::Error>(line)
        };

        let line = match tokio::time::timeout(self.config.timeout, exchange).await {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                let _ = worker.child.kill().await;
                *guard = None;
                return Err(AsrError::Inference(format!(
                    "mlx-whisper worker I/O: {error}"
                )));
            }
            Err(_) => {
                let _ = worker.child.kill().await;
                *guard = None;
                return Err(AsrError::Inference(format!(
                    "mlx-whisper worker timed out after {}s",
                    self.config.timeout.as_secs()
                )));
            }
        };
        let response: WorkerResponse = match serde_json::from_slice(&line) {
            Ok(response) => response,
            Err(error) => {
                if let Some(worker) = guard.as_mut() {
                    let _ = worker.child.kill().await;
                }
                *guard = None;
                return Err(AsrError::Inference(format!(
                    "invalid mlx-whisper response: {error}"
                )));
            }
        };
        if response.id != request_id {
            if let Some(worker) = guard.as_mut() {
                let _ = worker.child.kill().await;
            }
            *guard = None;
            return Err(AsrError::Inference(format!(
                "mlx-whisper response id mismatch: expected {request_id}, got {}",
                response.id
            )));
        }
        if let Some(error) = response.error.as_deref().filter(|value| !value.is_empty()) {
            if let Some(worker) = guard.as_mut() {
                let _ = worker.child.kill().await;
            }
            *guard = None;
            return Err(AsrError::Inference(error.to_owned()));
        }
        Ok((response, worker_reused))
    }
}

#[async_trait]
impl AsrEngine for MlxWhisperAsr {
    fn id(&self) -> AsrEngineId {
        // Suite enum has no dedicated MLX variant yet; use Whisper id + label.
        AsrEngineId::Whisper
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        let generation = self.lifecycle_generation.load(Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(AsrError::NotConfigured(
                "mlx-whisper engine is not active".into(),
            ));
        }
        let request_id = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let language = req
            .language_hint
            .clone()
            .or_else(|| self.config.language.clone());
        let audio_file = tokio::task::spawn_blocking(move || {
            let mut audio_file = tempfile::Builder::new()
                .prefix("lumen-mlx-whisper-")
                .suffix(".wav")
                .tempfile()
                .map_err(|error| {
                    AsrError::Inference(format!("create mlx-whisper audio: {error}"))
                })?;
            lumen_asr_engine::audio::write_wav_mono_i16(
                &mut audio_file,
                &req.samples,
                req.sample_rate,
            )
            .and_then(|_| {
                use std::io::Write;
                audio_file.flush()
            })
            .map_err(|error| AsrError::Inference(format!("write mlx-whisper audio: {error}")))?;
            Ok::<_, AsrError>(audio_file)
        })
        .await
        .map_err(|error| {
            AsrError::Inference(format!("prepare mlx-whisper audio task: {error}"))
        })??;

        let (response, worker_reused) = self
            .transcribe_path(generation, request_id, audio_file.path(), language)
            .await?;

        let (model, model_revision) = model_identity_from_path(Path::new(&self.config.model));
        let mut result = AsrResult::new(response.text.unwrap_or_default(), self.id());
        result.engine_label = "mlx_whisper".into();
        result.language = response.language;
        result.diagnostics.worker_reused = Some(worker_reused);
        result.diagnostics.model = model.or_else(|| Some(self.config.model.clone()));
        result.diagnostics.model_revision = model_revision;
        Ok(result)
    }
}

#[derive(Serialize)]
struct WorkerRequest {
    id: u64,
    audio_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

#[derive(Deserialize)]
struct WorkerResponse {
    id: u64,
    text: Option<String>,
    language: Option<String>,
    error: Option<String>,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

fn schedule_worker_stop(mut worker: Worker) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _ = worker.child.kill().await;
        });
    } else {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(async move {
                    let _ = worker.child.kill().await;
                }),
                Err(_) => {
                    let _ = worker.child.start_kill();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_worker_mentions_mlx_whisper() {
        assert!(PRODUCT_WORKER.contains("mlx_whisper"));
        assert!(PRODUCT_WORKER.contains("audio_path"));
    }

    #[test]
    fn default_model_is_turbo() {
        assert!(DEFAULT_MLX_WHISPER_MODEL.contains("large-v3-turbo"));
    }
}
