use lumen_asr::{AsrEngine, AsrRequest, QwenAsr, QwenAsrConfig};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("lumen-qwen-{name}-{nonce}"))
}

#[tokio::test]
async fn qwen_worker_is_reused_between_transcriptions() {
    let root = temp_dir("reuse");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("fake_worker.py");
    let starts = root.join("starts.txt");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import pathlib
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--language")
parser.add_argument("--startup-marker", required=True)
args = parser.parse_args()
with pathlib.Path(args.startup_marker).open("a", encoding="utf-8") as marker:
    marker.write("started\n")
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({
        "id": request["id"],
        "text": "Qwen result",
        "language": args.language or "zh",
    }), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();

    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: PathBuf::from("/usr/bin/python3"),
        worker_script: worker,
        model_dir: model,
        language: Some("zh".into()),
        timeout: std::time::Duration::from_secs(5),
        extra_args: vec!["--startup-marker".into(), starts.display().to_string()],
    });
    let request = || AsrRequest {
        samples: vec![0.0; 1_600],
        sample_rate: 16_000,
        hotwords: vec![],
    };

    let first = engine.transcribe(request()).await.unwrap();
    let second = engine.transcribe(request()).await.unwrap();

    assert_eq!(first.text, "Qwen result");
    assert_eq!(second.text, "Qwen result");
    assert_eq!(first.engine.as_str(), "qwen3_asr");
    assert_eq!(std::fs::read_to_string(&starts).unwrap(), "started\n");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_rejects_empty_audio_without_starting_worker() {
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: PathBuf::from("/does/not/exist"),
        worker_script: PathBuf::from("/does/not/exist"),
        model_dir: PathBuf::from("/does/not/exist"),
        language: None,
        timeout: std::time::Duration::from_secs(1),
        extra_args: vec![],
    });

    let error = engine
        .transcribe(AsrRequest {
            samples: vec![],
            sample_rate: 16_000,
            hotwords: vec![],
        })
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "empty audio");
}

#[tokio::test]
async fn qwen_restarts_worker_after_protocol_corruption() {
    let root = temp_dir("restart");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("flaky_worker.py");
    let starts = root.join("starts.txt");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import pathlib
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--startup-marker", required=True)
args = parser.parse_args()
marker = pathlib.Path(args.startup_marker)
start_count = len(marker.read_text(encoding="utf-8").splitlines()) if marker.exists() else 0
with marker.open("a", encoding="utf-8") as output:
    output.write("started\n")
for line in sys.stdin:
    request = json.loads(line)
    if start_count == 0:
        print("not-json", flush=True)
    else:
        print(json.dumps({"id": request["id"], "text": "recovered"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: PathBuf::from("/usr/bin/python3"),
        worker_script: worker,
        model_dir: model,
        language: None,
        timeout: std::time::Duration::from_secs(5),
        extra_args: vec!["--startup-marker".into(), starts.display().to_string()],
    });
    let request = || AsrRequest {
        samples: vec![0.0; 1_600],
        sample_rate: 16_000,
        hotwords: vec![],
    };

    assert!(engine.transcribe(request()).await.is_err());
    let recovered = engine.transcribe(request()).await.unwrap();

    assert_eq!(recovered.text, "recovered");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_unloads_model_when_engine_is_deselected() {
    let root = temp_dir("unload");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("fake_worker.py");
    let starts = root.join("starts.txt");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import pathlib
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--startup-marker", required=True)
args = parser.parse_args()
with pathlib.Path(args.startup_marker).open("a", encoding="utf-8") as output:
    output.write("started\n")
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id": request["id"], "text": "ok"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: PathBuf::from("/usr/bin/python3"),
        worker_script: worker,
        model_dir: model,
        language: None,
        timeout: std::time::Duration::from_secs(5),
        extra_args: vec!["--startup-marker".into(), starts.display().to_string()],
    });
    let request = || AsrRequest {
        samples: vec![0.0; 1_600],
        sample_rate: 16_000,
        hotwords: vec![],
    };

    engine.transcribe(request()).await.unwrap();
    assert!(engine.unload());
    engine.transcribe(request()).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}
