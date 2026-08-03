//! Per-track diarization fail-open — the "silent system track must not kill
//! the meeting" regression suite (matches the field failure: a 30-min
//! dual-track meeting whose system track was all-zero RMS failed the whole
//! run with `diarization failed: pipeline: too few x-vectors`).
//!
//! Cross-platform tests: silent tracks are skipped by the energy preflight on
//! *every* build (no models needed), so the "no speech on any track → failed
//! with an explicit reason" contract is asserted everywhere.
//!
//! The full "mic voiced + system all-zero → meeting succeeds with mic-only
//! speakers" flow needs real diarization; it is `#[ignore]`d and runs on macOS
//! with the installed diar weights:
//!
//! ```sh
//! cargo test -p lumen-meeting --features diarize --test silent_track -- --ignored --nocapture
//! ```

use std::io::Write;
use std::path::Path;
#[cfg(all(target_os = "macos", feature = "diarize"))]
use std::path::PathBuf;

use lumen_asr_engine::StubAsr;
#[cfg(all(target_os = "macos", feature = "diarize"))]
use lumen_core::SegmentChannel;
use lumen_core::{Meeting, MeetingStatus};
use lumen_meeting::{process_meeting, DiarModels, MeetingOptions};
use lumen_store::Store;
use uuid::Uuid;

/// Write mono PCM16 WAV — the format both meeting recorders produce.
fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) {
    let mut file = std::fs::File::create(path).unwrap();
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * 2;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_len).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&1u16.to_le_bytes()); // mono
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes()); // block align
    header.extend_from_slice(&16u16.to_le_bytes()); // bits
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    file.write_all(&header).unwrap();
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        file.write_all(&v.to_le_bytes()).unwrap();
    }
}

/// Speech-loudness synthetic signal (sine mixture, RMS ≈ 0.3) — loud enough
/// for the energy preflight; real diarization may or may not cluster it, and
/// either way the pipeline must succeed (fail-open).
fn voiced(seconds: f64, sample_rate: u32) -> Vec<f32> {
    (0..(seconds * f64::from(sample_rate)) as usize)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            (2.0 * std::f32::consts::PI * 180.0 * t).sin() * 0.4
                + (2.0 * std::f32::consts::PI * 610.0 * t).sin() * 0.2
        })
        .collect()
}

fn silent(seconds: f64, sample_rate: u32) -> Vec<f32> {
    vec![0.0f32; (seconds * f64::from(sample_rate)) as usize]
}

fn open_meeting(dir: &Path) -> (Store, Uuid) {
    let store = Store::open(dir.join("m.sqlite")).unwrap();
    let meeting = Meeting::new();
    store.create_meeting(&meeting).unwrap();
    store
        .update_meeting_status(meeting.id, MeetingStatus::Processing)
        .unwrap();
    (store, meeting.id)
}

/// Both tracks fully silent → the meeting fails, with the explicit
/// "no speech detected on any track" reason — not an internal diar error.
/// Runs on every build: silent tracks never reach diarization.
#[tokio::test]
async fn dual_track_all_silent_fails_with_explicit_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mic = dir.path().join("mic.wav");
    let sys = dir.path().join("system.wav");
    write_wav(&mic, &silent(20.0, 44_100), 44_100);
    write_wav(&sys, &silent(20.0, 48_000), 48_000);
    let (store, meeting_id) = open_meeting(dir.path());
    let engine = StubAsr::new("unused");

    let result = process_meeting(
        &store,
        meeting_id,
        &mic,
        Some(&sys),
        &DiarModels::under_root(dir.path().join("no-models")),
        &engine,
        None,
        &MeetingOptions::default(),
    )
    .await;

    assert!(result.is_err(), "all-silent meeting must fail");
    let after = store.get_meeting(meeting_id).unwrap().unwrap();
    assert_eq!(after.status, MeetingStatus::Failed);
    let reason = after.failure_reason.unwrap_or_default();
    assert!(
        reason.contains("no speech detected on any track"),
        "reason should be explicit, got: {reason}"
    );
    assert!(store.list_segments(meeting_id).unwrap().is_empty());
}

/// Single-track (mic-only) silent meeting gets the same explicit failure.
#[tokio::test]
async fn mic_only_silent_fails_with_explicit_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mic = dir.path().join("mic.wav");
    write_wav(&mic, &silent(15.0, 44_100), 44_100);
    let (store, meeting_id) = open_meeting(dir.path());
    let engine = StubAsr::new("unused");

    let result = process_meeting(
        &store,
        meeting_id,
        &mic,
        None,
        &DiarModels::under_root(dir.path().join("no-models")),
        &engine,
        None,
        &MeetingOptions::default(),
    )
    .await;

    assert!(result.is_err());
    let after = store.get_meeting(meeting_id).unwrap().unwrap();
    assert_eq!(after.status, MeetingStatus::Failed);
    assert!(after
        .failure_reason
        .unwrap_or_default()
        .contains("no speech detected on any track"));
}

/// A track with clear speech but faint background hiss elsewhere must NOT be
/// mistaken for silent by the preflight — on a non-diarizing build the voiced
/// mic track still reaches (and is rejected by) the diarization gate, proving
/// the preflight let it through.
#[cfg(not(all(target_os = "macos", feature = "diarize")))]
#[tokio::test]
async fn voiced_mic_track_passes_preflight_to_the_diarize_gate() {
    let dir = tempfile::tempdir().unwrap();
    let mic = dir.path().join("mic.wav");
    write_wav(&mic, &voiced(10.0, 16_000), 16_000);
    let (store, meeting_id) = open_meeting(dir.path());
    let engine = StubAsr::new("unused");

    let result = process_meeting(
        &store,
        meeting_id,
        &mic,
        None,
        &DiarModels::under_root(dir.path().join("no-models")),
        &engine,
        None,
        &MeetingOptions::default(),
    )
    .await;

    // Not `NoSpeech`: the preflight passed the track on to diarization, which
    // this build cannot do.
    let err = result.expect_err("non-diarize build cannot transcribe a voiced track");
    assert!(
        err.to_string().contains("unsupported"),
        "expected the diarize-unsupported gate, got: {err}"
    );
}

/// The field bug, end-to-end: mic track voiced (44.1 kHz), system track
/// all-zero (48 kHz). The meeting must complete from the mic track alone —
/// system skipped by the preflight, and even if the synthetic mic audio
/// defeats clustering, the fail-open single-speaker fallback keeps the
/// content. Needs the installed diar weights (`DIAR_MODELS_ROOT` or the
/// shared Lumen models dir).
#[cfg(all(target_os = "macos", feature = "diarize"))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the diar model weights (DIAR_MODELS_ROOT or ~/Library/Application Support/Lumen/models/diar)"]
async fn voiced_mic_with_all_zero_system_track_succeeds_mic_only() {
    let root = std::env::var("DIAR_MODELS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap())
                .join("Library/Application Support/Lumen/models/diar")
        });
    assert!(
        root.join("seg.onnx").is_file(),
        "diar models not installed at {}",
        root.display()
    );

    let dir = tempfile::tempdir().unwrap();
    let mic = dir.path().join("mic.wav");
    let sys = dir.path().join("system.wav");
    // 30 s of speech-loud audio on the mic; a fully silent system track, the
    // exact shape of the field failure (RMS = 0 end to end).
    write_wav(&mic, &voiced(30.0, 44_100), 44_100);
    write_wav(&sys, &silent(30.0, 48_000), 48_000);
    let (store, meeting_id) = open_meeting(dir.path());
    let engine = StubAsr::new("(stub transcription)");

    process_meeting(
        &store,
        meeting_id,
        &mic,
        Some(&sys),
        &DiarModels::under_root(&root),
        &engine,
        None,
        &MeetingOptions::default(),
    )
    .await
    .expect("silent system track must not fail the meeting");

    let after = store.get_meeting(meeting_id).unwrap().unwrap();
    assert_eq!(after.status, MeetingStatus::Ready);

    let segments = store.list_segments(meeting_id).unwrap();
    let speakers = store.list_speakers(meeting_id).unwrap();
    assert!(!segments.is_empty(), "mic content must survive");
    assert!(!speakers.is_empty(), "expected at least one mic speaker");
    // Only mic speakers: nothing may be attributed to the silent system track.
    assert!(
        segments
            .iter()
            .all(|s| s.channel != Some(SegmentChannel::System)),
        "no segment may come from the silent system track"
    );
    assert!(segments
        .iter()
        .all(|s| s.text.contains("(stub transcription)")));
    eprintln!(
        "meeting ready: {} segments across {} speakers (mic-only)",
        segments.len(),
        speakers.len()
    );
}
