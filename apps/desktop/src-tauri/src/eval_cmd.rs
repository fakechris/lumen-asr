//! `eval` — the audio side of the `lumen-eval` quality harness (INV-1805).
//!
//! Subcommands:
//! - `eval synth` — synthesize the corpus clips locally with macOS `say`
//!   (license-safe: nothing but text ships in the repo, audio is generated
//!   on your machine);
//! - `eval asr` — transcribe the clips with a local engine, print a JSON
//!   report, and gate it against `crates/lumen-eval/data/baselines.json`
//!   (or `--baseline <file>`); exit code 1 on a gated breach;
//! - `eval selftest` — validate the corpus/baselines and prove the gate
//!   catches an injected cleanup regression and a diarization speaker
//!   collapse (no audio, no models — what CI runs).
//!
//! Typical local loop:
//! ```text
//! lumen-asr-desktop eval synth --out-dir eval-clips
//! lumen-asr-desktop eval asr --engine qwen --clips-dir eval-clips \
//!   --record-baseline baselines.local.json   # first run
//! lumen-asr-desktop eval asr --engine qwen --clips-dir eval-clips  # gate
//! ```

use std::path::{Path, PathBuf};

use lumen_eval::corpus::EvalSet;
use lumen_eval::report::{compare_with_baselines, Baselines, EvalReport, Hypothesis};
use lumen_eval::{Corpus, BASELINES_JSON};

use crate::headless::{build_engine, EngineChoice};

const USAGE: &str = "eval <synth|asr|selftest> [options]
  synth  --out-dir <dir> [--voice-zh <name>] [--voice-en <name>]
  asr    --engine <sensevoice|qwen|whisper|mlx-whisper> [--clips-dir <dir>]
         [--set <name>]... [--lang <hint>] [--baseline <file>]
         [--out <report.json>] [--record-baseline <file>]
  selftest";

pub fn run_eval(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("synth") => run_synth(&args[1..]),
        Some("asr") => run_asr(&args[1..]),
        Some("selftest") => run_selftest(),
        _ => {
            eprintln!("eval: expected a subcommand\n{USAGE}");
            2
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == flag {
            return iter.next().cloned();
        }
    }
    None
}

fn flag_set_values(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == flag {
            if let Some(v) = iter.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

fn run_selftest() -> i32 {
    match lumen_eval::selftest::run() {
        Ok(performed) => {
            for line in performed {
                println!("eval selftest: {line}");
            }
            0
        }
        Err(error) => {
            eprintln!("eval selftest: FAILED — {error}");
            1
        }
    }
}

// --- synth -----------------------------------------------------------------

fn run_synth(args: &[String]) -> i32 {
    if !cfg!(target_os = "macos") {
        eprintln!("eval synth: needs macOS `say`; synthesize clips on a Mac");
        return 2;
    }
    let out_dir =
        PathBuf::from(flag_value(args, "--out-dir").unwrap_or_else(|| "eval-clips".into()));
    let voice_zh = flag_value(args, "--voice-zh").unwrap_or_else(|| "Tingting".into());
    let voice_en = flag_value(args, "--voice-en").unwrap_or_else(|| "Samantha".into());
    let corpus = match Corpus::embedded() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("eval synth: {e}");
            return 2;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("eval synth: cannot create {}: {e}", out_dir.display());
        return 2;
    }

    let mut ok = 0;
    for clip in corpus.base_clips() {
        let (text, _) = match corpus.resolved_text(clip) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("eval synth: {e}");
                return 2;
            }
        };
        let voice = if clip.lang == "en" {
            &voice_en
        } else {
            &voice_zh
        };
        let path = out_dir.join(format!("{}.wav", clip.id));
        let status = std::process::Command::new("say")
            .arg("-v")
            .arg(voice)
            .arg("-o")
            .arg(&path)
            .arg("--data-format=LEI16@16000")
            .arg(&text)
            .status();
        match status {
            Ok(s) if s.success() => {
                ok += 1;
                println!("eval synth: {} (voice {voice})", path.display());
            }
            _ => {
                eprintln!(
                    "eval synth: `say` failed for {} with voice {voice} — list voices with `say -v ?`",
                    clip.id
                );
                return 1;
            }
        }
    }

    // Long clips: concatenate the source wavs with the configured gap of
    // silence, matching how the corpus expects them joined.
    for clip in corpus
        .clips
        .iter()
        .filter(|c| !c.source_clip_ids.is_empty())
    {
        let gap = clip.gap_seconds.unwrap_or(0.8);
        let mut samples: Vec<f32> = Vec::new();
        let silence = (gap * 16_000.0).round() as usize;
        for (index, source_id) in clip.source_clip_ids.iter().enumerate() {
            if index > 0 {
                samples.extend(std::iter::repeat_n(0.0, silence));
            }
            let path = out_dir.join(format!("{source_id}.wav"));
            match read_wav_mono_16k(&path) {
                Ok(s) => samples.extend(s),
                Err(e) => {
                    eprintln!("eval synth: {e}");
                    return 1;
                }
            }
        }
        let path = out_dir.join(format!("{}.wav", clip.id));
        let bytes = lumen_asr::pcm_to_wav_bytes(&samples, 16_000);
        if let Err(e) = std::fs::write(&path, bytes) {
            eprintln!("eval synth: cannot write {}: {e}", path.display());
            return 1;
        }
        println!(
            "eval synth: {} ({} sources, {gap}s gaps)",
            path.display(),
            clip.source_clip_ids.len()
        );
        ok += 1;
    }

    println!(
        "eval synth: {ok} clips under {} — run `eval asr` next",
        out_dir.display()
    );
    0
}

/// Read a 16-bit PCM mono WAV at any sample rate, resampled to 16 kHz.
/// Only the format `eval synth` writes; anything else is an error.
fn read_wav_mono_16k(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{}: not a RIFF/WAVE file", path.display()));
    }
    let mut format: Option<(u16, u16, u32)> = None; // (channels, bits, sample_rate)
    let mut data: Option<(usize, usize)> = None; // (offset, len)
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if size >= 16 => {
                let audio_format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
                let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                let rate = u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]);
                let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
                if audio_format != 1 {
                    return Err(format!(
                        "{}: not PCM (format {audio_format})",
                        path.display()
                    ));
                }
                format = Some((channels, bits, rate));
            }
            b"data" => data = Some((body, size.min(bytes.len() - body))),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let (channels, bits, rate) =
        format.ok_or_else(|| format!("{}: missing fmt chunk", path.display()))?;
    let (offset, len) = data.ok_or_else(|| format!("{}: missing data chunk", path.display()))?;
    if bits != 16 || channels != 1 {
        return Err(format!(
            "{}: expected 16-bit mono, found {bits}-bit {channels}ch",
            path.display()
        ));
    }
    let mut samples = Vec::with_capacity(len / 2);
    for chunk in bytes[offset..offset + len].chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(v as f32 / 32_768.0);
    }
    if rate == 16_000 {
        Ok(samples)
    } else {
        Ok(lumen_asr::resample_linear(&samples, rate, 16_000))
    }
}

// --- asr -------------------------------------------------------------------

fn run_asr(args: &[String]) -> i32 {
    let engine_flag = match flag_value(args, "--engine") {
        Some(v) => v,
        None => {
            eprintln!("eval asr: --engine is required\n{USAGE}");
            return 2;
        }
    };
    let choice = match EngineChoice::parse(&engine_flag) {
        Some(c) => c,
        None => {
            eprintln!("eval asr: unknown engine `{engine_flag}`\n{USAGE}");
            return 2;
        }
    };
    let clips_dir =
        PathBuf::from(flag_value(args, "--clips-dir").unwrap_or_else(|| "eval-clips".into()));
    let set_filters = flag_set_values(args, "--set");
    let mut sets: Option<Vec<EvalSet>> = None;
    for value in &set_filters {
        match EvalSet::parse(value) {
            Some(s) => {
                sets.get_or_insert_with(Vec::new).push(s);
            }
            None => {
                eprintln!("eval asr: unknown set `{value}` (zh-filler|zh-plain|en|mix|dict|long)");
                return 2;
            }
        }
    }
    let lang = flag_value(args, "--lang");
    let record_baseline = flag_value(args, "--record-baseline");

    let corpus = match Corpus::embedded() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("eval asr: {e}");
            return 2;
        }
    };

    // Every filtered clip needs its wav present.
    let selected: Vec<_> = corpus
        .clips
        .iter()
        .filter(|c| sets.as_ref().map(|s| s.contains(&c.set)).unwrap_or(true))
        .collect();
    if selected.is_empty() {
        eprintln!("eval asr: no clips selected");
        return 2;
    }
    let missing: Vec<String> = selected
        .iter()
        .map(|c| clips_dir.join(format!("{}.wav", c.id)))
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "eval asr: {} clips missing under {} — run `eval synth` first: {}",
            missing.len(),
            clips_dir.display(),
            missing.join(", ")
        );
        return 2;
    }

    // No forced language: engines auto-detect unless --lang is given
    // (SenseVoice auto mode covers the mixed-script corpus).
    let engine = match build_engine(
        choice,
        lang.as_deref(),
        "mlx-community/whisper-large-v3-turbo",
    ) {
        Ok(e) => e,
        Err(error) => {
            eprintln!("eval asr: {error}");
            return 2;
        }
    };

    // Report only on the selected clips: a `--set` filter scopes the run.
    let mut corpus = corpus;
    corpus
        .clips
        .retain(|c| sets.as_ref().map(|s| s.contains(&c.set)).unwrap_or(true));

    let engine_errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine_label = choice.as_str().to_string();
    let report = EvalReport::run(&engine_label, &corpus, |clip, _text| {
        let path = clips_dir.join(format!("{}.wav", clip.id));
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("eval asr: {}: {e}", path.display());
                engine_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Hypothesis::text(String::new());
            }
        };
        let locale = match clip.lang.as_str() {
            "en" => "en-US",
            _ => "zh-CN",
        };
        match tauri::async_runtime::block_on(engine.transcribe_wav(&bytes, locale)) {
            Ok(result) => Hypothesis::text(result.text),
            Err(error) => {
                eprintln!("eval asr: {}: engine error: {error}", clip.id);
                engine_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Hypothesis::text(String::new())
            }
        }
    });
    let mut report = match report {
        Ok(r) => r,
        Err(e) => {
            eprintln!("eval asr: {e}");
            return 2;
        }
    };

    if let Some(record_path) = record_baseline {
        let recorded = Baselines::from_report(&report, true);
        let payload = serde_json::to_string_pretty(&recorded).unwrap_or_default();
        if let Err(e) = std::fs::write(&record_path, payload) {
            eprintln!("eval asr: cannot write {record_path}: {e}");
            return 2;
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        println!("eval asr: baselines recorded to {record_path} — review, then paste into crates/lumen-eval/data/baselines.json");
        return 0;
    }

    let baselines = match flag_value(args, "--baseline") {
        Some(path) => match Baselines::from_file(Path::new(&path)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("eval asr: {e}");
                return 2;
            }
        },
        None => match serde_json::from_str::<Baselines>(BASELINES_JSON) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("eval asr: embedded baselines invalid: {e}");
                return 2;
            }
        },
    };
    let mut verdict = compare_with_baselines(&report, &baselines);
    // Transcription failures fail the run even when the numbers pass.
    let errors = engine_errors.load(std::sync::atomic::Ordering::Relaxed);
    if errors > 0 {
        verdict.pass = false;
        verdict
            .breaches
            .push(format!("{errors} clip transcription error(s)"));
    }
    // --out reports carry the evaluated mode, written after the verdict.
    report.mode = verdict.mode;
    if let Some(out) = flag_value(args, "--out") {
        let payload = serde_json::to_string_pretty(&report).unwrap_or_default();
        if let Err(e) = std::fs::write(&out, payload) {
            eprintln!("eval asr: cannot write {out}: {e}");
            return 2;
        }
    }
    let payload = serde_json::json!({
        "report": report,
        "verdict": verdict,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );
    if verdict.pass {
        eprintln!("eval asr: gate passed ({})", verdict.mode.label());
        0
    } else {
        eprintln!("eval asr: gate FAILED");
        for breach in &verdict.breaches {
            eprintln!("eval asr: breach: {breach}");
        }
        1
    }
}
