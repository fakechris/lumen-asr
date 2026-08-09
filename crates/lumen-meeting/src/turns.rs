//! Post-diarization turn cleanup: absorb short fragments that create false
//! extra speakers (e.g. cough / noise as a one-second "S2").

use crate::assemble::DiarTurn;

/// Default minimum turn length (seconds). Fragments shorter than this are
/// absorbed into a neighbouring turn (see [`merge_short_diar_turns`]).
pub const DEFAULT_MIN_TURN_SECONDS: f64 = 1.5;

/// Default maximum silence gap (seconds) across which a short fragment may be
/// absorbed into a neighbour. Larger gaps leave the fragment alone so ASR is
/// not handed a multi-second silence-padded slice (same failure mode as long
/// turns OOM'ing the engine).
pub const DEFAULT_MAX_MERGE_GAP_SECONDS: f64 = 2.0;

/// Merge short diarization turns so brief noise spikes do not become their own
/// "speaker".
///
/// Uses [`DEFAULT_MAX_MERGE_GAP_SECONDS`] as the absorb gap limit. See
/// [`merge_short_diar_turns_with_gap`] for the full algorithm.
pub fn merge_short_diar_turns(turns: &[DiarTurn], min_seconds: f64) -> Vec<DiarTurn> {
    merge_short_diar_turns_with_gap(turns, min_seconds, DEFAULT_MAX_MERGE_GAP_SECONDS)
}

/// Merge short diarization turns with an explicit absorb gap limit.
///
/// Algorithm (single pass, left-to-right):
/// 1. Drop zero/negative-length turns.
/// 2. Any turn shorter than `min_seconds` is absorbed into a **neighbour** only
///    when the silence gap to that neighbour is ≤ `max_gap_seconds`:
///    - Prefer the **previous** turn when the gap is small enough (extend
///      previous `end`).
///    - Else prefer the **next** turn when the gap is small enough (pull next
///      `start` earlier).
///    - Else leave the short turn alone (isolated noise / distant fragment).
///    - Sole short turn: keep it.
/// 3. After absorption, collapse consecutive runs of the same speaker id into
///    one continuous turn.
///
/// This is intentionally conservative: it never invents a new speaker label and
/// never reorders turns. Gap protection prevents a 0.5s cough 30s after a
/// monologue from stretching the monologue's ASR window across the silence.
pub fn merge_short_diar_turns_with_gap(
    turns: &[DiarTurn],
    min_seconds: f64,
    max_gap_seconds: f64,
) -> Vec<DiarTurn> {
    let min_seconds = min_seconds.max(0.0);
    let max_gap = max_gap_seconds.max(0.0);
    let mut raw: Vec<DiarTurn> = turns.iter().copied().filter(|t| t.end > t.start).collect();
    if raw.is_empty() {
        return Vec::new();
    }

    // Absorb short fragments when a neighbour is close enough in time.
    let mut i = 0;
    while i < raw.len() {
        let dur = raw[i].end - raw[i].start;
        if dur + f64::EPSILON >= min_seconds {
            i += 1;
            continue;
        }

        let gap_prev = if i > 0 {
            Some((raw[i].start - raw[i - 1].end).max(0.0))
        } else {
            None
        };
        let gap_next = if i + 1 < raw.len() {
            Some((raw[i + 1].start - raw[i].end).max(0.0))
        } else {
            None
        };

        let can_prev = gap_prev.is_some_and(|g| g <= max_gap + f64::EPSILON);
        let can_next = gap_next.is_some_and(|g| g <= max_gap + f64::EPSILON);

        if can_prev {
            // Absorb into previous: extend previous end to cover this fragment.
            let new_end = raw[i].end.max(raw[i - 1].end);
            raw[i - 1].end = new_end;
            raw.remove(i);
            // Stay on same i (now the next element).
        } else if can_next {
            // Absorb into next (leading short, or previous was too far).
            let new_start = raw[i].start.min(raw[i + 1].start);
            raw[i + 1].start = new_start;
            raw.remove(i);
        } else {
            // Isolated short fragment (or sole turn): keep without stretching
            // a distant neighbour across silence.
            i += 1;
        }
    }

    // Collapse consecutive same-speaker runs only when the gap between them is
    // small — otherwise two monologue chunks separated by long silence become
    // one giant ASR slice (same OOM risk as unbounded absorb).
    let mut out: Vec<DiarTurn> = Vec::with_capacity(raw.len());
    for t in raw {
        if let Some(last) = out.last_mut() {
            let gap = (t.start - last.end).max(0.0);
            if last.speaker == t.speaker && gap <= max_gap + f64::EPSILON {
                last.end = last.end.max(t.end);
                continue;
            }
        }
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(start: f64, end: f64, speaker: u32) -> DiarTurn {
        DiarTurn::new(start, end, speaker)
    }

    #[test]
    fn keeps_long_alternating_speakers() {
        let turns = vec![t(0.0, 10.0, 0), t(10.0, 20.0, 1), t(20.0, 30.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].speaker, 1);
    }

    #[test]
    fn absorbs_short_s2_into_previous_s1() {
        // Real pattern from the Spanish dogfood: long S1, 0.75s S2, long S1.
        let turns = vec![t(0.0, 100.0, 0), t(100.0, 100.75, 1), t(100.75, 200.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        // short S2 → absorbed into S1; consecutive S1 collapse → one turn
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, 0);
        assert!((out[0].start - 0.0).abs() < 1e-9);
        assert!((out[0].end - 200.0).abs() < 1e-9);
    }

    #[test]
    fn absorbs_leading_short_into_next() {
        let turns = vec![t(0.0, 0.4, 1), t(0.4, 30.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, 0);
        assert!((out[0].start - 0.0).abs() < 1e-9);
    }

    #[test]
    fn keeps_short_when_alone() {
        let turns = vec![t(0.0, 0.5, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn collapses_same_speaker_neighbours_after_absorb() {
        let turns = vec![
            t(0.0, 5.0, 0),
            t(5.0, 5.2, 1), // short noise
            t(5.2, 12.0, 0),
            t(12.0, 20.0, 1), // real second speaker, long enough
        ];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].speaker, 0);
        assert!((out[0].end - 12.0).abs() < 1e-9);
        assert_eq!(out[1].speaker, 1);
    }

    #[test]
    fn does_not_absorb_across_long_silence_into_previous() {
        // Short cough 30s after monologue must not stretch the monologue to 45s.
        let turns = vec![t(0.0, 10.0, 0), t(40.0, 40.5, 1), t(40.5, 60.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].speaker, 0);
        assert!((out[0].end - 10.0).abs() < 1e-9);
        // short absorbed into following S0 (gap_next ≈ 0)
        assert_eq!(out[1].speaker, 0);
        assert!((out[1].start - 40.0).abs() < 1e-9);
        assert!((out[1].end - 60.0).abs() < 1e-9);
    }

    #[test]
    fn leaves_isolated_short_when_both_neighbours_far() {
        let turns = vec![t(0.0, 5.0, 0), t(40.0, 40.4, 1), t(80.0, 90.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].speaker, 1);
        assert!((out[0].end - 5.0).abs() < 1e-9);
        assert!((out[2].start - 80.0).abs() < 1e-9);
    }

    #[test]
    fn max_gap_zero_still_absorbs_abutting_fragments() {
        // gap == 0 is within max_gap 0 → absorb + collapse still work.
        let turns = vec![t(0.0, 5.0, 0), t(5.0, 5.2, 1), t(5.2, 12.0, 0)];
        let out = merge_short_diar_turns_with_gap(&turns, 1.5, 0.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, 0);
    }

    #[test]
    fn does_not_collapse_same_speaker_across_long_silence() {
        let turns = vec![t(0.0, 10.0, 0), t(40.0, 50.0, 0)];
        let out = merge_short_diar_turns(&turns, 1.5);
        assert_eq!(out.len(), 2);
        assert!((out[0].end - 10.0).abs() < 1e-9);
        assert!((out[1].start - 40.0).abs() < 1e-9);
    }
}
