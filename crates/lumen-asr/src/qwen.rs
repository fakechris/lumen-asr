use crate::{AsrEngine, AsrError, AsrRequest, AsrResult};
use async_trait::async_trait;
use lumen_core::AsrEngineId;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const PRODUCT_WORKER: &str = include_str!("qwen_worker.py");
const MAX_WORKER_RESPONSE_BYTES: usize = 1024 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct QwenAsrConfig {
    pub python_executable: PathBuf,
    pub worker_script: PathBuf,
    pub model_dir: PathBuf,
    pub language: Option<String>,
    pub timeout: Duration,
    /// Test/development worker flags. Product callers leave this empty.
    pub extra_args: Vec<String>,
}

impl QwenAsrConfig {
    pub fn product(
        python_executable: impl Into<PathBuf>,
        model_dir: impl Into<PathBuf>,
        language: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            python_executable: python_executable.into(),
            worker_script: PathBuf::new(),
            model_dir: model_dir.into(),
            language,
            timeout,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct QwenAsr {
    config: QwenAsrConfig,
    worker: Arc<Mutex<Option<QwenWorker>>>,
    active: Arc<AtomicBool>,
    lifecycle_generation: Arc<AtomicU64>,
}

impl QwenAsr {
    pub fn new(config: QwenAsrConfig) -> Self {
        Self {
            config,
            worker: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(true)),
            lifecycle_generation: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn model_dir(&self) -> &Path {
        &self.config.model_dir
    }

    pub fn python_executable(&self) -> &Path {
        &self.config.python_executable
    }

    pub fn activate(&self) {
        self.lifecycle_generation.fetch_add(1, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
    }

    /// Release the loaded model when the user switches to another ASR engine.
    ///
    /// If a request is in flight, it finishes normally and releases the model
    /// before another request can reuse the worker.
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

    async fn start_worker(&self) -> Result<QwenWorker, AsrError> {
        let mut command = Command::new(&self.config.python_executable);
        command.arg("-u");
        if self.config.worker_script.as_os_str().is_empty() {
            command.arg("-c").arg(PRODUCT_WORKER);
        } else {
            command.arg(&self.config.worker_script);
        }
        command.arg("--model").arg(&self.config.model_dir);
        if let Some(language) = self
            .config
            .language
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            command.arg("--language").arg(language);
        }
        command.args(&self.config.extra_args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| AsrError::NotConfigured(format!("Qwen worker: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AsrError::Inference("Qwen worker stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AsrError::Inference("Qwen worker stdout unavailable".into()))?;
        Ok(QwenWorker {
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
    ) -> Result<WorkerResponse, AsrError> {
        let result = self
            .transcribe_path_inner(generation, request_id, path)
            .await;
        if self.lifecycle_generation.load(Ordering::SeqCst) != generation
            && !self.active.load(Ordering::SeqCst)
        {
            let mut guard = self.worker.lock().await;
            if let Some(mut worker) = guard.take() {
                let _ = worker.child.kill().await;
            }
        }
        result
    }

    async fn transcribe_path_inner(
        &self,
        generation: u64,
        request_id: u64,
        path: &Path,
    ) -> Result<WorkerResponse, AsrError> {
        let mut guard = self.worker.lock().await;
        if self.lifecycle_generation.load(Ordering::SeqCst) != generation
            || !self.active.load(Ordering::SeqCst)
        {
            return Err(AsrError::NotConfigured(
                "Qwen engine was deselected before transcription started".into(),
            ));
        }
        if guard.is_none() {
            *guard = Some(self.start_worker().await?);
        }
        let worker = guard.as_mut().expect("worker initialized");
        let request = WorkerRequest {
            id: request_id,
            audio_path: path.display().to_string(),
        };
        let mut encoded = serde_json::to_vec(&request)
            .map_err(|error| AsrError::Inference(format!("encode Qwen request: {error}")))?;
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
                    "Qwen worker exited",
                ));
            }
            if bytes > MAX_WORKER_RESPONSE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Qwen worker response exceeded 1 MiB",
                ));
            }
            Ok::<Vec<u8>, std::io::Error>(line)
        };

        let line = match tokio::time::timeout(self.config.timeout, exchange).await {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                let _ = worker.child.kill().await;
                *guard = None;
                return Err(AsrError::Inference(format!("Qwen worker I/O: {error}")));
            }
            Err(_) => {
                let _ = worker.child.kill().await;
                *guard = None;
                return Err(AsrError::Inference(format!(
                    "Qwen worker timed out after {}s",
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
                    "invalid Qwen response: {error}"
                )));
            }
        };
        if response.id != request_id {
            if let Some(worker) = guard.as_mut() {
                let _ = worker.child.kill().await;
            }
            *guard = None;
            return Err(AsrError::Inference(format!(
                "Qwen response id mismatch: expected {request_id}, got {}",
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
        Ok(response)
    }
}

#[async_trait]
impl AsrEngine for QwenAsr {
    fn id(&self) -> AsrEngineId {
        AsrEngineId::Qwen3Asr
    }

    async fn transcribe(&self, req: AsrRequest) -> Result<AsrResult, AsrError> {
        if req.samples.is_empty() {
            return Err(AsrError::EmptyAudio);
        }
        let generation = self.lifecycle_generation.load(Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(AsrError::NotConfigured(
                "Qwen engine is not active".into(),
            ));
        }
        let request_id = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let audio_file = tokio::task::spawn_blocking(move || {
            let mut audio_file = tempfile::Builder::new()
                .prefix("lumen-qwen-")
                .suffix(".wav")
                .tempfile()
                .map_err(|error| AsrError::Inference(format!("create Qwen audio: {error}")))?;
            write_wav_mono_i16(&mut audio_file, &req.samples, req.sample_rate)
                .and_then(|_| audio_file.flush())
                .map_err(|error| AsrError::Inference(format!("write Qwen audio: {error}")))?;
            Ok::<_, AsrError>(audio_file)
        })
        .await
        .map_err(|error| AsrError::Inference(format!("prepare Qwen audio task: {error}")))??;
        let response = self
            .transcribe_path(generation, request_id, audio_file.path())
            .await?;
        Ok(AsrResult {
            text: response.text.unwrap_or_default(),
            engine: self.id(),
            language: response.language,
        })
    }
}

#[derive(Serialize)]
struct WorkerRequest {
    id: u64,
    audio_path: String,
}

#[derive(Deserialize)]
struct WorkerResponse {
    id: u64,
    text: Option<String>,
    language: Option<String>,
    error: Option<String>,
}

struct QwenWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

fn schedule_worker_stop(mut worker: QwenWorker) {
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

fn write_wav_mono_i16(
    output: &mut impl Write,
    samples: &[f32],
    sample_rate: u32,
) -> std::io::Result<()> {
    let sample_rate = sample_rate.max(1);
    let data_len = samples.len().saturating_mul(2);
    let data_len_u32 = u32::try_from(data_len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Qwen WAV input exceeds the RIFF size limit",
        )
    })?;
    let mut output = std::io::BufWriter::new(output);
    output.write_all(b"RIFF")?;
    output.write_all(&36u32.saturating_add(data_len_u32).to_le_bytes())?;
    output.write_all(b"WAVEfmt ")?;
    output.write_all(&16u32.to_le_bytes())?;
    output.write_all(&1u16.to_le_bytes())?;
    output.write_all(&1u16.to_le_bytes())?;
    output.write_all(&sample_rate.to_le_bytes())?;
    output.write_all(&sample_rate.saturating_mul(2).to_le_bytes())?;
    output.write_all(&2u16.to_le_bytes())?;
    output.write_all(&16u16.to_le_bytes())?;
    output.write_all(b"data")?;
    output.write_all(&data_len_u32.to_le_bytes())?;
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        output.write_all(&value.to_le_bytes())?;
    }
    output.flush()
}
