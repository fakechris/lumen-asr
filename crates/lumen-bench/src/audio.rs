use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

pub const BENCHMARK_SAMPLE_RATE: u32 = 16_000;

pub fn decode_to_mono_f32(audio_path: &Path, ffmpeg: &Path) -> Result<Vec<f32>> {
    let output = Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(audio_path)
        .args([
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ac",
            "1",
            "-ar",
            "16000",
            "pipe:1",
        ])
        .output()
        .with_context(|| format!("run ffmpeg at {}", ffmpeg.display()))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffmpeg failed for {}: {}",
            audio_path.display(),
            message.trim()
        );
    }
    if output.stdout.len() % 4 != 0 {
        bail!("ffmpeg returned a partial f32 sample");
    }
    let samples: Vec<f32> = output
        .stdout
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    if samples.is_empty() {
        bail!("decoded audio is empty: {}", audio_path.display());
    }
    Ok(samples)
}
