//! Headless CLI entry points, parsed before the GUI starts, so an agent or
//! script can (1) read the exact running build and (2) run the offline meeting
//! pipeline without launching the desktop app.
//!
//! `stdout` carries the machine-readable result (build line / transcript);
//! human-facing progress and errors go to `stderr`.

use std::path::Path;

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
         --build-info | --version        print `<version> <git-sha> <build-time>` and exit\n  \
         meeting process <wav> [--json]  diarize + transcribe a mic WAV offline and print the transcript\n\
         \nWith no headless command the desktop app launches normally."
    );
}

/// `meeting process <wav> [--json]`: run the offline diarize + transcribe
/// pipeline on a single mic WAV and print the transcript (one line per segment,
/// or a JSON array with `--json`). Exercises the exact production ASR path, so
/// it doubles as a way to reprocess a recording and observe resource use.
fn run_meeting_process(args: &[String]) -> i32 {
    let mut wav: Option<String> = None;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other if !other.starts_with('-') && wav.is_none() => wav = Some(other.to_string()),
            other => {
                eprintln!("meeting process: unexpected argument `{other}`");
                return 2;
            }
        }
    }
    let Some(wav) = wav else {
        eprintln!("usage: meeting process <wav> [--json]");
        return 2;
    };
    let wav_path = Path::new(&wav);
    if !wav_path.is_file() {
        eprintln!("meeting process: no such file: {wav}");
        return 2;
    }

    // Pipeline stage logs (diarize/transcribe timings) go to stderr so stdout
    // stays clean for the transcript. Best-effort: a missing subscriber just
    // means no logs.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let Some(sv_dir) = lumen_asr::resolve_sensevoice_dir(None) else {
        eprintln!("meeting process: no SenseVoice model dir resolved — install it first");
        return 1;
    };
    if !lumen_asr::sensevoice_ready(&sv_dir) {
        eprintln!(
            "meeting process: SenseVoice model not found under {} — install it first",
            sv_dir.display()
        );
        return 1;
    }
    let engine = lumen_asr::SenseVoiceSherpaAsr::new(sv_dir);
    let diar_models =
        lumen_meeting::DiarModels::under_root(lumen_asr::lumen_models_dir().join("diar"));

    // A throwaway store: this command transcribes and prints, it does not touch
    // the app's real meeting library.
    let store_path =
        std::env::temp_dir().join(format!("lumen-headless-{}.sqlite", std::process::id()));
    let store = match lumen_store::Store::open(&store_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("meeting process: open store: {error}");
            return 1;
        }
    };

    let opts = lumen_meeting::MeetingOptions {
        max_speakers: Some(6),
        ..Default::default()
    };

    let outcome = tauri::async_runtime::block_on(lumen_meeting::transcribe_meeting(
        wav_path,
        &diar_models,
        &engine,
        &store,
        &opts,
    ));
    let code = match outcome {
        Ok(meeting_id) => match store.list_segments(meeting_id) {
            Ok(segments) => {
                print_segments(&segments, json);
                0
            }
            Err(error) => {
                eprintln!("meeting process: read segments: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("meeting process: {error}");
            1
        }
    };

    let _ = std::fs::remove_file(&store_path);
    code
}

fn print_segments(segments: &[lumen_core::TranscriptSegment], json: bool) {
    if json {
        match serde_json::to_string_pretty(segments) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("meeting process: serialize json: {error}"),
        }
        return;
    }
    // Label speakers S1, S2, … in first-seen order for a readable transcript.
    use std::collections::HashMap;
    let mut labels: HashMap<uuid::Uuid, String> = HashMap::new();
    for seg in segments {
        let who = match seg.speaker_id {
            Some(id) => {
                let next = labels.len() + 1;
                labels
                    .entry(id)
                    .or_insert_with(|| format!("S{next}"))
                    .clone()
            }
            None => "?".to_string(),
        };
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
}
