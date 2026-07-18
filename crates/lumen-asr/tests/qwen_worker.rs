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

fn python_executable() -> PathBuf {
    std::env::var_os("LUMEN_QWEN_TEST_PYTHON")
        .or_else(|| std::env::var_os("PYTHON"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }))
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
        python_executable: python_executable(),
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
        python_executable: python_executable(),
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
        python_executable: python_executable(),
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
    engine.activate();
    engine.transcribe(request()).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_finishes_inflight_request_then_honors_pending_unload() {
    let root = temp_dir("pending-unload");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("slow_worker.py");
    let starts = root.join("starts.txt");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import pathlib
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--startup-marker", required=True)
args = parser.parse_args()
with pathlib.Path(args.startup_marker).open("a", encoding="utf-8") as output:
    output.write("started\n")
for line in sys.stdin:
    request = json.loads(line)
    time.sleep(0.15)
    print(json.dumps({"id": request["id"], "text": "ok"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: python_executable(),
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

    let in_flight_engine = engine.clone();
    let in_flight = tokio::spawn(async move { in_flight_engine.transcribe(request()).await });
    for _ in 0..100 {
        if starts.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(starts.is_file(), "worker did not start");
    assert!(engine.unload());
    assert_eq!(in_flight.await.unwrap().unwrap().text, "ok");

    engine.activate();
    assert_eq!(engine.transcribe(request()).await.unwrap().text, "ok");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_deselection_invalidates_requests_already_waiting_for_worker() {
    let root = temp_dir("queued-unload");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("slow_worker.py");
    let starts = root.join("starts.txt");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import pathlib
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--startup-marker", required=True)
args = parser.parse_args()
with pathlib.Path(args.startup_marker).open("a", encoding="utf-8") as output:
    output.write("started\n")
for line in sys.stdin:
    request = json.loads(line)
    time.sleep(0.15)
    print(json.dumps({"id": request["id"], "text": "ok"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: python_executable(),
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

    let first_engine = engine.clone();
    let first = tokio::spawn(async move { first_engine.transcribe(request()).await });
    for _ in 0..100 {
        if starts.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(starts.is_file(), "worker did not start");

    let queued_engine = engine.clone();
    let queued = tokio::spawn(async move { queued_engine.transcribe(request()).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(engine.unload());

    assert_eq!(first.await.unwrap().unwrap().text, "ok");
    assert!(queued.await.unwrap().is_err());
    engine.activate();
    assert_eq!(engine.transcribe(request()).await.unwrap().text, "ok");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_restarts_after_worker_reports_inference_error() {
    let root = temp_dir("worker-error");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("error_worker.py");
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
        print(json.dumps({"id": request["id"], "error": "session poisoned"}), flush=True)
    else:
        print(json.dumps({"id": request["id"], "text": "recovered"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: python_executable(),
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
    assert_eq!(engine.transcribe(request()).await.unwrap().text, "recovered");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn qwen_rejects_oversized_worker_response_and_restarts() {
    let root = temp_dir("oversized-response");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("oversized_worker.py");
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
        sys.stdout.write("x" * 1048577)
        sys.stdout.flush()
    else:
        print(json.dumps({"id": request["id"], "text": "recovered"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: python_executable(),
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

    let error = engine.transcribe(request()).await.unwrap_err();
    assert!(error.to_string().contains("exceeded 1 MiB"));
    assert_eq!(engine.transcribe(request()).await.unwrap().text, "recovered");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "started\nstarted\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn qwen_unload_reaps_worker_without_an_active_tokio_runtime() {
    let root = temp_dir("sync-unload");
    std::fs::create_dir_all(&root).unwrap();
    let worker = root.join("pid_worker.py");
    let pid_file = root.join("worker.pid");
    std::fs::write(
        &worker,
        r#"
import argparse
import json
import os
import pathlib
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--model", required=True)
parser.add_argument("--pid-file", required=True)
args = parser.parse_args()
pathlib.Path(args.pid_file).write_text(str(os.getpid()), encoding="utf-8")
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id": request["id"], "text": "ok"}), flush=True)
"#,
    )
    .unwrap();
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    let engine = QwenAsr::new(QwenAsrConfig {
        python_executable: python_executable(),
        worker_script: worker,
        model_dir: model,
        language: None,
        timeout: std::time::Duration::from_secs(5),
        extra_args: vec!["--pid-file".into(), pid_file.display().to_string()],
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(engine.transcribe(AsrRequest {
            samples: vec![0.0; 1_600],
            sample_rate: 16_000,
            hotwords: vec![],
        }))
        .unwrap();
    drop(runtime);

    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert!(engine.unload());
    for _ in 0..100 {
        if !std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir_all(root);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("Qwen worker process {pid} was not reaped after unload");
}
