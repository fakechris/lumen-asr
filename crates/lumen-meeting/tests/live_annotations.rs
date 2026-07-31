//! End-to-end integration test for L2 live-annotation reconciliation: a
//! meeting with stored `live_annotations` rows goes through the exact
//! offline chain (`reconcile_stored_annotations`, the same function
//! `process_meeting` runs between assembly and persistence), and the manual
//! names must land in the persisted speakers/segments of the final
//! transcript.
//!
//! The fixture reproduces the real recording-time time bases that used to
//! lose every annotation in the field:
//!
//! - The mic WAV starts ~0.18 s after the meeting's shared `t0` (recorded in
//!   the `<meeting-id>.timeline.json` sidecar), so offline WAV-time segments
//!   must be lifted onto the unified timeline before matching.
//! - Live annotation stamps are callback-arrival times: a live line's start
//!   is anchored at the first packet after the previous utterance's endpoint
//!   (so it drags in leading silence) and its end includes the streaming
//!   endpoint's trailing-silence window.
//! - The **loss** scenario: live caption lines split on streaming-ASR
//!   endpoints (~seconds long), while the offline diarizer merges continuous
//!   speech into far longer turns — the user's annotation on one live line
//!   covers only a small fraction of the diarized segment it belongs to.

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

    // The user's marks while recording (stored rows, unified-timeline stamps):
    // 1. "仅此句" on one short live line (~6.5 s arrival-stamped span, start
    //    glued to the previous endpoint) inside what diarization later merges
    //    into a single 40 s turn.
    // 2. "此句及之后" (open-ended) from ~129.5 s: 李四 speaks from here on.
    store
        .add_live_annotation(&LiveAnnotation::new(
            meeting.id,
            12.4,
            Some(18.9),
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
    // S1 = one long merged 40 s turn containing the annotated line; S2 = two
    // turns inside the open-ended range; S3 = an unannotated turn between.
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
    assert!(
        !outcome.renamed_speakers.is_empty(),
        "the annotated clusters must be renamed"
    );

    // Persist exactly like `process_meeting` does after reconciliation.
    for speaker in &speakers {
        store.upsert_speaker(speaker).unwrap();
    }
    store.add_segments(&segments).unwrap();

    // The final transcript (what every later read sees) carries the manual
    // attribution.
    let stored_speakers = store.list_speakers(meeting.id).unwrap();
    let stored_segments = store.list_segments(meeting.id).unwrap();
    let name_of = |seq: u32| -> Option<String> {
        let seg = stored_segments.iter().find(|s| s.seq == seq).unwrap();
        stored_speakers
            .iter()
            .find(|s| Some(s.id) == seg.speaker_id)
            .and_then(|s| s.display_name.clone())
    };

    // The short-line annotation names the whole merged 40 s turn: 6.5 s is
    // only 16 % of the segment (the old segment-only ≥50 % rule dropped every
    // such annotation → "no manual attribution at all" in the field) but 100 %
    // of the annotation, so the symmetric rule keeps it.
    assert_eq!(name_of(0).as_deref(), Some("张三"));
    // Unannotated cluster: untouched.
    assert_eq!(name_of(1), None);
    // Open-ended "此句及之后": applies to its own line's turn AND the later
    // one, with no closing mark ever written.
    assert_eq!(name_of(2).as_deref(), Some("李四"));
    assert_eq!(name_of(3).as_deref(), Some("李四"));

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
