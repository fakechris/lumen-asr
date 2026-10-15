//! Background warm-up of the local dictation ASR engine at record start.
//!
//! The sherpa-onnx engines build their `OfflineRecognizer` lazily inside the
//! first `transcribe` and keep it for the life of the engine instance. Without
//! a warm-up the first dictation after launch (or after a model switch) pays
//! that model load — plus ONNX Runtime's first-inference allocations — after
//! the user has already stopped speaking (measured on Apple Silicon: ~0.8 s
//! for SenseVoice, ~3 s for Qwen3-ASR). Warming with a short silent decode
//! while the user is still talking moves that cost into the recording.
//!
//! The engine's own recognizer lock serialises the warm-up with the real
//! transcription, so a stop that arrives mid-warm simply waits for the load it
//! would otherwise have done itself; nothing is ever loaded twice.
//!
//! [`PrewarmBook`] remembers which engine kinds are already warm so later
//! recordings skip the extra decode. Any engine replacement or unload calls
//! [`invalidate`]; a warm-up that straddles an invalidation is not recorded.

use lumen_asr::{AsrEngine, AsrRequest, EngineKind};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 0.2 s of silence at 16 kHz: long enough for the feature frontend, short
/// enough that the throwaway decode is a few milliseconds once loaded.
const WARM_SAMPLES: usize = 3_200;
const WARM_SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Default)]
struct PrewarmBook {
    epoch: u64,
    warm: Vec<EngineKind>,
    in_flight: Vec<EngineKind>,
}

impl PrewarmBook {
    /// Claims a warm-up for `kind`; `None` when it is already warm or warming.
    fn try_begin(&mut self, kind: EngineKind) -> Option<u64> {
        if self.warm.contains(&kind) || self.in_flight.contains(&kind) {
            return None;
        }
        self.in_flight.push(kind);
        Some(self.epoch)
    }

    /// Records the outcome of a warm-up claimed at `epoch`.
    fn finish(&mut self, kind: EngineKind, epoch: u64, warmed: bool) {
        if epoch != self.epoch {
            // Invalidated mid-flight: the engine instance may have been
            // replaced, so this result says nothing about the current one.
            return;
        }
        self.in_flight.retain(|k| *k != kind);
        if warmed && !self.warm.contains(&kind) {
            self.warm.push(kind);
        }
    }

    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.warm.clear();
        self.in_flight.clear();
    }
}

static BOOK: Mutex<PrewarmBook> = Mutex::new(PrewarmBook {
    epoch: 0,
    warm: Vec::new(),
    in_flight: Vec::new(),
});

fn book() -> std::sync::MutexGuard<'static, PrewarmBook> {
    BOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forget every warm engine. Call whenever a local engine instance is replaced
/// or its recognizer is unloaded.
pub(crate) fn invalidate() {
    book().invalidate();
}

/// Starts a background warm-up of `engine` unless `kind` is already warm or
/// warming. Never blocks the caller; failures only log, because the real
/// transcription reports its own errors.
pub(crate) fn spawn(kind: EngineKind, engine: Arc<dyn AsrEngine>) {
    let Some(epoch) = book().try_begin(kind) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let started = Instant::now();
        let request = AsrRequest::new(vec![0.0; WARM_SAMPLES], WARM_SAMPLE_RATE);
        let warmed = match engine.transcribe(request).await {
            Ok(_) => {
                tracing::info!(
                    engine = kind.as_str(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "local ASR prewarmed during recording"
                );
                true
            }
            Err(error) => {
                tracing::debug!(engine = kind.as_str(), %error, "local ASR prewarm failed");
                false
            }
        };
        book().finish(kind, epoch, warmed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warm_engine_is_not_warmed_again() {
        let mut book = PrewarmBook::default();
        let epoch = book.try_begin(EngineKind::SenseVoice).expect("first claim");
        assert_eq!(book.try_begin(EngineKind::SenseVoice), None, "in flight");
        book.finish(EngineKind::SenseVoice, epoch, true);
        assert_eq!(book.try_begin(EngineKind::SenseVoice), None, "warm");
        assert!(
            book.try_begin(EngineKind::Qwen).is_some(),
            "kinds are independent"
        );
    }

    #[test]
    fn a_failed_warm_up_can_be_retried() {
        let mut book = PrewarmBook::default();
        let epoch = book.try_begin(EngineKind::Qwen).unwrap();
        book.finish(EngineKind::Qwen, epoch, false);
        assert!(book.try_begin(EngineKind::Qwen).is_some());
    }

    #[test]
    fn invalidation_forgets_warm_engines() {
        let mut book = PrewarmBook::default();
        let epoch = book.try_begin(EngineKind::SenseVoice).unwrap();
        book.finish(EngineKind::SenseVoice, epoch, true);
        book.invalidate();
        assert!(book.try_begin(EngineKind::SenseVoice).is_some());
    }

    #[test]
    fn a_warm_up_straddling_invalidation_is_not_recorded() {
        let mut book = PrewarmBook::default();
        let stale = book.try_begin(EngineKind::Qwen).unwrap();
        book.invalidate();
        let fresh = book
            .try_begin(EngineKind::Qwen)
            .expect("new instance claims");
        book.finish(EngineKind::Qwen, stale, true);
        assert_eq!(
            book.try_begin(EngineKind::Qwen),
            None,
            "fresh claim still in flight"
        );
        book.finish(EngineKind::Qwen, fresh, true);
        assert_eq!(book.try_begin(EngineKind::Qwen), None);

        let mut book = PrewarmBook::default();
        let stale = book.try_begin(EngineKind::Qwen).unwrap();
        book.invalidate();
        book.finish(EngineKind::Qwen, stale, true);
        assert!(
            book.try_begin(EngineKind::Qwen).is_some(),
            "a stale success must not mark the replacement engine warm"
        );
    }
}
