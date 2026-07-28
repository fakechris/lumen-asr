//! Runnable (no models, no audio, no network) coverage of the assemble +
//! persist wiring, plus the platform gate. The real diar-rs + ASR path lives in
//! `integration_macos.rs` behind `#[ignore]`.

use lumen_meeting::{assemble_meeting, new_meeting, DiarTurn};
use lumen_store::Store;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("meeting.sqlite")).unwrap();
    (dir, store)
}

/// Assemble stub turns + text, persist exactly as `transcribe_meeting` does,
/// and verify the v6 rows round-trip.
#[test]
fn assembled_meeting_persists_and_reads_back() {
    let (_dir, store) = temp_store();

    let turns = vec![
        DiarTurn::new(0.0, 2.0, 0),
        DiarTurn::new(2.0, 4.0, 1),
        DiarTurn::new(4.0, 6.0, 0),
    ];
    let texts = vec![
        "hello there".to_string(),
        "hi back".to_string(),
        "bye now".to_string(),
    ];

    let mut meeting = new_meeting(Some("/store/audio.wav".into()), Some(6.0));
    meeting.title = Some("Weekly sync".into());
    let meeting_id = meeting.id;
    let assembled = assemble_meeting(meeting_id, &turns, &texts, Some(16_000), Some(6.0));

    // Same sequence transcribe_meeting runs.
    store.create_meeting(&meeting).unwrap();
    for speaker in &assembled.speakers {
        store.upsert_speaker(speaker).unwrap();
    }
    store.add_segments(&assembled.segments).unwrap();
    store
        .update_meeting_status(meeting_id, lumen_core::MeetingStatus::Ready)
        .unwrap();

    // Meeting row.
    let stored = store.get_meeting(meeting_id).unwrap().unwrap();
    assert_eq!(stored.status, lumen_core::MeetingStatus::Ready);
    assert_eq!(stored.title.as_deref(), Some("Weekly sync"));
    assert_eq!(stored.duration_seconds, Some(6.0));

    // Two distinct speakers, labelled S1/S2.
    let speakers = store.list_speakers(meeting_id).unwrap();
    assert_eq!(speakers.len(), 2);
    assert_eq!(speakers[0].label, "S1");
    assert_eq!(speakers[1].label, "S2");

    // Three segments in seq order, referencing real speaker rows.
    let segments = store.list_segments(meeting_id).unwrap();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].text, "hello there");
    assert_eq!(segments[0].seq, 0);
    // Turns 0 and 2 share engine speaker 0 -> same stored speaker row.
    assert_eq!(segments[0].speaker_id, segments[2].speaker_id);
    assert_ne!(segments[0].speaker_id, segments[1].speaker_id);
    let s1 = speakers.iter().find(|s| s.label == "S1").unwrap();
    assert_eq!(segments[0].speaker_id, Some(s1.id));
}

/// On any build that cannot diarize (this one — default features / non-macOS),
/// `transcribe_meeting` fails fast with `Unsupported` before touching the wav.
#[cfg(not(all(target_os = "macos", feature = "diarize")))]
#[tokio::test]
async fn transcribe_meeting_is_unsupported_without_diarize() {
    use lumen_asr_engine::StubAsr;
    use lumen_meeting::{transcribe_meeting, DiarModels, MeetingError, MeetingOptions};
    use std::path::Path;

    let (_dir, store) = temp_store();
    let engine = StubAsr::new("unused");
    let models = DiarModels::under_root("/nonexistent/diar");

    let result = transcribe_meeting(
        Path::new("/nonexistent/does-not-exist.wav"),
        &models,
        &engine,
        &store,
        &MeetingOptions::default(),
    )
    .await;

    assert!(matches!(result, Err(MeetingError::Unsupported(_))));
}
