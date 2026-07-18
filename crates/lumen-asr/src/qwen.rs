use crate::{AsrEngine, AsrError, AsrRequest, AsrResult};
use async_trait::async_trait;
use lumen_core::AsrEngineId;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const PRODUCT_WORKER: &str = include_str!("qwen_worker.py");
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
}

impl QwenAsr {
    pub fn new(config: QwenAsrConfig) -> Self {
        Self {
            config,
            worker: Arc::new(Mutex::new(None)),
        }
    }

    pub fn model_dir(&self) -> &Path {
        &self.config.model_dir
    }

    pub fn python_executable(&self) -> &Path {
        &self.config.python_executable
    }

    /// Release the loaded model when the user switches to another ASR engine.
    ///
    /// Engine selection is disabled while transcribing, so failure to acquire
    /// the worker lock only means an in-flight request owns it.
    pub fn unload(&self) -> bool {
        let Ok(mut guard) = self.worker.try_lock() else {
            return false;
        };
        if let Some(mut worker) = guard.take() {
            let _ = worker.child.start_kill();
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
        request_id: u64,
        path: &Path,
    ) -> Result<WorkerResponse, AsrError> {
        let mut guard = self.worker.lock().await;
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
            let mut line = String::new();
            let bytes = worker.stdout.read_line(&mut line).await?;
            if bytes == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Qwen worker exited",
                ));
            }
            Ok::<String, std::io::Error>(line)
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
        let response: WorkerResponse = match serde_json::from_str(&line) {
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
        let request_id = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut audio_file = tempfile::Builder::new()
            .prefix("lumen-qwen-")
            .suffix(".wav")
            .tempfile()
            .map_err(|error| AsrError::Inference(format!("create Qwen audio: {error}")))?;
        audio_file
            .write_all(&samples_to_wav_mono_i16(&req.samples, req.sample_rate))
            .and_then(|_| audio_file.flush())
            .map_err(|error| AsrError::Inference(format!("write Qwen audio: {error}")))?;
        let response = self.transcribe_path(request_id, audio_file.path()).await?;
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

fn samples_to_wav_mono_i16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let sample_rate = sample_rate.max(1);
    let data_len = samples.len().saturating_mul(2);
    let mut output = Vec::with_capacity(44 + data_len);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&(36u32.saturating_add(data_len as u32)).to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16u32.to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&sample_rate.to_le_bytes());
    output.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes());
    output.extend_from_slice(&2u16.to_le_bytes());
    output.extend_from_slice(&16u16.to_le_bytes());
    output.extend_from_slice(b"data");
    output.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        output.extend_from_slice(&value.to_le_bytes());
    }
    output
}
