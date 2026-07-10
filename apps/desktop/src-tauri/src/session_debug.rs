//! Per-session debug dumps: raw audio (WAV) + ASR / corrected text.
//!
//! Layout:
//!   ~/Library/Application Support/LumenAsr/debug/YYYYMMDD-HHMMSS-<id8>/
//!     meta.json
//!     audio_16k.wav
//!     asr.txt
//!     corrected.txt

use lumen_platform::default_data_dir;
use serde::Serialize;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDebugMeta {
    pub session_id: String,
    pub created_at_unix_ms: u128,
    pub target_app: Option<String>,
    pub target_bundle_id: Option<String>,
    pub frontmost_before_insert: Option<String>,
    pub sample_rate_capture: u32,
    pub num_samples_capture: usize,
    pub sample_rate_asr: u32,
    pub num_samples_asr: usize,
    pub duration_ms: u64,
    pub rms: f32,
    pub peak: f32,
    pub asr_engine: String,
    pub corrector_engine: String,
    pub asr_text: String,
    pub corrected_text: String,
    pub insert_strategy: String,
    pub insert_ok: bool,
    pub notes: Vec<String>,
}

pub fn debug_root() -> PathBuf {
    default_data_dir().join("debug")
}

pub fn new_session_dir(session_id: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let short = session_id.chars().take(8).collect::<String>();
    let dir = debug_root().join(format!("{ts}-{short}"));
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn write_session_debug(
    dir: &Path,
    meta: &SessionDebugMeta,
    samples_16k: &[f32],
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let wav_path = dir.join("audio_16k.wav");
    write_wav_f32_as_i16(&wav_path, samples_16k, 16_000)?;

    fs::write(dir.join("asr.txt"), &meta.asr_text).map_err(|e| e.to_string())?;
    fs::write(dir.join("corrected.txt"), &meta.corrected_text).map_err(|e| e.to_string())?;

    let json = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    fs::write(dir.join("meta.json"), json).map_err(|e| e.to_string())?;

    // Rolling pointer for latest dump.
    let _ = fs::write(
        debug_root().join("LATEST.txt"),
        format!("{}\n", dir.display()),
    );

    tracing::info!(
        dir = %dir.display(),
        samples = samples_16k.len(),
        rms = meta.rms,
        peak = meta.peak,
        asr = %meta.asr_text,
        target = ?meta.target_app,
        "session debug written"
    );
    Ok(())
}

pub fn audio_stats(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum = 0.0f32;
    let mut peak = 0.0f32;
    for &s in samples {
        let a = s.abs();
        sum += s * s;
        if a > peak {
            peak = a;
        }
    }
    let rms = (sum / samples.len() as f32).sqrt();
    (rms, peak)
}

/// Read a PCM16 mono WAV written by [`write_wav_f32_as_i16`] (or equivalent).
pub fn read_wav_mono_f32(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let bytes = fs::read(path).map_err(|e| format!("read audio: {e}"))?;
    if bytes.len() < 44 {
        return Err("audio file too short".into());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    // Walk chunks after RIFF header (offset 12)
    let mut i = 12usize;
    let mut sample_rate = 16_000u32;
    let mut data_off = None;
    let mut data_len = 0usize;
    let mut bits = 16u16;
    let mut channels = 1u16;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
            sample_rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
        } else if id == b"data" {
            data_off = Some(body);
            data_len = size.min(bytes.len().saturating_sub(body));
            break;
        }
        i = body + size + (size % 2); // word align
    }
    let data_off = data_off.ok_or_else(|| "WAV missing data chunk".to_string())?;
    if bits != 16 {
        return Err(format!("unsupported WAV bits={bits} (need 16)"));
    }
    if channels == 0 {
        return Err("invalid channel count".into());
    }
    let frame = 2 * channels as usize;
    let n_frames = data_len / frame;
    let mut samples = Vec::with_capacity(n_frames);
    for f in 0..n_frames {
        let mut acc = 0.0f32;
        for c in 0..channels as usize {
            let o = data_off + f * frame + c * 2;
            if o + 2 > bytes.len() {
                break;
            }
            let v = i16::from_le_bytes([bytes[o], bytes[o + 1]]);
            acc += v as f32 / 32768.0;
        }
        samples.push(acc / channels as f32);
    }
    Ok((samples, sample_rate))
}

/// Minimal PCM16 mono WAV writer (no extra crate).
fn write_wav_f32_as_i16(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    let mut f = File::create(path).map_err(|e| e.to_string())?;
    let n = samples.len() as u32;
    let data_bytes = n.saturating_mul(2);
    let file_size_minus_8 = 36u32.saturating_add(data_bytes);

    // RIFF header
    f.write_all(b"RIFF").map_err(|e| e.to_string())?;
    f.write_all(&file_size_minus_8.to_le_bytes())
        .map_err(|e| e.to_string())?;
    f.write_all(b"WAVE").map_err(|e| e.to_string())?;

    // fmt chunk
    f.write_all(b"fmt ").map_err(|e| e.to_string())?;
    f.write_all(&16u32.to_le_bytes()).map_err(|e| e.to_string())?; // chunk size
    f.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?; // PCM
    f.write_all(&1u16.to_le_bytes()).map_err(|e| e.to_string())?; // mono
    f.write_all(&sample_rate.to_le_bytes())
        .map_err(|e| e.to_string())?;
    let byte_rate = sample_rate * 2;
    f.write_all(&byte_rate.to_le_bytes())
        .map_err(|e| e.to_string())?;
    f.write_all(&2u16.to_le_bytes()).map_err(|e| e.to_string())?; // block align
    f.write_all(&16u16.to_le_bytes()).map_err(|e| e.to_string())?; // bits

    // data chunk
    f.write_all(b"data").map_err(|e| e.to_string())?;
    f.write_all(&data_bytes.to_le_bytes())
        .map_err(|e| e.to_string())?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        f.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}
