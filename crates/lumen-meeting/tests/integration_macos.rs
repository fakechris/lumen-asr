//! Real end-to-end diarization + transcription over a wav. Needs the open diar
//! model weights and an audio file, so it is `#[ignore]`d — it never runs in a
//! normal `cargo test`, only when invoked explicitly on macOS.
//!
//! Manual run (macOS, from the workspace root):
//!
//! ```sh
//! # 1. Point at the three diar-rs open weights (seg.onnx / emb.onnx / plda/).
//! export DIAR_MODELS_ROOT=/path/to/models/diar
//! # 2. Point at a 16 kHz-ish wav with >= 2 speakers.
//! export MEETING_WAV=/path/to/meeting.wav
//! # 3. Run with the diarize feature and the ignored filter.
//! cargo test -p lumen-meeting --features diarize --test integration_macos -- --ignored --nocapture
//! ```
//!
//! Uses `StubAsr` so it exercises the real diar-rs pipeline and the full
//! slice -> ASR -> assemble -> persist wiring without needing a real (sherpa)
//! ASR build; swap in a real engine to check transcription text too.

#![cfg(all(target_os = "macos", feature = "diarize"))]

use std::path::PathBuf;

use lumen_asr_engine::StubAsr;
use lumen_meeting::{transcribe_meeting, DiarModels, MeetingOptions};
use lumen_store::Store;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs diar model weights (DIAR_MODELS_ROOT) and an audio file (MEETING_WAV)"]
async fn diarize_transcribe_persist_end_to_end() {
    let root = std::env::var("DIAR_MODELS_ROOT").expect("set DIAR_MODELS_ROOT to the diar weights");
    let wav = std::env::var("MEETING_WAV").expect("set MEETING_WAV to a test recording");

    let models = DiarModels::under_root(&root);
    let engine = StubAsr::new("(stub transcription)");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("meeting.sqlite")).unwrap();

    let meeting_id = transcribe_meeting(
        &PathBuf::from(wav),
        &models,
        &engine,
        &store,
        &MeetingOptions::default(),
    )
    .await
    .expect("pipeline should succeed with real models + audio");

    let meeting = store.get_meeting(meeting_id).unwrap().unwrap();
    assert_eq!(meeting.status, lumen_core::MeetingStatus::Ready);

    let segments = store.list_segments(meeting_id).unwrap();
    let speakers = store.list_speakers(meeting_id).unwrap();
    assert!(!segments.is_empty(), "expected at least one turn");
    assert!(!speakers.is_empty(), "expected at least one speaker");
    eprintln!(
        "diarized {} turns across {} speakers",
        segments.len(),
        speakers.len()
    );
}
