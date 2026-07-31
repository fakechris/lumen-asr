//! Pure, engine-free assembly of diarization turns + per-turn text into the
//! stored ([`lumen_core`]) and interchange ([`lumen_transcript`]) shapes.
//!
//! This module deliberately depends on neither `diar-rs` nor any ASR engine:
//! its inputs are plain [`DiarTurn`]s and already-transcribed strings, so the
//! "turns + text -> transcript / storage rows" logic is fully unit-testable
//! with stub data (no model weights, no audio, no network).

use lumen_core::{Meeting, Speaker, TranscriptSegment};
use lumen_transcript::{
    Media, Provenance, Segment as TSegment, Speaker as TSpeaker, TranscriptV1, Word,
};
use uuid::Uuid;

/// A single diarization turn: a half-open `[start, end)` window (seconds from
/// media start) attributed to one engine speaker id.
///
/// This mirrors `diar_rs::Turn` but is defined here so the assembly logic and
/// the crate's public API stay cross-platform (diar-rs is macOS-only). On the
/// macOS diarization path each `diar_rs::Turn` is mapped into one of these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiarTurn {
    pub start: f64,
    pub end: f64,
    pub speaker: u32,
}

impl DiarTurn {
    pub fn new(start: f64, end: f64, speaker: u32) -> Self {
        Self {
            start,
            end,
            speaker,
        }
    }
}

/// The stable, human-facing label for an engine speaker id: `0 -> "S1"`.
pub fn speaker_label(engine_speaker: u32) -> String {
    format!("S{}", engine_speaker.saturating_add(1))
}

/// Everything the pipeline produces for one meeting, ready to persist and/or
/// export. Speakers are deduplicated; segments are `seq`-ordered and each
/// carries its resolved [`Speaker::id`].
#[derive(Debug, Clone)]
pub struct AssembledMeeting {
    /// Deduplicated speaker rows (one per distinct engine speaker id), ordered
    /// by engine id so labels read `S1, S2, ...`.
    pub speakers: Vec<Speaker>,
    /// Transcript segments in turn order (`seq = 0..n`).
    pub segments: Vec<TranscriptSegment>,
    /// The equivalent `lumen-transcript.v1` document (multi-segment + speaker
    /// table), suitable for export/interchange.
    pub transcript: TranscriptV1,
}

/// Assemble diarization turns plus their transcribed text into storage rows and
/// an interchange document for `meeting_id`.
///
/// `turns`, `texts`, and `words` are zipped positionally; excess of any is
/// ignored (`words` shorter than `texts` simply leaves later segments without
/// word timing). Engine speaker ids are deduplicated into [`Speaker`] rows
/// labelled `S{id+1}`; each turn becomes one segment referencing its speaker.
///
/// `words[i]` holds the word-level timings for turn `i`, already offset into
/// **absolute** media time by the caller (see
/// [`transcribe_turn`](crate::pipeline::transcribe_turn)). An empty inner vec
/// leaves that segment's `words` absent (`None`) so payloads for engines
/// without alignment (SenseVoice fallback) stay byte-for-byte unchanged.
///
/// `sample_rate` and `duration_seconds` are recorded in the transcript's
/// `media` block only (informational); pass `None`/`0` when unknown.
pub fn assemble_meeting(
    meeting_id: Uuid,
    turns: &[DiarTurn],
    texts: &[String],
    words: &[Vec<Word>],
    sample_rate: Option<u32>,
    duration_seconds: Option<f64>,
) -> AssembledMeeting {
    assemble_meeting_with_channels(
        meeting_id,
        turns,
        texts,
        words,
        &[],
        sample_rate,
        duration_seconds,
    )
}

/// [`assemble_meeting`] for dual-track (mic + system audio) meetings:
/// `channels[i]` tags segment `i` with the capture track it came from (see
/// [`SegmentChannel`](lumen_core::SegmentChannel)). An empty (or short)
/// `channels` slice leaves the remaining segments untagged (`None`), which is
/// exactly the legacy single-track behavior — [`assemble_meeting`] delegates
/// here with `&[]`.
pub fn assemble_meeting_with_channels(
    meeting_id: Uuid,
    turns: &[DiarTurn],
    texts: &[String],
    words: &[Vec<Word>],
    channels: &[lumen_core::SegmentChannel],
    sample_rate: Option<u32>,
    duration_seconds: Option<f64>,
) -> AssembledMeeting {
    // Distinct engine speaker ids, ascending, so S1/S2/... are stable.
    let mut ids: Vec<u32> = turns.iter().map(|t| t.speaker).collect();
    ids.sort_unstable();
    ids.dedup();

    let speakers: Vec<Speaker> = ids
        .iter()
        .map(|&engine_id| Speaker::new(meeting_id, speaker_label(engine_id)))
        .collect();

    // engine speaker id -> row (for segment.speaker_id) and label (for export).
    let speaker_for = |engine_id: u32| -> &Speaker {
        let idx = ids.binary_search(&engine_id).expect("id came from `ids`");
        &speakers[idx]
    };

    let n = turns.len().min(texts.len());
    let mut segments = Vec::with_capacity(n);
    let mut t_segments = Vec::with_capacity(n);
    for (seq, (turn, text)) in turns.iter().zip(texts.iter()).take(n).enumerate() {
        let speaker = speaker_for(turn.speaker);
        let seq_u32 = seq as u32;

        // Word timings for this turn (absolute media time); empty -> no timing.
        let turn_words = words.get(seq).filter(|w| !w.is_empty()).cloned();

        let mut segment =
            TranscriptSegment::new(meeting_id, seq_u32, turn.start, turn.end, text.clone());
        segment.speaker_id = Some(speaker.id);
        segment.words = turn_words.clone();
        segment.channel = channels.get(seq).copied();
        segments.push(segment);

        let mut t_segment = TSegment::new(turn.start, turn.end, text.clone())
            .with_id(seq.to_string())
            .with_speaker(speaker.label.clone());
        if let Some(w) = turn_words {
            t_segment = t_segment.with_words(w);
        }
        t_segments.push(t_segment);
    }

    let t_speakers: Vec<TSpeaker> = speakers
        .iter()
        .map(|s| TSpeaker::new(s.label.clone()))
        .collect();

    let media = Media {
        sample_rate,
        duration_seconds,
        ..Media::default()
    };

    let transcript = TranscriptV1::new(t_segments)
        .with_provenance(Provenance {
            app: "lumen-meeting".to_string(),
            engine: Some("diar-rs+asr".to_string()),
            ..Provenance::default()
        })
        .with_media(media)
        .with_speakers(t_speakers);

    AssembledMeeting {
        speakers,
        segments,
        transcript,
    }
}

/// Clamp a turn's `[start, end)` seconds to a valid sample range within a
/// `total_samples`-long buffer at `sample_rate`. Returns `None` for an empty
/// (or fully out-of-bounds) range so the caller can emit an empty segment
/// instead of calling ASR on zero samples.
pub fn turn_sample_range(
    start: f64,
    end: f64,
    sample_rate: u32,
    total_samples: usize,
) -> Option<(usize, usize)> {
    if !(start.is_finite() && end.is_finite()) || sample_rate == 0 {
        return None;
    }
    let sr = sample_rate as f64;
    let a = (start.max(0.0) * sr).floor() as usize;
    let b = (end.max(0.0) * sr).ceil() as usize;
    let a = a.min(total_samples);
    let b = b.min(total_samples);
    if b > a {
        Some((a, b))
    } else {
        None
    }
}

/// Build the initial `Meeting` row for a wav-backed offline run.
pub fn new_meeting(audio_path: Option<String>, duration_seconds: Option<f64>) -> Meeting {
    let mut meeting = Meeting::new();
    meeting.audio_path = audio_path;
    meeting.duration_seconds = duration_seconds;
    meeting.status = lumen_core::MeetingStatus::Processing;
    meeting
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns() -> Vec<DiarTurn> {
        vec![
            DiarTurn::new(0.0, 2.0, 0),
            DiarTurn::new(2.0, 5.0, 1),
            DiarTurn::new(5.0, 6.5, 0),
        ]
    }

    #[test]
    fn labels_are_one_based() {
        assert_eq!(speaker_label(0), "S1");
        assert_eq!(speaker_label(3), "S4");
    }

    #[test]
    fn speakers_are_deduped_and_labelled_in_order() {
        let mid = Uuid::new_v4();
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = assemble_meeting(mid, &turns(), &texts, &[], Some(16_000), Some(6.5));

        assert_eq!(out.speakers.len(), 2, "two distinct engine ids -> two rows");
        assert_eq!(out.speakers[0].label, "S1");
        assert_eq!(out.speakers[1].label, "S2");
        for s in &out.speakers {
            assert_eq!(s.meeting_id, mid);
        }
    }

    #[test]
    fn segments_are_seq_ordered_and_reference_correct_speaker() {
        let mid = Uuid::new_v4();
        let texts = vec![
            "hello".to_string(),
            "world".to_string(),
            "again".to_string(),
        ];
        let out = assemble_meeting(mid, &turns(), &texts, &[], None, None);

        assert_eq!(out.segments.len(), 3);
        for (i, seg) in out.segments.iter().enumerate() {
            assert_eq!(seg.seq, i as u32);
            assert_eq!(seg.meeting_id, mid);
        }
        assert_eq!(out.segments[0].text, "hello");
        // Turn 0 and turn 2 are both engine speaker 0 -> same speaker row.
        assert_eq!(out.segments[0].speaker_id, out.segments[2].speaker_id);
        assert_ne!(out.segments[0].speaker_id, out.segments[1].speaker_id);

        // The referenced ids are real speaker rows.
        let s1 = out.speakers.iter().find(|s| s.label == "S1").unwrap();
        assert_eq!(out.segments[0].speaker_id, Some(s1.id));
    }

    #[test]
    fn transcript_is_multi_segment_with_speaker_labels() {
        let mid = Uuid::new_v4();
        let texts = vec![
            "hello".to_string(),
            "world".to_string(),
            "again".to_string(),
        ];
        let out = assemble_meeting(mid, &turns(), &texts, &[], Some(16_000), Some(6.5));
        let t = &out.transcript;

        assert_eq!(t.segments.len(), 3);
        assert_eq!(t.segments[0].speaker.as_deref(), Some("S1"));
        assert_eq!(t.segments[1].speaker.as_deref(), Some("S2"));
        assert_eq!(t.segments[0].start, 0.0);
        assert_eq!(t.segments[1].end, 5.0);

        let speakers = t.speakers.as_ref().unwrap();
        assert_eq!(speakers.len(), 2);
        assert_eq!(speakers[0].id, "S1");

        assert_eq!(t.media.as_ref().unwrap().sample_rate, Some(16_000));
        assert_eq!(t.provenance.as_ref().unwrap().app, "lumen-meeting");

        // Round-trips through the interchange JSON.
        let json = t.to_json_string().unwrap();
        let back = TranscriptV1::from_json_str(&json).unwrap();
        assert_eq!(&back, t);
    }

    #[test]
    fn mismatched_turns_and_texts_take_the_shorter() {
        let mid = Uuid::new_v4();
        let texts = vec!["only-one".to_string()];
        let out = assemble_meeting(mid, &turns(), &texts, &[], None, None);
        assert_eq!(out.segments.len(), 1);
        // Speakers still reflect all turns (dedup over the full turn list).
        assert_eq!(out.speakers.len(), 2);
    }

    #[test]
    fn per_turn_words_land_on_segments_and_transcript() {
        let mid = Uuid::new_v4();
        let texts = vec!["你好".to_string(), "世界".to_string(), "".to_string()];
        // Word timings are already in absolute media time (offset applied upstream).
        let words = vec![
            vec![Word::new("你", 0.1, 0.4), Word::new("好", 0.4, 0.8)],
            vec![Word::new("世", 2.2, 2.5), Word::new("界", 2.5, 2.9)],
            // Third turn has no alignment (e.g. SenseVoice fallback / empty audio).
            vec![],
        ];
        let out = assemble_meeting(mid, &turns(), &texts, &words, Some(16_000), Some(6.5));

        // Stored segment carries the words verbatim.
        let s0 = out.segments[0].words.as_ref().expect("turn 0 words");
        assert_eq!(s0.len(), 2);
        assert_eq!(s0[0].word, "你");
        assert_eq!(s0[0].start, 0.1);
        assert_eq!(out.segments[1].words.as_ref().unwrap()[1].word, "界");
        // Empty inner vec -> absent, not Some(vec![]).
        assert!(out.segments[2].words.is_none());

        // Interchange transcript mirrors the same word timing.
        let t0 = out.transcript.segments[0].words.as_ref().expect("t words");
        assert_eq!(t0.len(), 2);
        assert_eq!(t0[1].end, 0.8);
        assert!(out.transcript.segments[2].words.is_none());

        // Round-trips through the interchange JSON with words intact.
        let json = out.transcript.to_json_string().unwrap();
        let back = TranscriptV1::from_json_str(&json).unwrap();
        assert_eq!(&back, &out.transcript);
    }

    #[test]
    fn fewer_words_than_turns_leaves_later_segments_unaligned() {
        let mid = Uuid::new_v4();
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // Only the first turn has words.
        let words = vec![vec![Word::new("a", 0.0, 1.0)]];
        let out = assemble_meeting(mid, &turns(), &texts, &words, None, None);
        assert!(out.segments[0].words.is_some());
        assert!(out.segments[1].words.is_none());
        assert!(out.segments[2].words.is_none());
    }

    #[test]
    fn assemble_with_channels_tags_segments_and_plain_assemble_leaves_none() {
        use lumen_core::SegmentChannel;

        let mid = Uuid::new_v4();
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let channels = vec![
            SegmentChannel::Mic,
            SegmentChannel::System,
            SegmentChannel::Mic,
        ];
        let out = assemble_meeting_with_channels(mid, &turns(), &texts, &[], &channels, None, None);
        assert_eq!(out.segments[0].channel, Some(SegmentChannel::Mic));
        assert_eq!(out.segments[1].channel, Some(SegmentChannel::System));
        assert_eq!(out.segments[2].channel, Some(SegmentChannel::Mic));

        // Legacy single-track assembly stores no channel at all.
        let legacy = assemble_meeting(mid, &turns(), &texts, &[], None, None);
        assert!(legacy.segments.iter().all(|s| s.channel.is_none()));
        // And apart from ids/channel, the shapes match (same speakers/segments).
        assert_eq!(legacy.segments.len(), out.segments.len());
        assert_eq!(legacy.speakers.len(), out.speakers.len());
    }

    #[test]
    fn sample_range_clamps_and_rejects_empty() {
        // 1 s at 16 kHz = samples [0, 16000).
        assert_eq!(
            turn_sample_range(0.0, 1.0, 16_000, 32_000),
            Some((0, 16_000))
        );
        // End past the buffer clamps to total.
        assert_eq!(
            turn_sample_range(1.0, 99.0, 16_000, 20_000),
            Some((16_000, 20_000))
        );
        // Fully out of bounds / zero-length -> None.
        assert_eq!(turn_sample_range(5.0, 5.0, 16_000, 16_000), None);
        assert_eq!(turn_sample_range(10.0, 11.0, 16_000, 16_000), None);
        assert_eq!(turn_sample_range(0.0, 1.0, 0, 16_000), None);
    }
}
