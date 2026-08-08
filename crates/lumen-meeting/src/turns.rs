//! Post-diarization turn cleanup: absorb short fragments that create false
//! extra speakers (e.g. cough / noise as a one-second "S2").

use crate::assemble::DiarTurn;

/// Default minimum turn length (seconds). Fragments shorter than this are
/// absorbed into a neighbouring turn (see [`merge_short_diar_turns`]).
pub const DEFAULT_MIN_TURN_SECONDS: f64 = 1.5;

/// Merge short diarization turns so brief noise spikes do not become their own
/// "speaker".
///
/// Algorithm (single pass, left-to-right):
/// 1. Drop zero/negative-length turns.
/// 2. Any turn shorter than `min_seconds` is absorbed into the **previous**
///    turn when one exists (extend previous `end`); otherwise it is absorbed
///    into the **next** turn (pull next `start` earlier). If it is the only
///    turn, keep it.
/// 3. After absorption, collapse consecutive runs of the same speaker id into
///    one continuous turn.
///
/// This is intentionally conservative: it never invents a new speaker label and
/// never reorders turns.
pub fn merge_short_diar_turns(turns: &[DiarTurn], min_seconds: f64) -> Vec<DiarTurn> {
    let min_seconds = min_seconds.max(0.0);
    let mut raw: Vec<DiarTurn> = turns
        .iter()
        .copied()
        .filter(|t| t.end > t.start)
        .collect();
    if raw.is_empty() {
        return Vec::new();
    }

    // Absorb short fragments.
    let mut i = 0;
    while i < raw.len() {
        let dur = raw[i].end - raw[i].start;
        if dur + f64::EPSILON >= min_seconds {
            i += 1;
            continue;
        }
        if i > 0 {
            // Absorb into previous: extend previous end to cover this fragment.
            let new_end = raw[i].end.max(raw[i - 1].end);
            raw[i - 1].end = new_end;
            raw.remove(i);
            // Stay on same i (now the next element).
        } else if raw.len() > 1 {
            // First and short: absorb into next.
            let new_start = raw[i].start.min(raw[i + 1].start);
            raw[i + 1].start = new_start;
            raw.remove(i);
        } else {
            // Sole turn: keep even if short.
            break;
        }
    }

    // Collapse consecutive same-speaker runs.
    let mut out: Vec<DiarTurn> = Vec::with_capacity(raw.len());
    for t in raw {
        if let Some(last) = out.last_mut() {
            if last.speaker == t.speaker {
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
        let turns = vec![
            t(0.0, 100.0, 0),
            t(100.0, 100.75, 1),
            t(100.75, 200.0, 0),
        ];
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
}
