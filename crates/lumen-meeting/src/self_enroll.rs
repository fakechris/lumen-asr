//! Self-enrollment support: turn one everyday recording into a single
//! voiceprint sample, so the user can register their own voice ("我") from the
//! dictation recordings they already made — no special "record yourself" step.
//!
//! The heavy work (WeSpeaker embedding) is macOS + `diarize`-gated like the
//! rest of the voiceprint stack; the caller (desktop) reads the WAV and does
//! the enrollment, this only measures voiced speech and embeds it.

use crate::preflight::scan_speech;
use crate::LiveVoiceprintEmbedder;

/// Embed the **voiced** portion of one recording into a single 256-d voiceprint
/// sample, returning it with the measured voiced duration.
///
/// Only the voiced spans are concatenated and embedded, so leading/trailing
/// silence and long pauses don't dilute the centroid. Returns `Ok(None)` when
/// the recording carries less than [`lumen_identity::MIN_VOICED_MS`] of voiced
/// speech (too little to trust) or the embedder rejects it — the caller skips
/// such recordings rather than enrolling a noisy sample.
pub fn embed_voiced_region(
    embedder: &mut LiveVoiceprintEmbedder,
    samples: &[f32],
    sample_rate: u32,
) -> Result<Option<(Vec<f32>, u64)>, String> {
    let scan = scan_speech(samples, sample_rate);
    let voiced_ms = (scan.voiced_seconds * 1000.0) as u64;
    if voiced_ms < lumen_identity::MIN_VOICED_MS {
        return Ok(None);
    }
    let mut voiced = Vec::new();
    for (start, end) in &scan.voiced_spans {
        let begin = (*start * sample_rate as f64).round() as usize;
        let end = ((*end * sample_rate as f64).round() as usize).min(samples.len());
        if begin < end {
            voiced.extend_from_slice(&samples[begin..end]);
        }
    }
    Ok(embedder
        .embed(&voiced, sample_rate)?
        .map(|embedding| (embedding, voiced_ms)))
}
