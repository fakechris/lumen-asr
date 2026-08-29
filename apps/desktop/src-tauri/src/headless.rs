//! Headless CLI entry points, parsed before the GUI starts, so an agent or
//! script can (1) read the exact running build and (2) run the offline meeting
//! pipeline without launching the desktop app.
//!
//! `stdout` carries the machine-readable result (build line / transcript);
//! human-facing progress and errors go to `stderr`.
//!
//! Production offline path (file → speakers + timestamps [+ optional translation]):
//! ```text
//! lumen-asr-desktop meeting process <audio>
//!   --engine sensevoice|qwen|mlx-whisper|whisper
//!   --lang Spanish|es|zh|auto|…
//!   --format text|json|transcript-v1|bilingual
//!   --translate zh              # add Chinese translations per segment
//!   [--minutes]                 # optional structured minutes (needs LLM; no user library write)
//!   [--max-speakers N]
//!   [--min-turn-seconds 1.5]    # absorb short false-speaker fragments
//! ```
//! Ogg-Opus inputs are decoded natively (no ffmpeg); other non-WAV inputs
//! (m4a/mp3/…) are converted via `ffmpeg` to 16 kHz mono PCM.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lumen_asr::{
    qwen_ready, resolve_qwen_asr_dir, resolve_sensevoice_dir, sensevoice_ready, whisper_ready,
    AsrEngine, MlxWhisperAsr, MlxWhisperConfig, QwenAsr, QwenAsrConfig, SenseVoiceSherpaAsr,
    WhisperAsr, DEFAULT_MLX_WHISPER_MODEL,
};
use lumen_meeting::{
    compact_meetings, export_meeting, CompactOptions, CompactTrackStatus, DiarModels, ExportPreset,
    MeetingOptions,
};
use lumen_prompts::{
    build_system_prompt_from, Casing, CleanupLevel, IntentSpec, PromptBuildInput, PunctPolicy,
    Style,
};
use lumen_transcript::{Media, Provenance, Segment as TSegment, Speaker as TSpeaker, TranscriptV1};

/// Inspect the process arguments before the Tauri app builds. Returns
/// `Some(exit_code)` when the invocation was a headless command (the caller
/// should exit with it), or `None` to fall through to the normal desktop app.
pub fn maybe_run_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--build-info") | Some("--version") => {
            print_build_info();
            Some(0)
        }
        Some("meeting") if args.get(1).map(String::as_str) == Some("process") => {
            Some(run_meeting_process(&args[2..]))
        }
        Some("meeting") if args.get(1).map(String::as_str) == Some("compact") => {
            Some(run_meeting_compact(&args[2..]))
        }
        Some("voiceprint-match") => Some(run_voiceprint_match(&args[1..])),
        Some("--help") | Some("-h") => {
            print_help();
            Some(0)
        }
        _ => None,
    }
}

/// Print `<version> <git-sha> <build-time>` — the same identity the GUI shows in
/// Settings, so a caller can confirm exactly which build is installed.
fn print_build_info() {
    println!(
        "{} {} {}",
        env!("CARGO_PKG_VERSION"),
        env!("LUMEN_GIT_SHA"),
        env!("LUMEN_BUILD_TIME"),
    );
}

fn print_help() {
    eprintln!(
        "lumen-asr-desktop — headless commands:\n  \
         --build-info | --version\n  \
         meeting process <audio> [options]\n    \
           Offline diarize + per-turn ASR. Accepts wav/opus (native) or m4a/mp3 (ffmpeg).\n    \
           --engine sensevoice|qwen|mlx-whisper|whisper\n    \
           --lang <hint>                     e.g. Spanish, es, zh, auto\n    \
           --format text|json|transcript-v1|bilingual\n    \
           --translate zh[,en,…]             LLM translate each segment (needs corrector)\n    \
           --minutes                         write <audio>.minutes.md (needs LLM; throwaway DB)\n    \
           --json                            alias for --format json\n    \
           --max-speakers N                  diar clustering cap (default: 6)\n    \
           --min-turn-seconds SEC            absorb shorter diar fragments (default: 1.5)\n  \
         meeting compact [--dry-run] [--meeting <id>]\n    \
           Migrate stored meeting recordings from PCM WAV to Ogg-Opus in place.\n    \
           Verify-then-delete per track; safe to re-run; skips live recordings.\n  \
         voiceprint-match <meeting_id>\n\
         \nWith no headless command the desktop app launches normally.\n\
         \nEngines:\n  \
         sensevoice    sherpa-onnx (zh/en/ja/ko/yue) — dictation default\n  \
         qwen          Qwen3-ASR sherpa-onnx — multi-lingual, auto-detects language\n  \
         mlx-whisper   mlx-whisper large-v3-turbo (Metal) — production Whisper\n  \
         whisper       sherpa-onnx Whisper (CPU) — not for large multi-lingual"
    );
}

/// `voiceprint-match <meeting_id>`: for a stored meeting, score each speaker's
/// saved centroid against the enrolled identity library and print the best
/// match + whether it would auto-tag. Read-only — does not reprocess or write
/// anything — so it answers "would enrollment attribute this meeting now?"
/// without a full reprocess. Meeting/identity names are the user's own data on
/// their own machine, printed only to their terminal.
fn run_voiceprint_match(args: &[String]) -> i32 {
    let Some(meeting_id) = args.first() else {
        eprintln!("usage: voiceprint-match <meeting_id>");
        return 2;
    };
    let Ok(meeting) = meeting_id.parse::<uuid::Uuid>() else {
        eprintln!("voiceprint-match: invalid meeting id `{meeting_id}`");
        return 2;
    };
    let store = match lumen_store::Store::open(lumen_platform::default_db_path()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("voiceprint-match: open store: {error}");
            return 1;
        }
    };
    let identities =
        match lumen_identity::IdentityStore::open(lumen_identity::default_identity_dir()) {
            Ok(identities) => identities,
            Err(error) => {
                eprintln!("voiceprint-match: open identity library: {error}");
                return 1;
            }
        };
    let speakers = match store.list_speakers(meeting) {
        Ok(speakers) => speakers,
        Err(error) => {
            eprintln!("voiceprint-match: list speakers: {error}");
            return 1;
        }
    };
    if speakers.is_empty() {
        eprintln!("voiceprint-match: no speakers for meeting {meeting}");
        return 1;
    }
    println!("label  current           best-match (score)   would-auto-tag");
    for speaker in &speakers {
        let embedding = match store.get_speaker_embedding(speaker.id) {
            Ok(Some(embedding)) => embedding,
            Ok(None) => {
                println!(
                    "{:<6} {:<16} (no centroid)",
                    speaker.label,
                    speaker.display_name.as_deref().unwrap_or("-")
                );
                continue;
            }
            Err(error) => {
                eprintln!(
                    "voiceprint-match: read embedding for {}: {error}",
                    speaker.label
                );
                continue;
            }
        };
        let best = identities
            .verify_speaker(&embedding)
            .map(|report| format!("{} ({:.3})", report.display_name, report.best_score))
            .unwrap_or_else(|| "— (library empty)".to_string());
        let auto = identities
            .match_speaker(&embedding)
            .map(|(name, score)| format!("{name} ({score:.3})"))
            .unwrap_or_else(|| "—".to_string());
        println!(
            "{:<6} {:<16} {:<20} {}",
            speaker.label,
            speaker.display_name.as_deref().unwrap_or("-"),
            best,
            auto
        );
    }
    0
}

/// `meeting compact [--dry-run] [--meeting <id>]`: migrate stored meeting
/// recordings from PCM WAV to Ogg-Opus in place. Conversion is
/// verify-then-delete per track, skips live recordings and tracks already on
/// Opus, and keeps the WAV on any failure — safe to re-run until the backlog
/// is clean.
fn run_meeting_compact(args: &[String]) -> i32 {
    let mut options = CompactOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => options.dry_run = true,
            "--meeting" => {
                i += 1;
                let Some(raw) = args.get(i) else {
                    eprintln!("meeting compact: missing value for --meeting");
                    return 2;
                };
                match raw.parse::<uuid::Uuid>() {
                    Ok(id) => options.meeting = Some(id),
                    Err(_) => {
                        eprintln!("meeting compact: invalid meeting id `{raw}`");
                        return 2;
                    }
                }
            }
            flag if flag.starts_with("--meeting=") => {
                let raw = &flag["--meeting=".len()..];
                match raw.parse::<uuid::Uuid>() {
                    Ok(id) => options.meeting = Some(id),
                    Err(_) => {
                        eprintln!("meeting compact: invalid meeting id `{raw}`");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!(
                    "meeting compact: unexpected argument `{other}`\n\
                     usage: meeting compact [--dry-run] [--meeting <id>]"
                );
                return 2;
            }
        }
        i += 1;
    }

    let store = match lumen_store::Store::open(lumen_platform::default_db_path()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("meeting compact: open store: {error}");
            return 1;
        }
    };

    let print_report = |report: &lumen_meeting::CompactMeetingReport| {
        let title = report.title.as_deref().unwrap_or("(untitled)");
        if report.tracks.is_empty() {
            println!("{} {title}: no audio tracks", report.id);
            return;
        }
        for track in &report.tracks {
            let outcome = match &track.status {
                CompactTrackStatus::Converted {
                    before_bytes,
                    after_bytes,
                } => format!(
                    "{} {} → {}",
                    if options.dry_run {
                        "would convert"
                    } else {
                        "converted"
                    },
                    format_bytes(*before_bytes),
                    format_bytes(*after_bytes)
                ),
                CompactTrackStatus::Skipped(reason) => format!("skipped ({reason})"),
                CompactTrackStatus::Failed(reason) => format!("FAILED ({reason})"),
            };
            println!("{} {title} [{}]: {outcome}", report.id, track.kind.as_str());
        }
    };

    let summary = match compact_meetings(&store, &options, print_report) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("meeting compact: {error:#}");
            return 1;
        }
    };

    println!(
        "{}: converted {}, skipped {}, failed {}; {} → {} ({} {})",
        if options.dry_run { "dry-run" } else { "done" },
        summary.converted,
        summary.skipped,
        summary.failed,
        format_bytes(summary.bytes_before),
        format_bytes(summary.bytes_after),
        format_bytes(summary.projected_savings_bytes()),
        if options.dry_run {
            "projected savings"
        } else {
            "reclaimed"
        },
    );
    if summary.failed > 0 {
        1
    } else {
        0
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineChoice {
    SenseVoice,
    Qwen,
    /// mlx-whisper Metal (production Whisper).
    MlxWhisper,
    /// sherpa-onnx Whisper (CPU).
    Whisper,
}

impl EngineChoice {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sensevoice" | "sv" | "local_sensevoice" => Some(Self::SenseVoice),
            "qwen" | "qwen3" | "qwen3-asr" | "local_qwen" => Some(Self::Qwen),
            "mlx-whisper" | "mlx_whisper" | "whisper-mlx" | "whisper_mlx" => Some(Self::MlxWhisper),
            "whisper" | "local_whisper" | "sherpa-whisper" => Some(Self::Whisper),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SenseVoice => "sensevoice",
            Self::Qwen => "qwen",
            Self::MlxWhisper => "mlx-whisper",
            Self::Whisper => "whisper",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
    TranscriptV1,
    /// Human bilingual blocks (source + each --translate lang).
    Bilingual,
}

impl OutputFormat {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" | "txt" => Some(Self::Text),
            "json" | "segments" => Some(Self::Json),
            "transcript-v1" | "transcript_v1" | "v1" | "lumen-transcript" => {
                Some(Self::TranscriptV1)
            }
            "bilingual" | "bi" | "es-zh" => Some(Self::Bilingual),
            _ => None,
        }
    }
}

struct MeetingProcessCli {
    audio: PathBuf,
    engine: EngineChoice,
    lang: Option<String>,
    format: OutputFormat,
    max_speakers: usize,
    min_turn_seconds: f64,
    /// Target languages for per-segment LLM translation (e.g. ["zh"]).
    translate_langs: Vec<String>,
    mlx_whisper_model: String,
    /// Optional structured minutes next to the input file (does not write the GUI library).
    minutes: bool,
}

fn parse_meeting_process_args(args: &[String]) -> Result<MeetingProcessCli, String> {
    let mut audio: Option<PathBuf> = None;
    let mut engine = EngineChoice::SenseVoice;
    let mut lang: Option<String> = None;
    let mut format = OutputFormat::Text;
    let mut max_speakers: usize = 6;
    let mut min_turn_seconds: f64 = lumen_meeting::DEFAULT_MIN_TURN_SECONDS;
    let mut translate_langs: Vec<String> = Vec::new();
    let mut mlx_whisper_model = DEFAULT_MLX_WHISPER_MODEL.to_string();
    let mut minutes = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--json" => format = OutputFormat::Json,
            "--engine" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --engine".to_string())?;
                engine = EngineChoice::parse(v).ok_or_else(|| {
                    format!("unknown --engine `{v}` (sensevoice|qwen|mlx-whisper|whisper)")
                })?;
            }
            flag if flag.starts_with("--engine=") => {
                let v = &flag["--engine=".len()..];
                engine = EngineChoice::parse(v).ok_or_else(|| {
                    format!("unknown --engine `{v}` (sensevoice|qwen|mlx-whisper|whisper)")
                })?;
            }
            "--lang" | "--language" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --lang".to_string())?;
                lang = Some(v.clone());
            }
            flag if flag.starts_with("--lang=") => {
                lang = Some(flag["--lang=".len()..].to_string());
            }
            flag if flag.starts_with("--language=") => {
                lang = Some(flag["--language=".len()..].to_string());
            }
            "--format" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --format".to_string())?;
                format = OutputFormat::parse(v).ok_or_else(|| {
                    format!("unknown --format `{v}` (text|json|transcript-v1|bilingual)")
                })?;
            }
            flag if flag.starts_with("--format=") => {
                let v = &flag["--format=".len()..];
                format = OutputFormat::parse(v).ok_or_else(|| {
                    format!("unknown --format `{v}` (text|json|transcript-v1|bilingual)")
                })?;
            }
            "--translate" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --translate".to_string())?;
                translate_langs.extend(split_langs(v));
            }
            flag if flag.starts_with("--translate=") => {
                translate_langs.extend(split_langs(&flag["--translate=".len()..]));
            }
            "--max-speakers" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --max-speakers".to_string())?;
                max_speakers = v
                    .parse()
                    .map_err(|_| format!("invalid --max-speakers `{v}`"))?;
            }
            flag if flag.starts_with("--max-speakers=") => {
                let v = &flag["--max-speakers=".len()..];
                max_speakers = v
                    .parse()
                    .map_err(|_| format!("invalid --max-speakers `{v}`"))?;
            }
            "--min-turn-seconds" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --min-turn-seconds".to_string())?;
                min_turn_seconds = v
                    .parse()
                    .map_err(|_| format!("invalid --min-turn-seconds `{v}`"))?;
            }
            flag if flag.starts_with("--min-turn-seconds=") => {
                let v = &flag["--min-turn-seconds=".len()..];
                min_turn_seconds = v
                    .parse()
                    .map_err(|_| format!("invalid --min-turn-seconds `{v}`"))?;
            }
            "--mlx-whisper-model" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value for --mlx-whisper-model".to_string())?;
                mlx_whisper_model = v.clone();
            }
            flag if flag.starts_with("--mlx-whisper-model=") => {
                mlx_whisper_model = flag["--mlx-whisper-model=".len()..].to_string();
            }
            "--minutes" => minutes = true,
            other if other.starts_with('-') => {
                return Err(format!("unexpected argument `{other}`"));
            }
            other if audio.is_none() => audio = Some(PathBuf::from(other)),
            other => return Err(format!("unexpected positional argument `{other}`")),
        }
        i += 1;
    }
    let audio = audio.ok_or_else(|| {
        "usage: meeting process <audio> [--engine …] [--lang …] [--format …] [--translate zh]"
            .to_string()
    })?;
    // bilingual format implies Chinese translation if none specified
    if format == OutputFormat::Bilingual && translate_langs.is_empty() {
        translate_langs.push("zh".into());
    }
    Ok(MeetingProcessCli {
        audio,
        engine,
        lang,
        format,
        max_speakers,
        min_turn_seconds,
        translate_langs,
        mlx_whisper_model,
        minutes,
    })
}

fn split_langs(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Convert compressed audio to 16 kHz mono PCM WAV via ffmpeg when needed.
/// Ogg-Opus passes through untouched: the meeting pipeline decodes it natively
/// (via lumen-audio), so `meeting process take.opus` needs no ffmpeg.
/// Returns `(wav_path, temp_dir_to_cleanup)`.
fn ensure_wav(path: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
    if crate::audio_convert::audio_extension(path) == "opus" {
        if !path.is_file() {
            return Err(format!("找不到音频文件：{}", path.display()));
        }
        return Ok((path.to_path_buf(), None));
    }
    let result = crate::audio_convert::ensure_wav(path)?;
    if result.1.is_some() {
        eprintln!(
            "meeting process: converted {} → 16 kHz mono wav",
            path.display()
        );
    }
    Ok(result)
}

fn normalize_lang_hint(raw: &str, engine: EngineChoice) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("auto") {
        return None;
    }
    // sherpa-onnx engines use short codes (Qwen3-ASR auto-detects the language;
    // the hint is informational). Only the legacy full-name mapping is gone.
    match engine {
        EngineChoice::Qwen
        | EngineChoice::Whisper
        | EngineChoice::SenseVoice
        | EngineChoice::MlxWhisper => Some(match t.to_ascii_lowercase().as_str() {
            "spanish" | "español" | "espanol" | "spa" => "es".into(),
            "english" | "eng" => "en".into(),
            "chinese" | "中文" | "zh-cn" | "zh-hans" => "zh".into(),
            "japanese" | "jpn" => "ja".into(),
            "korean" | "kor" => "ko".into(),
            "yue" | "cantonese" => "yue".into(),
            _ => t.to_ascii_lowercase(),
        }),
    }
}

fn python_for_mlx() -> PathBuf {
    crate::config::AppConfig::load().asr.python_executable()
}

fn build_engine(
    choice: EngineChoice,
    lang: Option<&str>,
    mlx_whisper_model: &str,
) -> Result<Box<dyn AsrEngine>, String> {
    match choice {
        EngineChoice::SenseVoice => {
            let dir = resolve_sensevoice_dir(None)
                .ok_or_else(|| "no SenseVoice model dir resolved — install it first".to_string())?;
            if !sensevoice_ready(&dir) {
                return Err(format!(
                    "SenseVoice model not ready under {}",
                    dir.display()
                ));
            }
            let mut eng = SenseVoiceSherpaAsr::new(dir);
            if let Some(l) = lang {
                eng = eng.with_language(l);
            }
            eprintln!(
                "meeting process: engine=sensevoice dir={}",
                eng.model_dir().display()
            );
            Ok(Box::new(eng))
        }
        EngineChoice::Qwen => {
            let dir = resolve_qwen_asr_dir(None)
                .ok_or_else(|| "no Qwen3-ASR model dir resolved — install it first".to_string())?;
            if !qwen_ready(&dir) {
                return Err(format!("Qwen3-ASR model not ready under {}", dir.display()));
            }
            let language = lang.map(|s| s.to_string());
            eprintln!(
                "meeting process: engine=qwen (sherpa-onnx) dir={} lang={:?}",
                dir.display(),
                language
            );
            let cfg = QwenAsrConfig::product(dir, language, Duration::from_secs(600));
            Ok(Box::new(QwenAsr::new(cfg)))
        }
        EngineChoice::MlxWhisper => {
            let python = python_for_mlx();
            let language = lang.map(|s| s.to_string());
            eprintln!(
                "meeting process: engine=mlx-whisper (Metal) model={} python={} lang={:?}",
                mlx_whisper_model,
                python.display(),
                language
            );
            let cfg = MlxWhisperConfig::product(
                python,
                mlx_whisper_model,
                language,
                Duration::from_secs(900),
            );
            Ok(Box::new(MlxWhisperAsr::new(cfg)))
        }
        EngineChoice::Whisper => {
            let dir = lumen_asr::default_whisper_dir();
            if !whisper_ready(&dir) {
                return Err(format!(
                    "Whisper model not ready under {} (CPU sherpa; use --engine mlx-whisper for production)",
                    dir.display()
                ));
            }
            let mut eng = WhisperAsr::new(dir);
            if let Some(l) = lang {
                eng = eng.with_language(l);
            } else {
                eng = eng.with_language("en");
            }
            eprintln!(
                "meeting process: engine=whisper (sherpa CPU) dir={} — prefer --engine mlx-whisper",
                eng.model_dir().display()
            );
            Ok(Box::new(eng))
        }
    }
}

/// `meeting process <audio> [options]`: offline diarize + per-turn ASR.
fn run_meeting_process(args: &[String]) -> i32 {
    let cli = match parse_meeting_process_args(args) {
        Ok(c) => c,
        Err(error) => {
            eprintln!("meeting process: {error}");
            return 2;
        }
    };
    if !cli.audio.is_file() {
        eprintln!("meeting process: no such file: {}", cli.audio.display());
        return 2;
    }
    if let Err(error) = check_engine_lang(cli.engine, cli.lang.as_deref()) {
        eprintln!("meeting process: {error}");
        return 2;
    }

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let (wav_path, audio_tmp) = match ensure_wav(&cli.audio) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("meeting process: {error}");
            return 1;
        }
    };

    let lang_norm = cli
        .lang
        .as_deref()
        .and_then(|l| normalize_lang_hint(l, cli.engine));
    // Prefer normalized form so aliases like "Español" → "es" tag as ES, not SRC.
    let source_tag = source_lang_tag(lang_norm.as_deref().or(cli.lang.as_deref()));

    let engine = match build_engine(cli.engine, lang_norm.as_deref(), &cli.mlx_whisper_model) {
        Ok(e) => e,
        Err(error) => {
            eprintln!("meeting process: {error}");
            if let Some(tmp) = audio_tmp {
                let _ = std::fs::remove_dir_all(tmp);
            }
            return 1;
        }
    };

    let diar_models = DiarModels::under_root(lumen_asr::lumen_models_dir().join("diar"));

    let opts = MeetingOptions {
        max_speakers: Some(cli.max_speakers),
        language_hint: lang_norm.clone(),
        min_turn_seconds: Some(cli.min_turn_seconds),
        title: Some(
            cli.audio
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "offline".into()),
        ),
        ..Default::default()
    };

    let dir = std::env::temp_dir().join(format!("lumen-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(error) = std::fs::create_dir(&dir) {
        eprintln!("meeting process: create temp dir: {error}");
        if let Some(tmp) = audio_tmp {
            let _ = std::fs::remove_dir_all(tmp);
        }
        return 1;
    }

    let code = {
        let store = match lumen_store::Store::open(dir.join("headless.sqlite")) {
            Ok(s) => s,
            Err(error) => {
                eprintln!("meeting process: open store: {error}");
                let _ = std::fs::remove_dir_all(&dir);
                if let Some(tmp) = &audio_tmp {
                    let _ = std::fs::remove_dir_all(tmp);
                }
                return 1;
            }
        };

        let outcome = tauri::async_runtime::block_on(lumen_meeting::transcribe_meeting(
            &wav_path,
            &diar_models,
            engine.as_ref(),
            &store,
            &opts,
        ));
        match outcome {
            Ok(meeting_id) => {
                if cli.minutes {
                    if let Err(error) = write_cli_minutes(&store, meeting_id, &cli.audio) {
                        eprintln!("meeting process: minutes skipped: {error}");
                    }
                }
                // Optional per-segment LLM translation (e.g. --translate zh).
                // Keep all error paths as `i32` values so temp dirs still clean up.
                match if cli.translate_langs.is_empty() {
                    Ok(Vec::new())
                } else {
                    translate_segments(&store, meeting_id, &cli.translate_langs)
                } {
                    Err(error) => {
                        eprintln!("meeting process: translate: {error}");
                        1
                    }
                    Ok(translations) => match cli.format {
                        OutputFormat::Text | OutputFormat::Json => {
                            match store.list_segments(meeting_id) {
                                Ok(segments) => {
                                    if cli.format == OutputFormat::Json && !translations.is_empty()
                                    {
                                        print_segments_with_translations(
                                            &segments,
                                            &translations,
                                            true,
                                        )
                                    } else if !translations.is_empty() {
                                        print_bilingual(&segments, &translations, &source_tag)
                                    } else {
                                        print_segments(&segments, cli.format == OutputFormat::Json)
                                    }
                                }
                                Err(error) => {
                                    eprintln!("meeting process: read segments: {error}");
                                    1
                                }
                            }
                        }
                        OutputFormat::Bilingual => match store.list_segments(meeting_id) {
                            Ok(segments) => print_bilingual(&segments, &translations, &source_tag),
                            Err(error) => {
                                eprintln!("meeting process: read segments: {error}");
                                1
                            }
                        },
                        OutputFormat::TranscriptV1 => match store.get_meeting_detail(meeting_id) {
                            Ok(Some(mut detail)) => {
                                detail.meeting.language =
                                    lang_norm.clone().or_else(|| cli.lang.clone());
                                detail.meeting.audio_path = Some(cli.audio.display().to_string());
                                if translations.is_empty() {
                                    match export_meeting(&detail, ExportPreset::DataJson) {
                                        Ok(out) => {
                                            println!("{}", out.content);
                                            0
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "meeting process: export transcript-v1: {error}"
                                            );
                                            1
                                        }
                                    }
                                } else {
                                    match export_transcript_v1_with_translations(
                                        &detail,
                                        &translations,
                                        cli.engine.as_str(),
                                    ) {
                                        Ok(json) => {
                                            println!("{json}");
                                            0
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "meeting process: export bilingual transcript-v1: {error}"
                                            );
                                            1
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                eprintln!("meeting process: meeting missing after process");
                                1
                            }
                            Err(error) => {
                                eprintln!("meeting process: load detail: {error}");
                                1
                            }
                        },
                    },
                }
            }
            Err(error) => {
                eprintln!("meeting process: {error}");
                1
            }
        }
    };

    let _ = std::fs::remove_dir_all(&dir);
    if let Some(tmp) = audio_tmp {
        let _ = std::fs::remove_dir_all(tmp);
    }
    if code == 0 {
        eprintln!(
            "meeting process: done engine={} format={:?}",
            cli.engine.as_str(),
            cli.format
        );
    }
    code
}

/// Print the transcript and return an exit code (`1` if a `--json` transcript
/// could not be serialized, so the caller never reports success with no output).
fn print_segments(segments: &[lumen_core::TranscriptSegment], json: bool) -> i32 {
    if json {
        return match serde_json::to_string_pretty(segments) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(error) => {
                eprintln!("meeting process: serialize json: {error}");
                1
            }
        };
    }
    let labels = speaker_labels(segments);
    for seg in segments {
        let who = seg
            .speaker_id
            .and_then(|id| labels.get(&id).cloned())
            .unwrap_or_else(|| "?".into());
        println!(
            "[{:>8.1}-{:<8.1}] {}: {}",
            seg.start_seconds, seg.end_seconds, who, seg.text
        );
    }
    eprintln!(
        "({} segments, {} speaker{})",
        segments.len(),
        labels.len(),
        if labels.len() == 1 { "" } else { "s" }
    );
    0
}

fn minutes_output_path(audio: &Path) -> PathBuf {
    let stem = audio
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "meeting".into());
    let name = format!("{stem}.minutes.md");
    match audio.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Structured minutes for `--minutes`. Uses the user's corrector config, writes
/// next to the input file, and never touches the GUI meeting library.
fn write_cli_minutes(
    store: &lumen_store::Store,
    meeting_id: uuid::Uuid,
    audio: &Path,
) -> Result<(), String> {
    let cfg = crate::config::AppConfig::load();
    if !cfg.corrector.enabled || cfg.corrector.provider == "none" {
        return Err("no LLM configured (Settings → AI cleanup)".into());
    }
    let corrector = crate::corrector_svc::build_corrector(&cfg.corrector)?;
    let segments = store.list_segments(meeting_id).map_err(|e| e.to_string())?;
    let speakers = store.list_speakers(meeting_id).map_err(|e| e.to_string())?;
    let transcript = lumen_meeting::minutes::render_transcript_for_minutes(&segments, &speakers);
    let minutes = tauri::async_runtime::block_on(lumen_meeting::minutes::generate_minutes(
        corrector.as_ref(),
        &transcript,
        None,
        None,
    ))
    .map_err(|e| e.to_string())?;
    let model = cfg.corrector.model.trim();
    let rows = lumen_meeting::minutes::minutes_summaries(
        meeting_id,
        &minutes,
        (!model.is_empty()).then_some(model),
    )
    .map_err(|e| e.to_string())?;
    for row in rows {
        store.save_summary(&row).map_err(|e| e.to_string())?;
    }
    let detail = store
        .get_meeting_detail(meeting_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "meeting missing after minutes".to_string())?;
    let out = export_meeting(&detail, ExportPreset::MinutesMd).map_err(|e| e.to_string())?;
    let dest = minutes_output_path(audio);
    std::fs::write(&dest, &out.content).map_err(|e| format!("write {}: {e}", dest.display()))?;
    eprintln!("meeting process: wrote {}", dest.display());
    Ok(())
}

fn speaker_labels(
    segments: &[lumen_core::TranscriptSegment],
) -> std::collections::HashMap<uuid::Uuid, String> {
    use std::collections::HashMap;
    let mut labels: HashMap<uuid::Uuid, String> = HashMap::new();
    for seg in segments {
        if let Some(id) = seg.speaker_id {
            let next = labels.len() + 1;
            labels.entry(id).or_insert_with(|| format!("S{next}"));
        }
    }
    labels
}

/// Per-segment translations: index aligned with `list_segments` order.
/// Each entry is a map lang → text (e.g. "zh" → "……").
type SegmentTranslations = Vec<std::collections::BTreeMap<String, String>>;

fn translate_segments(
    store: &lumen_store::Store,
    meeting_id: uuid::Uuid,
    langs: &[String],
) -> Result<SegmentTranslations, String> {
    let segments = store.list_segments(meeting_id).map_err(|e| e.to_string())?;
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let cfg = crate::config::AppConfig::load();
    let corrector = crate::corrector_svc::build_corrector(&cfg.corrector)
        .map_err(|e| format!("corrector: {e}"))?;
    if !cfg.corrector.enabled || cfg.corrector.provider == "none" {
        return Err(
            "translation requires a configured LLM corrector (Settings → AI cleanup / config.toml [corrector])"
                .into(),
        );
    }

    // Per-segment translation (serial). Batching is optional later if volume hurts.
    let mut out: SegmentTranslations = Vec::with_capacity(segments.len());
    for (i, seg) in segments.iter().enumerate() {
        let mut map = std::collections::BTreeMap::new();
        let src = seg.text.trim();
        if src.is_empty() {
            out.push(map);
            continue;
        }
        for lang in langs {
            let target = display_lang_name(lang);
            let prompt_input = PromptBuildInput {
                cleanup: CleanupLevel::Light,
                style: Style::Neutral,
                casing: Casing::Sentence,
                punctuation: PunctPolicy::default(),
                polish: vec![],
                custom: None,
                intent: IntentSpec::Translate {
                    target_language: target.clone(),
                    style: Some("faithful".into()),
                },
            };
            let system = build_system_prompt_from(&prompt_input);
            let temperature = CleanupLevel::Light.temperature();
            eprintln!(
                "meeting process: translate segment {}/{} → {lang}…",
                i + 1,
                segments.len()
            );
            let result = tauri::async_runtime::block_on(lumen_corrector::correct_or_fallback_with(
                corrector.as_ref(),
                src,
                lumen_corrector::DictionaryContext::default(),
                system,
                temperature,
            ));
            if result.model_applied {
                map.insert(lang.clone(), result.text.trim().to_string());
            } else {
                // Do not write source text into translations.{lang}: Cut and
                // other consumers of lumen-transcript.v1 cannot tell fallback
                // pollution from a real translation.
                eprintln!(
                    "meeting process: translate segment {} → {lang} skipped (fallback {:?})",
                    i + 1,
                    result.fallback_reason
                );
            }
        }
        out.push(map);
    }
    Ok(out)
}

fn display_lang_name(code: &str) -> String {
    match code.to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "zh-hans" | "cn" | "chinese" => "Chinese".into(),
        "en" | "eng" | "english" => "English".into(),
        "es" | "spa" | "spanish" => "Spanish".into(),
        "ja" | "jpn" | "japanese" => "Japanese".into(),
        other => other.to_string(),
    }
}

/// Compact uppercase tag for the bilingual source line (from `--lang`).
///
/// Unknown / missing / auto → `SRC` so we never hardcode a dogfood language.
fn source_lang_tag(lang: Option<&str>) -> String {
    let Some(raw) = lang.map(str::trim).filter(|s| !s.is_empty()) else {
        return "SRC".into();
    };
    let lower = raw.to_ascii_lowercase();
    if lower == "auto" {
        return "SRC".into();
    }
    let primary = lower
        .split(['-', '_', ' '])
        .next()
        .unwrap_or(lower.as_str());
    match primary {
        "es" | "spa" | "spanish" | "español" | "espanol" => "ES".into(),
        "zh" | "cn" | "chinese" | "中文" => "ZH".into(),
        "en" | "eng" | "english" => "EN".into(),
        "ja" | "jpn" | "japanese" => "JA".into(),
        "ko" | "kor" | "korean" => "KO".into(),
        "yue" | "cantonese" => "YUE".into(),
        "fr" | "fra" | "french" => "FR".into(),
        "de" | "deu" | "german" => "DE".into(),
        "pt" | "por" | "portuguese" => "PT".into(),
        other if other.len() <= 8 && other.chars().all(|c| c.is_ascii_alphanumeric()) => {
            other.to_ascii_uppercase()
        }
        _ => "SRC".into(),
    }
}

/// SenseVoice official languages; anything else with that engine is garbage.
fn sensevoice_supports_lang(lang: &str) -> bool {
    let lower = lang.trim().to_ascii_lowercase();
    if lower.is_empty() || lower == "auto" {
        return true;
    }
    let primary = lower
        .split(['-', '_', ' '])
        .next()
        .unwrap_or(lower.as_str());
    matches!(
        primary,
        "zh" | "cn"
            | "chinese"
            | "中文"
            | "en"
            | "eng"
            | "english"
            | "ja"
            | "jpn"
            | "japanese"
            | "ko"
            | "kor"
            | "korean"
            | "yue"
            | "cantonese"
    )
}

/// Reject engine/language pairs that will silently produce garbage text.
fn check_engine_lang(engine: EngineChoice, lang: Option<&str>) -> Result<(), String> {
    let Some(raw) = lang.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    match engine {
        EngineChoice::SenseVoice if !sensevoice_supports_lang(raw) => Err(format!(
            "engine `sensevoice` does not support language `{raw}` \
             (official: zh/en/ja/ko/yue). Use `--engine mlx-whisper` or `--engine qwen` \
             for multi-lingual file ASR, or omit `--lang` for CJK dictation defaults"
        )),
        _ => Ok(()),
    }
}

fn print_bilingual(
    segments: &[lumen_core::TranscriptSegment],
    translations: &SegmentTranslations,
    source_tag: &str,
) -> i32 {
    let labels = speaker_labels(segments);
    for (i, seg) in segments.iter().enumerate() {
        let who = seg
            .speaker_id
            .and_then(|id| labels.get(&id).cloned())
            .unwrap_or_else(|| "?".into());
        println!(
            "[{:>8.1}-{:<8.1}] {}:",
            seg.start_seconds, seg.end_seconds, who
        );
        println!("  {source_tag}: {}", seg.text);
        if let Some(map) = translations.get(i) {
            for (lang, text) in map {
                let tag = lang.to_ascii_uppercase();
                println!("  {tag}: {text}");
            }
        }
        println!();
    }
    eprintln!(
        "({} segments, bilingual source={source_tag} langs={:?})",
        segments.len(),
        translations
            .first()
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    );
    0
}

fn print_segments_with_translations(
    segments: &[lumen_core::TranscriptSegment],
    translations: &SegmentTranslations,
    pretty: bool,
) -> i32 {
    #[derive(serde::Serialize)]
    struct Row<'a> {
        start_seconds: f64,
        end_seconds: f64,
        speaker_id: Option<uuid::Uuid>,
        text: &'a str,
        translations: std::collections::BTreeMap<String, String>,
    }
    let rows: Vec<Row> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| Row {
            start_seconds: s.start_seconds,
            end_seconds: s.end_seconds,
            speaker_id: s.speaker_id,
            text: &s.text,
            translations: translations.get(i).cloned().unwrap_or_default(),
        })
        .collect();
    let text = if pretty {
        serde_json::to_string_pretty(&rows)
    } else {
        serde_json::to_string(&rows)
    };
    match text {
        Ok(t) => {
            println!("{t}");
            0
        }
        Err(e) => {
            eprintln!("meeting process: serialize: {e}");
            1
        }
    }
}

fn export_transcript_v1_with_translations(
    detail: &lumen_core::MeetingDetail,
    translations: &SegmentTranslations,
    engine: &str,
) -> Result<String, String> {
    use std::collections::HashMap;
    let label_of: HashMap<uuid::Uuid, &str> = detail
        .speakers
        .iter()
        .map(|s| (s.id, s.label.as_str()))
        .collect();

    let t_segments: Vec<TSegment> = detail
        .segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let mut ts = TSegment::new(seg.start_seconds, seg.end_seconds, seg.text.clone())
                .with_id(seg.seq.to_string());
            if let Some(label) = seg.speaker_id.and_then(|id| label_of.get(&id)) {
                ts = ts.with_speaker((*label).to_string());
            }
            if let Some(map) = translations.get(i) {
                for (lang, text) in map {
                    ts = ts.with_translation(lang.clone(), text.clone());
                }
            }
            ts
        })
        .collect();

    let t_speakers: Vec<TSpeaker> = detail
        .speakers
        .iter()
        .map(|s| {
            let mut ts = TSpeaker::new(s.label.clone());
            if let Some(name) = &s.display_name {
                ts = ts.with_display_name(name.clone());
            }
            ts
        })
        .collect();

    let media = Media {
        path: detail.meeting.audio_path.clone(),
        duration_seconds: detail.meeting.duration_seconds,
        ..Media::default()
    };
    let mut provenance = Provenance::new("lumen-meeting");
    provenance.engine = Some(format!("diar-rs+{engine}"));
    provenance.language = detail.meeting.language.clone();
    provenance.created_at = Some(detail.meeting.created_at.to_rfc3339());

    let doc = TranscriptV1::new(t_segments)
        .with_provenance(provenance)
        .with_media(media)
        .with_speakers(t_speakers);
    doc.to_json_string_pretty().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn source_lang_tag_from_cli_not_hardcoded_es() {
        assert_eq!(source_lang_tag(Some("zh")), "ZH");
        assert_eq!(source_lang_tag(Some("Chinese")), "ZH");
        assert_eq!(source_lang_tag(Some("es")), "ES");
        assert_eq!(source_lang_tag(Some("Spanish")), "ES");
        assert_eq!(source_lang_tag(Some("Español")), "ES");
        assert_eq!(source_lang_tag(Some("en")), "EN");
        assert_eq!(source_lang_tag(None), "SRC");
        assert_eq!(source_lang_tag(Some("auto")), "SRC");
    }

    #[test]
    fn source_tag_prefers_normalized_alias() {
        // Mirrors run_meeting_process: tag from lang_norm, not raw CLI.
        let raw = "Español";
        let norm = normalize_lang_hint(raw, EngineChoice::MlxWhisper);
        assert_eq!(norm.as_deref(), Some("es"));
        assert_eq!(source_lang_tag(norm.as_deref().or(Some(raw))), "ES");
    }

    #[test]
    fn sensevoice_rejects_spanish() {
        let err = check_engine_lang(EngineChoice::SenseVoice, Some("es")).unwrap_err();
        assert!(err.contains("sensevoice"), "{err}");
        assert!(err.contains("mlx-whisper") || err.contains("qwen"), "{err}");
    }

    #[test]
    fn sensevoice_accepts_official_langs() {
        for lang in ["zh", "en", "ja", "ko", "yue", "auto", "Chinese"] {
            check_engine_lang(EngineChoice::SenseVoice, Some(lang)).unwrap();
        }
        check_engine_lang(EngineChoice::SenseVoice, None).unwrap();
    }

    #[test]
    fn multilingual_engines_accept_spanish() {
        check_engine_lang(EngineChoice::MlxWhisper, Some("es")).unwrap();
        check_engine_lang(EngineChoice::Qwen, Some("Spanish")).unwrap();
    }

    #[test]
    fn parse_bilingual_defaults_translate_zh() {
        let cli = parse_meeting_process_args(&args(&[
            "talk.m4a",
            "--format",
            "bilingual",
            "--engine",
            "mlx-whisper",
            "--lang",
            "es",
        ]))
        .unwrap();
        assert_eq!(cli.format, OutputFormat::Bilingual);
        assert_eq!(cli.translate_langs, vec!["zh".to_string()]);
        assert_eq!(cli.engine, EngineChoice::MlxWhisper);
        assert_eq!(cli.lang.as_deref(), Some("es"));
    }

    #[test]
    fn parse_engine_and_min_turn() {
        let cli = parse_meeting_process_args(&args(&[
            "a.wav",
            "--engine=qwen",
            "--min-turn-seconds",
            "0",
            "--lang",
            "Spanish",
        ]))
        .unwrap();
        assert_eq!(cli.engine, EngineChoice::Qwen);
        assert!((cli.min_turn_seconds - 0.0).abs() < 1e-9);
        assert_eq!(cli.lang.as_deref(), Some("Spanish"));
        assert!(!cli.minutes);
    }

    #[test]
    fn parse_minutes_flag() {
        let cli = parse_meeting_process_args(&args(&["talk.wav", "--minutes"])).unwrap();
        assert!(cli.minutes);
    }
}
