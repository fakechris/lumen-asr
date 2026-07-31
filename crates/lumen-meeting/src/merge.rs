//! Pure merging of two per-track transcriptions (mic + system audio) into one
//! chronological take.
//!
//! Dual-track meetings diarize and transcribe each WAV **independently**: the
//! mic track carries the local user, the system track carries the remote
//! participants. Because the two diarizations cluster independently, their
//! engine speaker ids both start at `0` — so before combining, every system
//! speaker id is offset past the mic track's ids (mic keeps `S1..Sn`, system
//! becomes `S(n+1)..`), guaranteeing a mic speaker and a remote speaker are
//! never collapsed into one label. Segments are then interleaved by start
//! time so the assembled transcript reads in wall-clock order, and each one
//! carries its [`SegmentChannel`] so "我" (mic) and "对方" (system) stay
//! distinguishable downstream.
//!
//! No audio, models, or I/O here — everything is unit-testable with stub data.

use lumen_core::SegmentChannel;
use lumen_transcript::Word;

use crate::assemble::DiarTurn;

/// One track's diarize + per-turn ASR output, positionally zipped
/// (`turns[i]` ↔ `texts[i]` ↔ `words[i]`).
#[derive(Debug, Clone, Default)]
pub struct TrackTake {
    pub turns: Vec<DiarTurn>,
    pub texts: Vec<String>,
    pub words: Vec<Vec<Word>>,
}

/// The chronological, channel-tagged union of both tracks. Vectors stay
/// positionally zipped; `channels[i]` says which track segment `i` came from.
#[derive(Debug, Clone, Default)]
pub struct MergedTake {
    pub turns: Vec<DiarTurn>,
    pub texts: Vec<String>,
    pub words: Vec<Vec<Word>>,
    pub channels: Vec<SegmentChannel>,
}

/// The offset added to every system-track engine speaker id so system speaker
/// labels never collide with mic ones: one past the highest mic id (`0` when
/// the mic track produced no turns).
pub fn system_speaker_offset(mic_turns: &[DiarTurn]) -> u32 {
    mic_turns
        .iter()
        .map(|t| t.speaker.saturating_add(1))
        .max()
        .unwrap_or(0)
}

/// Merge the mic take and the system take into one chronological,
/// channel-tagged take. System speaker ids are shifted by
/// [`system_speaker_offset`]; ordering is by turn start time (stable, so
/// equal-start segments keep mic before system).
pub fn merge_tracks(mic: TrackTake, system: TrackTake) -> MergedTake {
    let offset = system_speaker_offset(&mic.turns);

    // Positionally zip each track; excess of any vector is ignored, matching
    // `assemble_meeting`'s zip contract (a missing words entry means no
    // word-level timing for that turn).
    let mut entries: Vec<(DiarTurn, String, Vec<Word>, SegmentChannel)> = Vec::new();
    let mic_n = mic.turns.len().min(mic.texts.len());
    for i in 0..mic_n {
        entries.push((
            mic.turns[i],
            mic.texts[i].clone(),
            mic.words.get(i).cloned().unwrap_or_default(),
            SegmentChannel::Mic,
        ));
    }
    let sys_n = system.turns.len().min(system.texts.len());
    for i in 0..sys_n {
        let mut turn = system.turns[i];
        turn.speaker = turn.speaker.saturating_add(offset);
        entries.push((
            turn,
            system.texts[i].clone(),
            system.words.get(i).cloned().unwrap_or_default(),
            SegmentChannel::System,
        ));
    }

    // Stable sort by start time: NaN-free in practice (diarization emits real
    // times), but order NaN last deterministically rather than panicking.
    entries.sort_by(|a, b| {
        a.0.start
            .partial_cmp(&b.0.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut merged = MergedTake::default();
    for (turn, text, words, channel) in entries {
        merged.turns.push(turn);
        merged.texts.push(text);
        merged.words.push(words);
        merged.channels.push(channel);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(turns: Vec<DiarTurn>, texts: Vec<&str>) -> TrackTake {
        TrackTake {
            turns,
            texts: texts.into_iter().map(str::to_string).collect(),
            words: Vec::new(),
        }
    }

    #[test]
    fn offset_is_one_past_the_highest_mic_speaker_id() {
        assert_eq!(system_speaker_offset(&[]), 0);
        assert_eq!(system_speaker_offset(&[DiarTurn::new(0.0, 1.0, 0)]), 1);
        assert_eq!(
            system_speaker_offset(&[DiarTurn::new(0.0, 1.0, 2), DiarTurn::new(1.0, 2.0, 0)]),
            3
        );
    }

    #[test]
    fn merge_interleaves_by_start_time_and_tags_channels() {
        // Mic: user speaks at 0-2 and 5-6. System: remote replies at 2-5.
        let mic = take(
            vec![DiarTurn::new(0.0, 2.0, 0), DiarTurn::new(5.0, 6.0, 0)],
            vec!["我先说", "我再说"],
        );
        let system = take(vec![DiarTurn::new(2.0, 5.0, 0)], vec!["对方回复"]);

        let merged = merge_tracks(mic, system);
        assert_eq!(merged.texts, vec!["我先说", "对方回复", "我再说"]);
        assert_eq!(
            merged.channels,
            vec![
                SegmentChannel::Mic,
                SegmentChannel::System,
                SegmentChannel::Mic
            ]
        );
        // Chronological: starts are non-decreasing.
        let starts: Vec<f64> = merged.turns.iter().map(|t| t.start).collect();
        assert_eq!(starts, vec![0.0, 2.0, 5.0]);
    }

    #[test]
    fn merge_offsets_system_speaker_ids_past_mic_ones() {
        // Mic clustered two speakers (0, 1); system independently clustered two
        // speakers that also start at 0 — they must not collide.
        let mic = take(
            vec![DiarTurn::new(0.0, 1.0, 0), DiarTurn::new(1.0, 2.0, 1)],
            vec!["a", "b"],
        );
        let system = take(
            vec![DiarTurn::new(2.0, 3.0, 0), DiarTurn::new(3.0, 4.0, 1)],
            vec!["c", "d"],
        );

        let merged = merge_tracks(mic, system);
        let ids: Vec<u32> = merged.turns.iter().map(|t| t.speaker).collect();
        // Mic keeps 0/1; system 0/1 become 2/3 → labels S1..S4, no collision.
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn merge_with_empty_system_take_keeps_mic_order_and_tags_mic() {
        let mic = take(
            vec![DiarTurn::new(0.0, 1.0, 0), DiarTurn::new(1.0, 2.0, 1)],
            vec!["a", "b"],
        );
        let merged = merge_tracks(mic, TrackTake::default());
        assert_eq!(merged.texts, vec!["a", "b"]);
        assert_eq!(
            merged.channels,
            vec![SegmentChannel::Mic, SegmentChannel::Mic]
        );
        let ids: Vec<u32> = merged.turns.iter().map(|t| t.speaker).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn merge_with_empty_mic_take_starts_system_labels_at_zero() {
        let system = take(vec![DiarTurn::new(0.0, 1.0, 0)], vec!["remote"]);
        let merged = merge_tracks(TrackTake::default(), system);
        assert_eq!(merged.turns[0].speaker, 0);
        assert_eq!(merged.channels, vec![SegmentChannel::System]);
    }

    #[test]
    fn merge_preserves_word_timings_per_segment() {
        let mic = TrackTake {
            turns: vec![DiarTurn::new(0.0, 1.0, 0)],
            texts: vec!["你好".into()],
            words: vec![vec![Word::new("你", 0.0, 0.4), Word::new("好", 0.4, 0.8)]],
        };
        let system = TrackTake {
            turns: vec![DiarTurn::new(0.5, 1.5, 0)],
            texts: vec!["hello".into()],
            words: vec![vec![Word::new("hello", 0.5, 1.4)]],
        };
        let merged = merge_tracks(mic, system);
        assert_eq!(merged.words[0].len(), 2);
        assert_eq!(merged.words[0][0].word, "你");
        assert_eq!(merged.words[1].len(), 1);
        assert_eq!(merged.words[1][0].word, "hello");
    }

    #[test]
    fn merge_is_stable_for_equal_start_times() {
        // Same start: mic entry stays ahead of the system entry.
        let mic = take(vec![DiarTurn::new(1.0, 2.0, 0)], vec!["mic"]);
        let system = take(vec![DiarTurn::new(1.0, 2.0, 0)], vec!["sys"]);
        let merged = merge_tracks(mic, system);
        assert_eq!(merged.texts, vec!["mic", "sys"]);
        assert_eq!(
            merged.channels,
            vec![SegmentChannel::Mic, SegmentChannel::System]
        );
    }
}
