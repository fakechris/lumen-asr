//! Cross-track echo duplicate suppression for dual-track meetings.
//!
//! When the user is **not** wearing headphones, the remote side's voice plays
//! through the loudspeaker and is picked up again by the microphone. Each track
//! is diarized and transcribed independently, so the same remote utterance
//! shows up twice in the merged transcript: once on the system track (the real
//! copy) and once on the mic track (the acoustic echo). This module detects
//! those mic-side echo copies with **multiple independent pieces of evidence**
//! so the caller can drop them from the final take.
//!
//! A mic segment is suppressed only when **all** evidence agrees (better to
//! miss an echo than to delete real speech):
//!
//! 1. **Delay window** — `mic.start − system.start` within
//!    [[`ECHO_DELAY_MIN_S`], [`ECHO_DELAY_MAX_S`]]. Acoustically the echo
//!    arrives tens of milliseconds after playout, but segment-level ASR
//!    timestamps are coarse, so the window is generous.
//! 2. **Time coverage** — overlap / mic duration ≥ [`ECHO_MIN_COVERAGE`].
//! 3. **Text similarity** — only judged when the normalized mic text has at
//!    least [`ECHO_MIN_TEXT_CHARS`] characters (short backchannels like
//!    "好的" / "嗯" are never judged); normalized edit-distance similarity ≥
//!    [`ECHO_MIN_TEXT_SIMILARITY`] or one normalized text contains the other.
//! 4. **Audio cross-correlation** — the two WAVs' corresponding time windows
//!    (16 kHz mono, each in its own track's timeline) correlate with a
//!    normalized peak ≥ [`ECHO_XCORR_MIN_PEAK`] within ±[`ECHO_XCORR_MAX_LAG_S`]
//!    of lag.
//!
//! Timeline caveat: the two tracks are written by two independent recorders
//! and each WAV's timestamps count from its own first sample. The recorder
//! writes a `<meeting-id>.timeline.json` sidecar with each track's measured
//! start offset from the shared recording `t0`; [`read_timeline_skew`] turns
//! that into the system→mic start skew and the pass shifts the system
//! segments onto the mic timeline before pairing (and shifts back when
//! reading system audio windows). Without a sidecar (older or crash-recovered
//! meetings) the skew is `0.0` — the previous "near-common start" assumption.
//! The offsets are capture-start measurements, not sample-exact, so the
//! constants keep their generous tolerances; they can be tightened once the
//! offsets are derived from the audio itself.
//!
//! Everything IO-related **fails open**: when a WAV cannot be read or a window
//! is too short/silent to correlate, the audio evidence is *missing* and the
//! segment is kept — an IO problem must never delete real speech.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::assemble::DiarTurn;
use crate::merge::TrackTake;

/// Evidence 1: earliest the mic echo may start relative to the system copy.
/// Slightly negative because segment boundaries are ASR/diarization estimates,
/// not sample-accurate onsets.
pub(crate) const ECHO_DELAY_MIN_S: f64 = -0.25;

/// Evidence 1: latest the mic echo may start relative to the system copy.
pub(crate) const ECHO_DELAY_MAX_S: f64 = 0.75;

/// Evidence 2: minimum fraction of the mic segment covered by the system
/// segment's time range.
pub(crate) const ECHO_MIN_COVERAGE: f64 = 0.7;

/// Evidence 3: minimum normalized character count before text is judged at
/// all. Short backchannels ("好的", "对", "嗯", "ok") legitimately repeat
/// across both sides of a call, so they are never treated as text evidence.
pub(crate) const ECHO_MIN_TEXT_CHARS: usize = 6;

/// Evidence 3: minimum normalized edit-distance similarity (1 − distance /
/// longer length) between the two normalized texts.
pub(crate) const ECHO_MIN_TEXT_SIMILARITY: f64 = 0.8;

/// Evidence 4: lag search range for the cross-correlation, on top of the
/// coarse "same window in both timelines" alignment. Covers loudspeaker →
/// mic acoustic delay plus residual clock skew between the two recorders.
pub(crate) const ECHO_XCORR_MAX_LAG_S: f64 = 0.3;

/// Evidence 4: minimum normalized cross-correlation peak. 0.4 is deliberately
/// permissive for a room path (loudspeaker → air → mic adds reverb, level
/// change and spectral shaping that decorrelate the waveform), while still far
/// above the ~0.02–0.05 peaks unrelated speech windows produce; the other
/// three evidences carry the specificity.
pub(crate) const ECHO_XCORR_MIN_PEAK: f64 = 0.4;

/// All correlation work happens at this rate (both tracks are decimated to it).
const XCORR_SAMPLE_RATE: u32 = 16_000;

/// Cap on the correlated window length. Bounds the naive O(window × lags)
/// correlation to ~3×10⁸ multiply-adds per candidate pair (2 s × ±0.3 s at
/// 16 kHz) — a few hundred milliseconds in a release build, and candidate
/// pairs that survive evidence 1–3 are few.
const XCORR_WINDOW_MAX_S: f64 = 2.0;

/// Windows shorter than this (0.25 s at 16 kHz) are too little material to
/// correlate reliably → evidence missing, segment kept.
const XCORR_MIN_WINDOW_SAMPLES: usize = 4_000;

/// How many leading characters of a segment's text the diagnostics sidecar may
/// carry. The sidecar is plain-text JSON on disk, so it must never duplicate
/// the meeting transcript — a short preview is only for lining an entry up
/// with the stored segment.
const DIAGNOSTIC_PREVIEW_CHARS: usize = 8;

/// A mic/system segment pair that passed evidence 1–3 (delay window, coverage,
/// text similarity) and awaits the audio check.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EchoCandidate {
    pub mic_index: usize,
    pub system_index: usize,
    /// `mic.start − system.start`, seconds.
    pub delay_s: f64,
    /// Overlap duration / mic segment duration, in `[0, 1]`.
    pub coverage: f64,
    /// Normalized edit-distance similarity of the normalized texts.
    pub text_similarity: f64,
    /// Whether one normalized text contains the other (accepted even when
    /// `text_similarity` is below threshold, e.g. an echo cut into a longer
    /// system segment).
    pub text_contains: bool,
}

/// Text-evidence scores for one mic/system pair. `None` when either normalized
/// text is too short to judge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TextEvidence {
    pub similarity: f64,
    pub contains: bool,
}

/// Diagnostic record for one evaluated candidate pair — everything needed to
/// audit why a segment was (or was not) suppressed.
///
/// Privacy: the sidecar is an unmanaged plain-text JSON file next to the
/// meeting audio, so it must **not** contain the verbatim transcript. Text is
/// summarized as a character count plus a short
/// ([`DIAGNOSTIC_PREVIEW_CHARS`]) preview — enough to line an entry up with
/// the stored transcript segment without duplicating meeting content on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EchoDiagnosticEntry {
    pub mic_index: usize,
    pub system_index: usize,
    /// Engine speaker id of the mic turn (the mic track's own id space).
    /// Absent in pre-v2 sidecars — the cross-track unification pass then
    /// treats the entry as evidence-free.
    #[serde(default)]
    pub mic_speaker: Option<u32>,
    /// Engine speaker id of the system turn (the system track's own id space,
    /// *before* the merge offset). Absent in pre-v2 sidecars.
    #[serde(default)]
    pub system_speaker: Option<u32>,
    pub mic_start: f64,
    pub mic_end: f64,
    pub system_start: f64,
    pub system_end: f64,
    /// Character count of the mic segment's text (not the text itself).
    pub mic_text_chars: usize,
    /// Character count of the system segment's text (not the text itself).
    pub system_text_chars: usize,
    /// First few characters of the mic text, for lining the entry up with the
    /// transcript.
    pub mic_text_preview: String,
    /// First few characters of the system text.
    pub system_text_preview: String,
    pub delay_s: f64,
    pub coverage: f64,
    pub text_similarity: f64,
    pub text_contains: bool,
    /// Normalized cross-correlation peak; `null` when the audio evidence was
    /// unavailable (WAV unreadable, window too short/silent) — in which case
    /// the segment is always kept.
    pub xcorr_peak: Option<f64>,
    pub suppressed: bool,
}

/// The sidecar document written next to the meeting audio
/// (`<audio-stem>.echo_suppression.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EchoDiagnostics {
    pub version: u32,
    pub mic_segments: usize,
    pub system_segments: usize,
    /// Per mic engine speaker id: how many mic turns that speaker had
    /// *before* suppression. Denominator for the cross-track unification
    /// pass's "significant share suppressed" test. Absent (empty) in pre-v2
    /// sidecars.
    #[serde(default)]
    pub mic_speaker_segments: std::collections::BTreeMap<u32, usize>,
    /// System→mic start skew (seconds) applied from the recording-time
    /// timeline sidecar before pairing; `0.0` when no sidecar was available.
    /// `system_start`/`system_end` in the entries are already shifted by it.
    pub system_skew_seconds: f64,
    /// Pairs that passed evidence 1–3 and were audio-checked.
    pub candidates: usize,
    /// Pairs where all four evidences agreed → mic segment hidden.
    pub suppressed: usize,
    pub entries: Vec<EchoDiagnosticEntry>,
}

/// Outcome of the whole pass: which mic segments to keep, plus the diagnostics.
#[derive(Debug)]
pub(crate) struct EchoSuppression {
    /// One flag per mic turn index; `false` = suppressed (hidden from the
    /// final take).
    pub keep: Vec<bool>,
    pub diagnostics: EchoDiagnostics,
}

/// Which track a correlation window should be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EchoTrack {
    Mic,
    System,
}

/// Injected window reader: `(track, start seconds in that track's own
/// timeline, duration seconds)` → 16 kHz mono samples, or `None` when the
/// audio is unavailable (fail-open).
type WindowReader<'a> = &'a mut dyn FnMut(EchoTrack, f64, f64) -> Option<Vec<f32>>;

/// Normalize text for comparison: drop everything but letters/digits, then
/// lowercase — punctuation, whitespace and case differences between the two
/// tracks' ASR outputs must not defeat the match.
fn normalize_text(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Truncated preview of a segment's text for the diagnostics sidecar (never
/// the full text — see [`EchoDiagnosticEntry`]).
fn text_preview(text: &str) -> String {
    text.chars().take(DIAGNOSTIC_PREVIEW_CHARS).collect()
}

/// Character-level Levenshtein distance (two-row DP).
fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            curr[j + 1] = substitution.min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Evidence 3: compare two texts after normalization. Returns `None` when
/// either normalized text is shorter than [`ECHO_MIN_TEXT_CHARS`] — too little
/// signal to judge, so the pair can never be an echo candidate.
pub(crate) fn text_evidence(mic_text: &str, system_text: &str) -> Option<TextEvidence> {
    let mic = normalize_text(mic_text);
    let system = normalize_text(system_text);
    let mic_chars: Vec<char> = mic.chars().collect();
    let system_chars: Vec<char> = system.chars().collect();
    if mic_chars.len() < ECHO_MIN_TEXT_CHARS || system_chars.len() < ECHO_MIN_TEXT_CHARS {
        return None;
    }
    let distance = levenshtein(&mic_chars, &system_chars);
    let longer = mic_chars.len().max(system_chars.len());
    let similarity = 1.0 - distance as f64 / longer as f64;
    let contains = mic.contains(&system) || system.contains(&mic);
    Some(TextEvidence {
        similarity,
        contains,
    })
}

/// Evidence 1–3 (pure): for every mic segment, find the best system segment
/// that could be its playout source — start delay inside the window, system
/// coverage of the mic segment high enough, and texts similar (or one
/// containing the other) after normalization. At most one candidate per mic
/// segment (best text similarity, then best coverage).
pub(crate) fn find_echo_candidates(
    mic_turns: &[DiarTurn],
    mic_texts: &[String],
    system_turns: &[DiarTurn],
    system_texts: &[String],
) -> Vec<EchoCandidate> {
    let mic_n = mic_turns.len().min(mic_texts.len());
    let system_n = system_turns.len().min(system_texts.len());
    let mut candidates = Vec::new();
    for i in 0..mic_n {
        let mic = &mic_turns[i];
        let mic_duration = mic.end - mic.start;
        if mic_duration <= 0.0 {
            continue;
        }
        let mut best: Option<EchoCandidate> = None;
        for j in 0..system_n {
            let system = &system_turns[j];
            // Evidence 1: delay window.
            let delay_s = mic.start - system.start;
            if !(ECHO_DELAY_MIN_S..=ECHO_DELAY_MAX_S).contains(&delay_s) {
                continue;
            }
            // Evidence 2: time coverage of the mic segment.
            let overlap = mic.end.min(system.end) - mic.start.max(system.start);
            let coverage = (overlap / mic_duration).max(0.0);
            if coverage < ECHO_MIN_COVERAGE {
                continue;
            }
            // Evidence 3: text similarity (short texts are never judged).
            let Some(text) = text_evidence(&mic_texts[i], &system_texts[j]) else {
                continue;
            };
            if text.similarity < ECHO_MIN_TEXT_SIMILARITY && !text.contains {
                continue;
            }
            let candidate = EchoCandidate {
                mic_index: i,
                system_index: j,
                delay_s,
                coverage,
                text_similarity: text.similarity,
                text_contains: text.contains,
            };
            let better = best.as_ref().is_none_or(|b| {
                (candidate.text_similarity, candidate.coverage) > (b.text_similarity, b.coverage)
            });
            if better {
                best = Some(candidate);
            }
        }
        candidates.extend(best);
    }
    candidates
}

/// Evidence 4 (pure): peak normalized cross-correlation of `needle` slid over
/// `haystack` (both 16 kHz mono; the haystack should be the same nominal time
/// window padded by the lag range on each side). For every offset the dot
/// product is normalized by the energies of the two overlapping windows, so
/// the peak is level-independent and in `[0, 1]`; the absolute value is taken
/// because the playback chain may invert polarity.
///
/// Returns `None` (evidence missing → keep the segment) when either window is
/// shorter than [`XCORR_MIN_WINDOW_SAMPLES`] or essentially silent.
pub(crate) fn normalized_xcorr_peak(needle: &[f32], haystack: &[f32]) -> Option<f64> {
    let n = needle.len().min(haystack.len());
    if n < XCORR_MIN_WINDOW_SAMPLES {
        return None;
    }
    let needle = &needle[..n];
    let needle_energy: f64 = needle.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    if needle_energy <= f64::EPSILON {
        return None;
    }
    // Prefix sums of the haystack's squared samples, so each offset's window
    // energy is O(1) and the whole search is O(n · lags) for the dot products.
    let mut prefix = Vec::with_capacity(haystack.len() + 1);
    prefix.push(0.0f64);
    let mut acc = 0.0f64;
    for &s in haystack {
        acc += f64::from(s) * f64::from(s);
        prefix.push(acc);
    }
    let mut peak: Option<f64> = None;
    for offset in 0..=(haystack.len() - n) {
        let window_energy = prefix[offset + n] - prefix[offset];
        if window_energy <= f64::EPSILON {
            continue;
        }
        let dot: f64 = needle
            .iter()
            .zip(&haystack[offset..offset + n])
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum();
        let r = (dot / (needle_energy * window_energy).sqrt()).abs();
        if peak.is_none_or(|p| r > p) {
            peak = Some(r);
        }
    }
    peak
}

/// Run the audio check (evidence 4) over every candidate and build the full
/// diagnostics. The window reader is injected so the decision policy —
/// including the fail-open on missing audio — is unit-testable without files.
pub(crate) fn evaluate_candidates(
    candidates: &[EchoCandidate],
    mic_turns: &[DiarTurn],
    mic_texts: &[String],
    system_turns: &[DiarTurn],
    system_texts: &[String],
    read_window: WindowReader<'_>,
) -> EchoDiagnostics {
    let mut entries = Vec::with_capacity(candidates.len());
    let mut suppressed_total = 0usize;
    for candidate in candidates {
        let mic = &mic_turns[candidate.mic_index];
        let system = &system_turns[candidate.system_index];
        // Correlate over the (capped) overlap of the two segments. Both turn
        // lists are on one timeline here (the caller aligns the system turns
        // by the sidecar skew, or by 0.0 without one); the injected reader
        // maps System window times back to that WAV's own clock. The system
        // read is padded by the lag range on both sides so every lag has
        // full support.
        let window_start = mic.start.max(system.start);
        let window_len = (mic.end.min(system.end) - window_start).min(XCORR_WINDOW_MAX_S);
        let xcorr_peak = if window_len > 0.0 {
            let needle = read_window(EchoTrack::Mic, window_start, window_len);
            let haystack = read_window(
                EchoTrack::System,
                window_start - ECHO_XCORR_MAX_LAG_S,
                window_len + 2.0 * ECHO_XCORR_MAX_LAG_S,
            );
            match (needle, haystack) {
                (Some(needle), Some(haystack)) => normalized_xcorr_peak(&needle, &haystack),
                // WAV unreadable / window unavailable: evidence missing.
                _ => None,
            }
        } else {
            None
        };
        // Fail-open: only a present-and-high correlation peak suppresses.
        let suppressed = xcorr_peak.is_some_and(|p| p >= ECHO_XCORR_MIN_PEAK);
        suppressed_total += usize::from(suppressed);
        entries.push(EchoDiagnosticEntry {
            mic_index: candidate.mic_index,
            system_index: candidate.system_index,
            mic_speaker: Some(mic.speaker),
            system_speaker: Some(system.speaker),
            mic_start: mic.start,
            mic_end: mic.end,
            system_start: system.start,
            system_end: system.end,
            mic_text_chars: mic_texts[candidate.mic_index].chars().count(),
            system_text_chars: system_texts[candidate.system_index].chars().count(),
            mic_text_preview: text_preview(&mic_texts[candidate.mic_index]),
            system_text_preview: text_preview(&system_texts[candidate.system_index]),
            delay_s: candidate.delay_s,
            coverage: candidate.coverage,
            text_similarity: candidate.text_similarity,
            text_contains: candidate.text_contains,
            xcorr_peak,
            suppressed,
        });
    }
    let mut mic_speaker_segments = std::collections::BTreeMap::new();
    for turn in mic_turns {
        *mic_speaker_segments.entry(turn.speaker).or_default() += 1;
    }
    EchoDiagnostics {
        version: 2,
        mic_segments: mic_turns.len(),
        system_segments: system_turns.len(),
        mic_speaker_segments,
        // Callers that aligned the system turns overwrite this with the
        // sidecar skew they applied (see `suppress_cross_track_echoes`).
        system_skew_seconds: 0.0,
        candidates: entries.len(),
        suppressed: suppressed_total,
        entries,
    }
}

/// Recording-time timeline sidecar fields the echo pass cares about (see the
/// desktop recorder's `<meeting-id>.timeline.json`; extra fields are ignored).
#[derive(Debug, Deserialize)]
struct TimelineSidecar {
    #[serde(default)]
    mic_offset_seconds: f64,
    #[serde(default)]
    system_offset_seconds: Option<f64>,
}

/// Read the system→mic start skew (seconds) from the timeline sidecar next to
/// the mic WAV: `system_offset_seconds − mic_offset_seconds`, i.e. how much
/// later than the mic WAV's first sample the system WAV's first sample is on
/// the shared recording timeline. Adding it to a system-track timestamp maps
/// it onto the mic track's timeline. Fail-open: a missing/unreadable sidecar,
/// a mic-only recording, or a non-finite value all yield `0.0` (the previous
/// near-common-start assumption).
pub(crate) fn read_timeline_skew(mic_wav: &Path) -> f64 {
    let (mic_offset, system_offset) = read_timeline_offsets(mic_wav);
    let Some(system_offset) = system_offset else {
        return 0.0;
    };
    let skew = system_offset - mic_offset;
    if skew.is_finite() {
        skew
    } else {
        0.0
    }
}

/// Read both per-track WAV start offsets (seconds from the meeting's shared
/// `t0`) from the timeline sidecar next to the mic WAV. Adding a track's
/// offset to a timestamp in that track's WAV time maps it onto the unified
/// meeting timeline — which is where the recording-time live annotations
/// live. Fail-open: a missing/unreadable sidecar yields `(0.0, None)` (the
/// near-common-start assumption); a mic-only recording has no system offset.
pub(crate) fn read_timeline_offsets(mic_wav: &Path) -> (f64, Option<f64>) {
    let path = mic_wav.with_extension("timeline.json");
    let Ok(json) = std::fs::read_to_string(&path) else {
        return (0.0, None);
    };
    let Ok(sidecar) = serde_json::from_str::<TimelineSidecar>(&json) else {
        tracing::warn!(path = %path.display(), "unparseable timeline sidecar; assuming zero offsets");
        return (0.0, None);
    };
    let sanitize = |v: f64| if v.is_finite() { v } else { 0.0 };
    (
        sanitize(sidecar.mic_offset_seconds),
        sidecar.system_offset_seconds.map(sanitize),
    )
}

/// Shift system-track turns onto the mic track's timeline by the sidecar skew,
/// so evidence 1–2 (delay window, coverage) compare like with like.
pub(crate) fn align_system_turns(turns: &[DiarTurn], system_skew_s: f64) -> Vec<DiarTurn> {
    turns
        .iter()
        .map(|t| DiarTurn::new(t.start + system_skew_s, t.end + system_skew_s, t.speaker))
        .collect()
}

/// The whole pass over two transcribed takes plus their on-disk WAVs: evidence
/// 1–3 pairing, per-candidate audio cross-check against the two files, and the
/// keep/suppress verdict per mic turn. Never fails — any IO problem downgrades
/// to "keep".
///
/// `system_skew_s` (from [`read_timeline_skew`]) puts the system segments on
/// the mic timeline before pairing; system audio windows are read back in the
/// system WAV's own timeline. Diagnostics therefore report `system_start` /
/// `system_end` on the unified (mic) timeline.
pub(crate) fn suppress_cross_track_echoes(
    mic: &TrackTake,
    system: &TrackTake,
    mic_wav: &Path,
    system_wav: &Path,
    system_skew_s: f64,
) -> EchoSuppression {
    let system_turns = align_system_turns(&system.turns, system_skew_s);
    let candidates = find_echo_candidates(&mic.turns, &mic.texts, &system_turns, &system.texts);
    let mut read = |track: EchoTrack, start_s: f64, duration_s: f64| -> Option<Vec<f32>> {
        // Window times arrive on the unified (mic) timeline; the system WAV's
        // own clock starts `system_skew_s` later, so map back before reading.
        let (path, start_s) = match track {
            EchoTrack::Mic => (mic_wav, start_s),
            EchoTrack::System => (system_wav, start_s - system_skew_s),
        };
        read_wav_window_mono_16k(path, start_s, duration_s)
    };
    let mut diagnostics = evaluate_candidates(
        &candidates,
        &mic.turns,
        &mic.texts,
        &system_turns,
        &system.texts,
        &mut read,
    );
    diagnostics.system_skew_seconds = system_skew_s;
    let mut keep = vec![true; mic.turns.len()];
    for entry in &diagnostics.entries {
        if entry.suppressed {
            keep[entry.mic_index] = false;
        }
    }
    EchoSuppression { keep, diagnostics }
}

/// Rebuild a take with only the kept indices (positional zip preserved).
/// Indices beyond `keep` are kept — absent evidence never removes anything.
pub(crate) fn filter_track_take(take: &TrackTake, keep: &[bool]) -> TrackTake {
    let mut out = TrackTake::default();
    for (i, turn) in take.turns.iter().enumerate() {
        if !keep.get(i).copied().unwrap_or(true) {
            continue;
        }
        out.turns.push(*turn);
        if let Some(text) = take.texts.get(i) {
            out.texts.push(text.clone());
        }
        if let Some(words) = take.words.get(i) {
            out.words.push(words.clone());
        }
    }
    out
}

/// Sidecar path: `<audio-stem>.echo_suppression.json` next to the meeting
/// audio, namespaced by the recording's file stem so recordings sharing a
/// directory never collide.
pub(crate) fn diagnostics_sidecar_path(mic_wav: &Path) -> PathBuf {
    let stem = mic_wav
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("meeting");
    mic_wav.with_file_name(format!("{stem}.echo_suppression.json"))
}

/// Write the diagnostics sidecar next to the meeting audio. Best-effort at the
/// call site: a write failure is logged, never fails the pipeline.
pub(crate) fn write_diagnostics_sidecar(
    diagnostics: &EchoDiagnostics,
    mic_wav: &Path,
) -> io::Result<PathBuf> {
    let path = diagnostics_sidecar_path(mic_wav);
    let json = serde_json::to_string_pretty(diagnostics).map_err(io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

// ─────────────────────────────────────────────────────────────────────────────
// WAV window IO — minimal reader for the exact format our recorders write
// (RIFF / PCM16 mono), decimated to 16 kHz. Every failure returns `None`.
// ─────────────────────────────────────────────────────────────────────────────

/// Read `[start_s, start_s + duration_s)` from a PCM16 **mono** WAV and
/// resample it to 16 kHz. `start_s` is clamped at 0 and the read is truncated
/// at end-of-data; `None` on any parse/IO problem or an empty result — callers
/// treat that as missing evidence.
fn read_wav_window_mono_16k(path: &Path, start_s: f64, duration_s: f64) -> Option<Vec<f32>> {
    let (samples, sample_rate) = read_wav_window_native(path, start_s, duration_s)?;
    Some(resample_linear(&samples, sample_rate, XCORR_SAMPLE_RATE))
}

/// Native-rate window read. Parses the RIFF chunks (tolerating extra chunks
/// and the odd-size pad byte) but accepts only uncompressed 16-bit mono PCM —
/// the one format both meeting recorders write.
fn read_wav_window_native(path: &Path, start_s: f64, duration_s: f64) -> Option<(Vec<f32>, u32)> {
    if duration_s <= 0.0 {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut riff = [0u8; 12];
    file.read_exact(&mut riff).ok()?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return None;
    }
    let mut sample_rate: Option<u32> = None;
    loop {
        let mut header = [0u8; 8];
        file.read_exact(&mut header).ok()?;
        let chunk_id = [header[0], header[1], header[2], header[3]];
        let chunk_size = u64::from(u32::from_le_bytes([
            header[4], header[5], header[6], header[7],
        ]));
        match &chunk_id {
            b"fmt " => {
                if chunk_size < 16 {
                    return None;
                }
                let mut fmt = [0u8; 16];
                file.read_exact(&mut fmt).ok()?;
                let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
                if audio_format != 1 || channels != 1 || bits != 16 || rate == 0 {
                    return None;
                }
                sample_rate = Some(rate);
                let remainder = chunk_size - 16 + (chunk_size & 1);
                if remainder > 0 {
                    file.seek(SeekFrom::Current(i64::try_from(remainder).ok()?))
                        .ok()?;
                }
            }
            b"data" => {
                let rate = sample_rate?;
                let total_samples = chunk_size / 2;
                let start_sample = (start_s.max(0.0) * f64::from(rate)) as u64;
                if start_sample >= total_samples {
                    return None;
                }
                let wanted = (duration_s * f64::from(rate)).ceil() as u64;
                let count = wanted.min(total_samples - start_sample);
                if count == 0 {
                    return None;
                }
                file.seek(SeekFrom::Current(i64::try_from(start_sample * 2).ok()?))
                    .ok()?;
                let mut bytes = vec![0u8; usize::try_from(count * 2).ok()?];
                file.read_exact(&mut bytes).ok()?;
                let samples = bytes
                    .chunks_exact(2)
                    .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0)
                    .collect();
                return Some((samples, rate));
            }
            _ => {
                let skip = chunk_size + (chunk_size & 1);
                file.seek(SeekFrom::Current(i64::try_from(skip).ok()?))
                    .ok()?;
            }
        }
    }
}

/// Linear-interpolation resampler. Plenty for a correlation detector — the
/// decision needs a stable peak, not audio fidelity.
fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = f64::from(from_rate) / f64::from(to_rate);
    let out_len = (samples.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let position = i as f64 * ratio;
        let index = position as usize;
        let frac = (position - index as f64) as f32;
        let a = samples[index];
        let b = samples.get(index + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Deterministic pseudo-random noise in [-1, 1) (LCG; no rand dep).
    fn noise(seed: u64, len: usize) -> Vec<f32> {
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / 8_388_608.0) - 1.0
            })
            .collect()
    }

    /// Write a canonical 44-byte-header PCM16 mono WAV (what the recorders
    /// produce).
    fn write_wav(path: &Path, sample_rate: u32, samples: &[f32]) {
        let data_bytes = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for &s in samples {
            bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&bytes).unwrap();
    }

    // ── evidence 1–3 (pure pairing) ─────────────────────────────────────

    #[test]
    fn candidate_found_for_delayed_covered_similar_pair() {
        // System plays the remote line at 10.0–13.0; the mic picks it up
        // 0.2 s later with near-identical text.
        let system_turns = vec![DiarTurn::new(10.0, 13.0, 0)];
        let mic_turns = vec![DiarTurn::new(10.2, 13.1, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["今天我们讨论一下项目进度"]),
            &system_turns,
            &texts(&["今天我们讨论一下项目进度。"]),
        );
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!((c.mic_index, c.system_index), (0, 0));
        assert!((c.delay_s - 0.2).abs() < 1e-9);
        assert!(c.coverage > 0.9, "coverage {}", c.coverage);
        assert!(c.text_similarity > 0.99, "sim {}", c.text_similarity);
    }

    #[test]
    fn short_backchannel_text_is_never_judged() {
        // "好的" repeats on both sides constantly; even with perfect timing
        // overlap it must not become a candidate.
        let system_turns = vec![DiarTurn::new(5.0, 5.6, 0)];
        let mic_turns = vec![DiarTurn::new(5.1, 5.7, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["好的"]),
            &system_turns,
            &texts(&["好的"]),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn insufficient_coverage_is_not_a_candidate() {
        // Same words, but the system segment covers less than 70% of the mic
        // segment's time range.
        let system_turns = vec![DiarTurn::new(10.0, 11.0, 0)];
        let mic_turns = vec![DiarTurn::new(10.2, 13.0, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["今天我们讨论一下项目进度"]),
            &system_turns,
            &texts(&["今天我们讨论一下项目进度"]),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn dissimilar_text_is_not_a_candidate() {
        // Overlapping double-talk (both sides speaking at once) has matching
        // times but different words — must be kept.
        let system_turns = vec![DiarTurn::new(10.0, 13.0, 0)];
        let mic_turns = vec![DiarTurn::new(10.2, 13.0, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["我觉得这个方案不太合适"]),
            &system_turns,
            &texts(&["下周的发布计划需要再确认"]),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn delay_outside_window_is_not_a_candidate() {
        let system_turns = vec![DiarTurn::new(10.0, 13.0, 0), DiarTurn::new(20.0, 23.0, 0)];
        // Mic starts 0.5 s BEFORE the system copy (echo cannot precede its
        // source beyond timestamp tolerance), and 1.0 s after the second.
        let mic_turns = vec![DiarTurn::new(9.5, 12.5, 0), DiarTurn::new(21.0, 23.5, 0)];
        let line = "今天我们讨论一下项目进度";
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&[line, line]),
            &system_turns,
            &texts(&[line, line]),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn containment_counts_as_text_match() {
        // The mic echo got cut short, so its text is a strict substring of the
        // longer system segment (edit-distance similarity alone would fail).
        let system_turns = vec![DiarTurn::new(10.0, 14.0, 0)];
        let mic_turns = vec![DiarTurn::new(10.3, 13.0, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["项目进度需要重新排一下"]),
            &system_turns,
            &texts(&["我认为这个项目进度需要重新排一下才行因为下游依赖变了"]),
        );
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].text_contains);
        assert!(candidates[0].text_similarity < ECHO_MIN_TEXT_SIMILARITY);
    }

    #[test]
    fn normalization_ignores_punctuation_whitespace_and_case() {
        let ev = text_evidence("OK, let's SHIP it now!", "ok lets ship it now").unwrap();
        assert!(ev.similarity > 0.99, "sim {}", ev.similarity);
        let ev = text_evidence("今天，我们讨论：项目进度。", "今天我们讨论项目进度").unwrap();
        assert!(ev.similarity > 0.99, "sim {}", ev.similarity);
    }

    #[test]
    fn best_candidate_prefers_higher_text_similarity() {
        // Two system segments both plausible in time; the more similar text
        // wins the pairing.
        let system_turns = vec![DiarTurn::new(10.0, 13.0, 0), DiarTurn::new(9.9, 13.0, 1)];
        let mic_turns = vec![DiarTurn::new(10.2, 13.0, 0)];
        let candidates = find_echo_candidates(
            &mic_turns,
            &texts(&["今天我们讨论一下项目进度"]),
            &system_turns,
            &texts(&["今天我们讨论一下项目进度对吧", "今天我们讨论一下项目进度"]),
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].system_index, 1);
    }

    // ── evidence 4 (cross-correlation) ──────────────────────────────────

    #[test]
    fn xcorr_peak_is_high_for_the_same_signal_shifted() {
        // The haystack contains the needle at a 50 ms offset.
        let base = noise(7, 16_000);
        let needle = base[800..800 + 8_000].to_vec();
        let haystack = base[..12_000].to_vec();
        let peak = normalized_xcorr_peak(&needle, &haystack).unwrap();
        assert!(peak > 0.9, "peak {peak}");
    }

    #[test]
    fn xcorr_peak_is_low_for_unrelated_signals() {
        let needle = noise(1, 8_000);
        let haystack = noise(2, 12_000);
        let peak = normalized_xcorr_peak(&needle, &haystack).unwrap();
        assert!(peak < ECHO_XCORR_MIN_PEAK, "peak {peak}");
    }

    #[test]
    fn xcorr_peak_detects_shifted_sine_against_noise_floor() {
        // A tone buried at an offset still correlates near 1.0 with itself.
        let sine: Vec<f32> = (0..10_000)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin() * 0.5)
            .collect();
        let mut haystack = vec![0.0f32; 2_000];
        haystack.extend_from_slice(&sine);
        let peak = normalized_xcorr_peak(&sine[..8_000], &haystack).unwrap();
        assert!(peak > 0.95, "peak {peak}");
    }

    #[test]
    fn xcorr_rejects_short_or_silent_windows() {
        // Too short → evidence missing.
        assert!(normalized_xcorr_peak(&noise(3, 1_000), &noise(3, 2_000)).is_none());
        // Silent needle → no energy to normalize by.
        assert!(normalized_xcorr_peak(&vec![0.0; 8_000], &noise(4, 12_000)).is_none());
        // All-silent haystack → every window skipped.
        assert!(normalized_xcorr_peak(&noise(5, 8_000), &vec![0.0; 12_000]).is_none());
    }

    #[test]
    fn xcorr_handles_haystack_shorter_than_needle() {
        // Degenerate but must not panic: needle truncated to the haystack.
        let base = noise(6, 9_000);
        let peak = normalized_xcorr_peak(&base, &base[..8_000]).unwrap();
        assert!(peak > 0.99, "peak {peak}");
    }

    // ── evaluation policy (fail-open) ───────────────────────────────────

    fn one_candidate_fixture() -> (Vec<DiarTurn>, Vec<String>, Vec<DiarTurn>, Vec<String>) {
        let system_turns = vec![DiarTurn::new(0.0, 0.6, 0)];
        let mic_turns = vec![DiarTurn::new(0.05, 0.65, 0)];
        let line = texts(&["今天我们讨论一下项目进度"]);
        (mic_turns, line.clone(), system_turns, line)
    }

    #[test]
    fn missing_audio_evidence_fails_open_to_keeping_the_segment() {
        let (mic_turns, mic_texts, system_turns, system_texts) = one_candidate_fixture();
        let candidates = find_echo_candidates(&mic_turns, &mic_texts, &system_turns, &system_texts);
        assert_eq!(candidates.len(), 1);
        // Reader simulating unreadable WAVs.
        let mut reader = |_: EchoTrack, _: f64, _: f64| -> Option<Vec<f32>> { None };
        let diag = evaluate_candidates(
            &candidates,
            &mic_turns,
            &mic_texts,
            &system_turns,
            &system_texts,
            &mut reader,
        );
        assert_eq!(diag.candidates, 1);
        assert_eq!(diag.suppressed, 0);
        assert_eq!(diag.entries[0].xcorr_peak, None);
        assert!(!diag.entries[0].suppressed);
    }

    #[test]
    fn correlated_audio_suppresses_and_uncorrelated_keeps() {
        let (mic_turns, mic_texts, system_turns, system_texts) = one_candidate_fixture();
        let candidates = find_echo_candidates(&mic_turns, &mic_texts, &system_turns, &system_texts);
        let shared = noise(11, 20_000);

        // Same underlying audio on both tracks → suppressed.
        let mut echo_reader = |track: EchoTrack, _: f64, dur: f64| -> Option<Vec<f32>> {
            let n = (dur * 16_000.0) as usize;
            match track {
                EchoTrack::Mic => Some(shared[..n.min(shared.len())].to_vec()),
                EchoTrack::System => Some(shared[..n.min(shared.len())].to_vec()),
            }
        };
        let diag = evaluate_candidates(
            &candidates,
            &mic_turns,
            &mic_texts,
            &system_turns,
            &system_texts,
            &mut echo_reader,
        );
        assert_eq!(diag.suppressed, 1);
        assert!(diag.entries[0].suppressed);
        assert!(diag.entries[0].xcorr_peak.unwrap() > 0.9);

        // Unrelated audio (headphones: mic hears the user, not the speaker)
        // → kept even though text/timing matched.
        let other = noise(12, 20_000);
        let mut clean_reader = |track: EchoTrack, _: f64, dur: f64| -> Option<Vec<f32>> {
            let n = (dur * 16_000.0) as usize;
            match track {
                EchoTrack::Mic => Some(other[..n.min(other.len())].to_vec()),
                EchoTrack::System => Some(shared[..n.min(shared.len())].to_vec()),
            }
        };
        let diag = evaluate_candidates(
            &candidates,
            &mic_turns,
            &mic_texts,
            &system_turns,
            &system_texts,
            &mut clean_reader,
        );
        assert_eq!(diag.suppressed, 0);
        assert!(!diag.entries[0].suppressed);
        assert!(diag.entries[0].xcorr_peak.unwrap() < ECHO_XCORR_MIN_PEAK);
    }

    #[test]
    fn filter_track_take_drops_only_suppressed_indices() {
        use lumen_transcript::Word;
        let take = TrackTake {
            turns: vec![
                DiarTurn::new(0.0, 1.0, 0),
                DiarTurn::new(1.0, 2.0, 1),
                DiarTurn::new(2.0, 3.0, 0),
            ],
            texts: texts(&["a", "b", "c"]),
            words: vec![
                vec![Word::new("a", 0.0, 1.0)],
                vec![Word::new("b", 1.0, 2.0)],
                vec![Word::new("c", 2.0, 3.0)],
            ],
        };
        let filtered = filter_track_take(&take, &[true, false, true]);
        assert_eq!(filtered.texts, texts(&["a", "c"]));
        assert_eq!(filtered.turns.len(), 2);
        assert_eq!(filtered.turns[1].speaker, 0);
        assert_eq!(filtered.words.len(), 2);
        assert_eq!(filtered.words[1][0].word, "c");
        // Missing keep entries default to keep.
        let untouched = filter_track_take(&take, &[]);
        assert_eq!(untouched.texts.len(), 3);
    }

    // ── WAV window IO ───────────────────────────────────────────────────

    #[test]
    fn wav_window_reads_and_resamples_to_16k() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        // 1 s of 32 kHz audio → windows come back decimated by 2.
        let samples: Vec<f32> = (0..32_000).map(|i| (i % 100) as f32 / 200.0).collect();
        write_wav(&path, 32_000, &samples);
        let window = read_wav_window_mono_16k(&path, 0.25, 0.5).unwrap();
        assert_eq!(window.len(), 8_000);
        // First sample of the window is the source sample at 0.25 s.
        assert!((window[0] - samples[8_000]).abs() < 0.01, "{}", window[0]);
        // A window past end-of-data is truncated; fully past → None.
        let tail = read_wav_window_mono_16k(&path, 0.9, 0.5).unwrap();
        assert!(tail.len() < 8_000);
        assert!(read_wav_window_mono_16k(&path, 2.0, 0.5).is_none());
    }

    #[test]
    fn wav_window_fails_open_on_missing_or_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_wav_window_mono_16k(&dir.path().join("absent.wav"), 0.0, 1.0).is_none());
        // Not a WAV at all.
        let junk = dir.path().join("junk.wav");
        std::fs::write(&junk, b"definitely not RIFF data").unwrap();
        assert!(read_wav_window_mono_16k(&junk, 0.0, 1.0).is_none());
    }

    #[test]
    fn wav_window_rejects_non_mono_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        // Hand-build a stereo header (channels = 2).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36u32 + 8).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo → reject
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&64_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        std::fs::write(&path, bytes).unwrap();
        assert!(read_wav_window_mono_16k(&path, 0.0, 0.5).is_none());
    }

    // ── end-to-end over real files ──────────────────────────────────────

    #[test]
    fn suppresses_echo_pair_across_two_wav_files() {
        let dir = tempfile::tempdir().unwrap();
        let mic_wav = dir.path().join("meeting.wav");
        let system_wav = dir.path().join("meeting.system.wav");

        // System track: the remote line as 1.2 s of noise from t=0.
        let line_audio = noise(21, 19_200);
        write_wav(&system_wav, 16_000, &line_audio);
        // Mic track: 0.1 s of silence, then the same audio picked up from the
        // loudspeaker (attenuated).
        let mut mic_audio = vec![0.0f32; 1_600];
        mic_audio.extend(line_audio.iter().map(|s| s * 0.5));
        write_wav(&mic_wav, 16_000, &mic_audio);

        let line = "今天我们讨论一下项目进度安排";
        let system = TrackTake {
            turns: vec![DiarTurn::new(0.0, 1.2, 0)],
            texts: texts(&[line]),
            words: Vec::new(),
        };
        let mic = TrackTake {
            turns: vec![DiarTurn::new(0.1, 1.3, 0)],
            texts: texts(&[line]),
            words: Vec::new(),
        };

        let result = suppress_cross_track_echoes(&mic, &system, &mic_wav, &system_wav, 0.0);
        assert_eq!(result.keep, vec![false]);
        assert_eq!(result.diagnostics.suppressed, 1);
        assert_eq!(result.diagnostics.system_skew_seconds, 0.0);
        let entry = &result.diagnostics.entries[0];
        assert!(entry.xcorr_peak.unwrap() > ECHO_XCORR_MIN_PEAK);

        // Sidecar lands next to the mic wav, namespaced by its stem.
        let sidecar = write_diagnostics_sidecar(&result.diagnostics, &mic_wav).unwrap();
        assert_eq!(sidecar, dir.path().join("meeting.echo_suppression.json"));
        let raw = std::fs::read_to_string(&sidecar).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["suppressed"], 1);
        assert_eq!(json["entries"][0]["suppressed"], true);
        // Privacy: the sidecar carries only lengths and a short preview,
        // never the verbatim transcript text.
        assert!(!raw.contains(line), "sidecar must not contain full text");
        assert_eq!(json["entries"][0]["mic_text_chars"], line.chars().count());
        assert_eq!(
            json["entries"][0]["mic_text_preview"],
            line.chars().take(8).collect::<String>()
        );
    }

    #[test]
    fn unreadable_wavs_keep_everything_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let line = "今天我们讨论一下项目进度安排";
        let system = TrackTake {
            turns: vec![DiarTurn::new(0.0, 1.2, 0)],
            texts: texts(&[line]),
            words: Vec::new(),
        };
        let mic = TrackTake {
            turns: vec![DiarTurn::new(0.1, 1.3, 0)],
            texts: texts(&[line]),
            words: Vec::new(),
        };
        let result = suppress_cross_track_echoes(
            &mic,
            &system,
            &dir.path().join("missing.wav"),
            &dir.path().join("missing.system.wav"),
            0.0,
        );
        // Evidence 1–3 matched, but the audio evidence is missing → keep.
        assert_eq!(result.keep, vec![true]);
        assert_eq!(result.diagnostics.candidates, 1);
        assert_eq!(result.diagnostics.suppressed, 0);
    }

    /// The unification pass consumes the in-memory diagnostics, but the
    /// sidecar is the on-disk audit trail of the very same data — so its JSON
    /// must round-trip losslessly (`Deserialize` is the format contract for
    /// tooling and tests).
    #[test]
    fn diagnostics_sidecar_round_trips_the_v2_speaker_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mic_wav = dir.path().join("meeting.wav");

        // Round-trip: the v2 speaker attribution fields survive.
        let mic_turns = vec![DiarTurn::new(0.0, 1.0, 0), DiarTurn::new(2.0, 3.0, 0)];
        let system_turns = vec![DiarTurn::new(0.0, 1.1, 1)];
        let candidates = vec![EchoCandidate {
            mic_index: 0,
            system_index: 0,
            delay_s: 0.0,
            coverage: 1.0,
            text_similarity: 1.0,
            text_contains: true,
        }];
        let mut always_missing = |_track: EchoTrack, _s: f64, _d: f64| None;
        let diagnostics = evaluate_candidates(
            &candidates,
            &mic_turns,
            &texts(&["今天我们讨论一下项目进度", "另一句"]),
            &system_turns,
            &texts(&["今天我们讨论一下项目进度"]),
            &mut always_missing,
        );
        assert_eq!(diagnostics.version, 2);
        assert_eq!(diagnostics.mic_speaker_segments.get(&0).copied(), Some(2));
        let sidecar = write_diagnostics_sidecar(&diagnostics, &mic_wav).unwrap();
        let json = std::fs::read_to_string(&sidecar).unwrap();
        let back: EchoDiagnostics = serde_json::from_str(&json).expect("sidecar parses back");
        assert_eq!(back.version, 2);
        assert_eq!(back.mic_speaker_segments, diagnostics.mic_speaker_segments);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].mic_speaker, Some(0));
        assert_eq!(back.entries[0].system_speaker, Some(1));
    }

    // ── unified-timeline skew (timeline.json sidecar) ───────────────────

    #[test]
    fn align_system_turns_shifts_onto_mic_timeline() {
        let turns = vec![DiarTurn::new(0.4, 1.6, 0), DiarTurn::new(3.0, 4.0, 1)];
        let aligned = align_system_turns(&turns, 1.0);
        assert_eq!(aligned[0], DiarTurn::new(1.4, 2.6, 0));
        assert_eq!(aligned[1], DiarTurn::new(4.0, 5.0, 1));
        // Zero skew is the identity (the no-sidecar fallback).
        assert_eq!(align_system_turns(&turns, 0.0), turns);
    }

    #[test]
    fn timeline_skew_read_from_sidecar_or_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mic_wav = dir.path().join("meeting.wav");

        // No sidecar at all → 0.0 (older / crash-recovered meetings).
        assert_eq!(read_timeline_skew(&mic_wav), 0.0);

        // Dual-track sidecar → system minus mic offset.
        let sidecar = dir.path().join("meeting.timeline.json");
        std::fs::write(
            &sidecar,
            r#"{"mic_offset_seconds":0.05,"system_offset_seconds":0.45,"t0_wall_clock":"x"}"#,
        )
        .unwrap();
        assert!((read_timeline_skew(&mic_wav) - 0.4).abs() < 1e-12);

        // Mic-only sidecar (no system offset) → 0.0.
        std::fs::write(
            &sidecar,
            r#"{"mic_offset_seconds":0.05,"t0_wall_clock":"x"}"#,
        )
        .unwrap();
        assert_eq!(read_timeline_skew(&mic_wav), 0.0);

        // Garbage stays fail-open.
        std::fs::write(&sidecar, "not json").unwrap();
        assert_eq!(read_timeline_skew(&mic_wav), 0.0);
    }

    #[test]
    fn sidecar_skew_lets_a_late_starting_system_track_pair_up() {
        // The system tap started 1.0 s after the mic (a realistic permission
        // prompt / tap spin-up gap). In raw per-WAV timestamps the echo pair
        // is 1.1 s apart — far outside the delay window — but on the unified
        // timeline it is a plain 0.1 s echo.
        let dir = tempfile::tempdir().unwrap();
        let mic_wav = dir.path().join("meeting.wav");
        let system_wav = dir.path().join("meeting.system.wav");
        let skew = 1.0f64;

        // System WAV (its own clock): 0.4 s silence, then 1.2 s of "speech".
        let line_audio = noise(21, 19_200);
        let mut system_audio = vec![0.0f32; 6_400];
        system_audio.extend(line_audio.iter().copied());
        write_wav(&system_wav, 16_000, &system_audio);
        // Mic WAV: the same audio picked up from the loudspeaker, starting at
        // 1.5 s mic time (= 0.4 s system time + 1.0 s skew + 0.1 s delay).
        let mut mic_audio = vec![0.0f32; 24_000];
        mic_audio.extend(line_audio.iter().map(|s| s * 0.5));
        write_wav(&mic_wav, 16_000, &mic_audio);

        let line = "今天我们讨论一下项目进度安排";
        let system = TrackTake {
            turns: vec![DiarTurn::new(0.4, 1.6, 0)], // system WAV's own clock
            texts: texts(&[line]),
            words: Vec::new(),
        };
        let mic = TrackTake {
            turns: vec![DiarTurn::new(1.5, 2.7, 0)], // mic WAV clock
            texts: texts(&[line]),
            words: Vec::new(),
        };

        // Without the sidecar skew the pair is not even a candidate.
        let unaligned = suppress_cross_track_echoes(&mic, &system, &mic_wav, &system_wav, 0.0);
        assert_eq!(unaligned.keep, vec![true]);
        assert_eq!(unaligned.diagnostics.candidates, 0);

        // With it, all four evidences line up and the echo is suppressed.
        let aligned = suppress_cross_track_echoes(&mic, &system, &mic_wav, &system_wav, skew);
        assert_eq!(aligned.keep, vec![false]);
        assert_eq!(aligned.diagnostics.suppressed, 1);
        assert_eq!(aligned.diagnostics.system_skew_seconds, skew);
        let entry = &aligned.diagnostics.entries[0];
        // Diagnostics report the system segment on the unified timeline.
        assert!((entry.system_start - 1.4).abs() < 1e-9);
        assert!((entry.delay_s - 0.1).abs() < 1e-9);
        assert!(entry.xcorr_peak.unwrap() > ECHO_XCORR_MIN_PEAK);
    }
}
