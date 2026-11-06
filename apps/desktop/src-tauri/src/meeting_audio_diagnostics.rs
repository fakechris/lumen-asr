//! Per-meeting audio pipeline diagnostics, written next to the WAVs at stop
//! time (`<meeting-id>.audio_diagnostics.json`, best-effort).
//!
//! The recorder already warns about capture stalls in the log; this sidecar
//! gives the same story a durable, per-meeting home: per-track sample rate,
//! duration, padded gaps, live fan-out drop counts, and — when the cloud
//! realtime layer ran — per-track session health (packets fed, sample-rate
//! changes, timeline drift, session failures).
//!
//! Purely observational: nothing here feeds back into recording or
//! processing, and every failure to write is a warning, never an error.

use std::path::Path;

/// One padded capture stall (system sleep / App Nap), as detected by the
/// recorder's gap clock and back-filled with silence into the WAV.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapEvent {
    /// Position on the meeting timeline (seconds since t0).
    pub start_seconds: f64,
    pub duration_seconds: f64,
}

/// A mid-recording stream change observed on a track. The recorder's device
/// stream is fixed for the meeting's lifetime (no hot-swap rebuild), so the
/// observable change today is a capture sample-rate change reported by the
/// cloud realtime pump's sample clock; a future device watcher would append
/// `kind: "device"` entries to the same list.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamChangeEvent {
    /// Timeline position (seconds since t0) where the change was observed.
    pub at_seconds: f64,
    /// "sample_rate" today; "device" reserved for a future watcher.
    pub kind: String,
    pub from: String,
    pub to: String,
}

/// Cloud realtime session health for one track (present only when the
/// realtime layer ran for that track).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeTrackDiagnostics {
    /// Cloud sessions opened for this track (1 today; higher once a
    /// reconnect loop exists).
    pub sessions_started: u32,
    /// Sessions re-opened after an in-meeting failure (0 today — a failure
    /// ends the track's realtime; the offline pipeline still gap-fills).
    pub reconnects: u32,
    /// Failure messages from ended sessions (connect / send / provider).
    pub failures: Vec<String>,
    /// Audio packets observed from the track's fan-out.
    pub packets_observed: u64,
    /// Samples fed into the provider resampler (native capture rate).
    pub samples_fed: u64,
    /// Provider resampler constructions (one per session).
    pub resampler_initializations: u32,
    pub stream_change_events: Vec<StreamChangeEvent>,
    /// Drift between callback-arrival time and the sample-count timeline:
    /// `(last_arrival - first_arrival) - samples_fed / rate`. Healthy
    /// real-time delivery reads as ≈ -(one packet duration); values near
    /// zero or positive mean the pump received less audio than wall time
    /// delivered (upstream drops / stalls); strongly negative means
    /// over-delivery.
    pub clock_drift_seconds: Option<f64>,
    /// True when the stop path's bounded wait for the session to finish
    /// timed out: whatever was collected is partial, and a `None` drift then
    /// means "not finalized", not "no audio".
    pub collection_timed_out: bool,
    /// Finalized utterances collected from the session(s).
    pub utterance_count: usize,
}

/// Diagnostics for one recorded track (mic or system).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackAudioDiagnostics {
    /// "mic" | "system"
    pub track: String,
    /// WAV sample rate (0 when the track never started).
    pub sample_rate: u32,
    /// Recorded duration (silence padding included).
    pub duration_seconds: f64,
    /// Live fan-out packets dropped because a consumer fell behind (the WAV
    /// write path is separate and unaffected).
    pub tap_dropped_packets: u64,
    pub gaps: Vec<GapEvent>,
    pub gap_total_seconds: f64,
    /// Cloud realtime health; absent when realtime was off or never started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realtime: Option<RealtimeTrackDiagnostics>,
}

/// Top-level diagnostics document for one meeting.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingAudioDiagnostics {
    pub schema: u32,
    pub meeting_id: String,
    pub tracks: Vec<TrackAudioDiagnostics>,
}

/// Handles retained across one recording so the stop path can read the live
/// fan-out drop counters: a `LiveTapSender` clone shares its counter with the
/// clones the recorders hold, so drops stay readable after the recorders
/// have torn their sinks down. Stored in `AppState` at start, consumed at stop.
pub struct MeetingAudioDiagSession {
    pub meeting_id: uuid::Uuid,
    pub mic_tap: Option<lumen_asr::LiveTapSender>,
    pub system_tap: Option<lumen_asr::LiveTapSender>,
}

impl TrackAudioDiagnostics {
    /// Assemble one track's diagnostics from its stop summary. `sample_rate`
    /// / `duration_seconds` / gaps come from the recorder's `RecordingSummary`
    /// (`None` when the track never started); the drop count comes from the
    /// retained tap handle (`None` when the track ran without a fan-out).
    pub fn from_summary(
        track: &str,
        sample_rate: u32,
        duration_seconds: f64,
        gaps: &[crate::meeting_audio_diagnostics::GapEvent],
        tap_dropped_packets: Option<u64>,
    ) -> Self {
        Self {
            track: track.to_string(),
            sample_rate,
            duration_seconds,
            tap_dropped_packets: tap_dropped_packets.unwrap_or(0),
            gaps: gaps.to_vec(),
            gap_total_seconds: gaps.iter().map(|g| g.duration_seconds).sum(),
            realtime: None,
        }
    }
}

impl From<crate::realtime_asr::RealtimePumpDiagnostics> for RealtimeTrackDiagnostics {
    fn from(p: crate::realtime_asr::RealtimePumpDiagnostics) -> Self {
        Self {
            sessions_started: p.sessions_started,
            reconnects: p.reconnects,
            failures: p.failures,
            packets_observed: p.packets_observed,
            samples_fed: p.samples_fed,
            resampler_initializations: p.resampler_initializations,
            stream_change_events: p.stream_change_events,
            clock_drift_seconds: p.clock_drift_seconds,
            collection_timed_out: p.collection_timed_out,
            utterance_count: p.utterance_count,
        }
    }
}

/// Write the diagnostics sidecar next to the meeting WAVs. Best-effort: a
/// failure logs a warning and changes nothing else — the meeting itself is
/// never at risk from diagnostics. Returns `true` iff the file was written.
pub fn write_audio_diagnostics(
    dir: &Path,
    meeting_id: uuid::Uuid,
    diag: &MeetingAudioDiagnostics,
) -> bool {
    let path = dir.join(format!("{meeting_id}.audio_diagnostics.json"));
    match serde_json::to_string_pretty(diag) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "audio diagnostics write failed");
                false
            }
        },
        Err(error) => {
            tracing::warn!(%error, "audio diagnostics serialize failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_diag() -> MeetingAudioDiagnostics {
        MeetingAudioDiagnostics {
            schema: 1,
            meeting_id: "0194cf3a-1111-7111-8222-333344445555".into(),
            tracks: vec![
                TrackAudioDiagnostics {
                    track: "mic".into(),
                    sample_rate: 48_000,
                    duration_seconds: 1_832.4,
                    tap_dropped_packets: 7,
                    gaps: vec![GapEvent {
                        start_seconds: 905.0,
                        duration_seconds: 12.5,
                    }],
                    gap_total_seconds: 12.5,
                    realtime: Some(RealtimeTrackDiagnostics {
                        sessions_started: 1,
                        reconnects: 0,
                        failures: vec!["send: channel closed".into()],
                        packets_observed: 18_320,
                        samples_fed: 88_147_200,
                        resampler_initializations: 1,
                        stream_change_events: vec![StreamChangeEvent {
                            at_seconds: 917.5,
                            kind: "sample_rate".into(),
                            from: "16000".into(),
                            to: "44100".into(),
                        }],
                        clock_drift_seconds: Some(0.03),
                        collection_timed_out: false,
                        utterance_count: 214,
                    }),
                },
                TrackAudioDiagnostics {
                    track: "system".into(),
                    sample_rate: 48_000,
                    duration_seconds: 1_830.1,
                    tap_dropped_packets: 0,
                    gaps: Vec::new(),
                    gap_total_seconds: 0.0,
                    realtime: None,
                },
            ],
        }
    }

    #[test]
    fn diagnostics_roundtrip() {
        let diag = sample_diag();
        let json = serde_json::to_string(&diag).unwrap();
        let back: MeetingAudioDiagnostics = serde_json::from_str(&json).unwrap();
        assert_eq!(back, diag);
        // The realtime section is optional and omitted when absent.
        let bare_diag = MeetingAudioDiagnostics {
            schema: 1,
            meeting_id: "m".into(),
            tracks: vec![TrackAudioDiagnostics {
                track: "mic".into(),
                ..Default::default()
            }],
        };
        let bare: MeetingAudioDiagnostics =
            serde_json::from_str(&serde_json::to_string(&bare_diag).unwrap()).unwrap();
        assert!(bare.tracks[0].realtime.is_none());
    }

    #[test]
    fn gap_total_is_reported_per_track() {
        let mut track = TrackAudioDiagnostics {
            track: "mic".into(),
            ..Default::default()
        };
        track.gaps = vec![
            GapEvent {
                start_seconds: 1.0,
                duration_seconds: 3.0,
            },
            GapEvent {
                start_seconds: 10.0,
                duration_seconds: 2.5,
            },
        ];
        track.gap_total_seconds = track.gaps.iter().map(|g| g.duration_seconds).sum();
        assert!((track.gap_total_seconds - 5.5).abs() < 1e-9);
    }

    #[test]
    fn write_is_best_effort() {
        // Writing into a path that is a *file*, not a directory, must warn
        // and carry on — never panic or propagate.
        let file_as_dir =
            std::env::temp_dir().join(format!("lumen-audiodiag-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file_as_dir, b"not a dir").unwrap();
        write_audio_diagnostics(&file_as_dir, uuid::Uuid::new_v4(), &sample_diag());
        std::fs::remove_file(&file_as_dir).unwrap();
    }

    #[test]
    fn write_succeeds_into_real_dir() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-audiodiag-ok-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        let id = uuid::Uuid::new_v4();
        write_audio_diagnostics(&dir, id, &sample_diag());
        let path = dir.join(format!("{id}.audio_diagnostics.json"));
        assert!(path.is_file());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("sampleRate"),
            "camelCase keys expected: {text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
