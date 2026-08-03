//! End-to-end integration test for L2 live-annotation reconciliation under the
//! **timeline-boundary model**: a meeting with stored `live_annotations` rows
//! goes through the exact offline chain (`reconcile_stored_annotations`, the
//! same function `process_meeting` runs between assembly and persistence), and
//! the manual names must land in the persisted speakers/segments — including
//! **splitting** a diarized segment at a boundary time that falls inside it.
//!
//! The fixture keeps the real recording-time timeline offset: the mic WAV
//! starts ~0.18 s after the meeting's shared `t0` (recorded in the
//! `<meeting-id>.timeline.json` sidecar), so offline WAV-time segments must be
//! lifted onto the unified timeline before their boundary times are compared.

use lumen_core::{LiveAnnotation, Meeting, SegmentChannel, Speaker, TranscriptSegment};
use lumen_meeting::reconcile_stored_annotations;
use lumen_store::Store;
use uuid::Uuid;

fn segment(
    meeting_id: Uuid,
    seq: u32,
    start: f64,
    end: f64,
    speaker: &Speaker,
) -> TranscriptSegment {
    let mut seg = TranscriptSegment::new(meeting_id, seq, start, end, "…");
    seg.speaker_id = Some(speaker.id);
    seg.channel = Some(SegmentChannel::Mic);
    seg
}

#[test]
fn recorded_annotations_survive_the_offline_pipeline_into_the_final_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("m.sqlite")).unwrap();
    let meeting = Meeting::new();
    store.create_meeting(&meeting).unwrap();

    // Recording-time timeline sidecar, exactly as the desktop recorder
    // writes it next to the mic WAV: the mic capture came up 0.18 s after t0.
    let mic_wav = dir.path().join(format!("{}.wav", meeting.id));
    std::fs::write(
        mic_wav.with_extension("timeline.json"),
        r#"{"mic_offset_seconds":0.18,"system_offset_seconds":null,"t0_wall_clock":"2026-07-30T10:00:00Z"}"#,
    )
    .unwrap();

    // The user's boundaries while recording (stored rows, unified-timeline
    // stamps): 张三 from 12.4 s, then 李四 from 129.5 s.
    store
        .add_live_annotation(&LiveAnnotation::new(
            meeting.id,
            12.4,
            None,
            SegmentChannel::Mic,
            None,
            "张三",
        ))
        .unwrap();
    store
        .add_live_annotation(&LiveAnnotation::new(
            meeting.id,
            129.5,
            None,
            SegmentChannel::Mic,
            None,
            "李四",
        ))
        .unwrap();

    // What the offline pipeline assembled (WAV-time turns, diarized clusters):
    // S1 = one long 40 s turn that *starts before* the 张三 boundary at 12.4;
    // S3 = a turn that falls inside 张三's range; S2 = two turns in 李四's range.
    let s1 = Speaker::new(meeting.id, "S1");
    let s2 = Speaker::new(meeting.id, "S2");
    let s3 = Speaker::new(meeting.id, "S3");
    let mut speakers = vec![s1.clone(), s2.clone(), s3.clone()];
    let mut segments = vec![
        segment(meeting.id, 0, 10.0, 50.0, &s1),
        segment(meeting.id, 1, 60.0, 70.0, &s3),
        segment(meeting.id, 2, 130.0, 140.0, &s2),
        segment(meeting.id, 3, 200.0, 210.0, &s2),
    ];

    let outcome = reconcile_stored_annotations(
        &store,
        meeting.id,
        &mic_wav,
        None,
        &mut speakers,
        &mut segments,
    )
    .unwrap();
    // The 张三 boundary at 12.4 falls inside S1's [10, 50] turn (unified
    // [10.18, 50.18]), so that segment is split at the boundary time.
    assert_eq!(
        outcome.split_segments, 1,
        "S1 must split at the 张三 boundary"
    );

    // Persist exactly like `process_meeting` does after reconciliation.
    for speaker in &speakers {
        store.upsert_speaker(speaker).unwrap();
    }
    store.add_segments(&segments).unwrap();

    // The final transcript, read back in seq order.
    let stored_speakers = store.list_speakers(meeting.id).unwrap();
    let stored_segments = store.list_segments(meeting.id).unwrap();
    let name_at = |index: usize| -> Option<String> {
        let seg = &stored_segments[index];
        stored_speakers
            .iter()
            .find(|s| Some(s.id) == seg.speaker_id)
            .and_then(|s| s.display_name.clone())
    };

    // S1 split into a head before the boundary and a tail from it:
    //   [10, 12.22] keeps the diarization cluster (before 张三's boundary),
    //   [12.22, 50] is attributed to 张三.
    assert_eq!(stored_segments.len(), 5);
    assert!((stored_segments[0].start_seconds - 10.0).abs() < 1e-6);
    assert!((stored_segments[0].end_seconds - 12.22).abs() < 1e-6);
    assert_eq!(name_at(0), None);
    assert!((stored_segments[1].start_seconds - 12.22).abs() < 1e-6);
    assert_eq!(name_at(1).as_deref(), Some("张三"));
    // S3 falls inside 张三's range (until the 李四 boundary at 129.5).
    assert_eq!(name_at(2).as_deref(), Some("张三"));
    // The two later turns fall in 李四's range.
    assert_eq!(name_at(3).as_deref(), Some("李四"));
    assert_eq!(name_at(4).as_deref(), Some("李四"));
    // Dense, gap-free seq renumbering after the split.
    assert_eq!(
        stored_segments.iter().map(|s| s.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );

    let named: Vec<&str> = stored_speakers
        .iter()
        .filter_map(|s| s.display_name.as_deref())
        .collect();
    assert!(named.contains(&"张三"));
    assert!(named.contains(&"李四"));
}

/// No annotations → strict no-op (and no sidecar needed): guards the wiring
/// for meetings recorded without any manual marks.
#[test]
fn a_meeting_without_annotations_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("m.sqlite")).unwrap();
    let meeting = Meeting::new();
    store.create_meeting(&meeting).unwrap();

    let s1 = Speaker::new(meeting.id, "S1");
    let mut speakers = vec![s1.clone()];
    let mut segments = vec![segment(meeting.id, 0, 0.0, 5.0, &s1)];
    let before = (speakers.clone(), segments.clone());

    let outcome = reconcile_stored_annotations(
        &store,
        meeting.id,
        &dir.path().join("missing.wav"),
        None,
        &mut speakers,
        &mut segments,
    )
    .unwrap();

    assert_eq!(outcome, Default::default());
    assert_eq!((speakers, segments), before);
}
