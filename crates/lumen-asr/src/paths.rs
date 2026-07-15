//! Resolve offline ASR model directories shared by all Lumen applications.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const ENV_LUMEN_MODELS_DIR: &str = "LUMEN_MODELS_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCandidate {
    pub engine: String,
    pub path: PathBuf,
    pub label: String,
    pub ready: bool,
    pub source: String,
}

pub fn user_home_dir() -> PathBuf {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(path) = nonempty_env_path(key) {
            return path;
        }
    }
    match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
        (Some(drive), Some(path)) if !drive.is_empty() && !path.is_empty() => {
            let mut home = PathBuf::from(drive);
            home.push(path);
            home
        }
        _ => std::env::temp_dir(),
    }
}

fn nonempty_env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn lumen_models_dir() -> PathBuf {
    lumen_models_dir_with_override(None)
}

pub fn lumen_models_dir_with_override(override_root: Option<&Path>) -> PathBuf {
    if let Some(root) = override_root.filter(|path| !path.as_os_str().is_empty()) {
        return root.to_path_buf();
    }
    if let Some(root) = nonempty_env_path(ENV_LUMEN_MODELS_DIR) {
        return root;
    }
    let home = user_home_dir();
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Lumen/models")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".lumen/models")
    }
}

/// Compatibility alias for callers written before the cluster-wide path rename.
pub fn app_models_dir() -> PathBuf {
    lumen_models_dir()
}

pub fn shared_sensevoice_dir(models_root: Option<&Path>) -> PathBuf {
    lumen_models_dir_with_override(models_root).join("sensevoice")
}

pub fn shared_whisper_dir(models_root: Option<&Path>) -> PathBuf {
    lumen_models_dir_with_override(models_root).join("whisper")
}

pub fn legacy_model_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/Application Support/LumenAsr/models"),
        home.join("Library/Application Support/LumenNavi/models"),
        home.join(".lumen-asr/models"),
        home.join(".lumen-navi/models"),
    ]
}

pub fn default_sensevoice_dir() -> PathBuf {
    default_sensevoice_dir_with_root(None)
}

pub fn default_sensevoice_dir_with_root(models_root: Option<&Path>) -> PathBuf {
    if let Some(path) = nonempty_env_path("LUMEN_SENSEVOICE_DIR") {
        return path;
    }
    let shared = shared_sensevoice_dir(models_root);
    if sensevoice_ready(&shared) {
        return shared;
    }
    for (path, _) in sensevoice_discovery_paths(models_root) {
        if path != shared && sensevoice_ready(&path) {
            return path;
        }
    }
    shared
}

pub fn default_whisper_dir() -> PathBuf {
    default_whisper_dir_with_root(None)
}

pub fn default_whisper_dir_with_root(models_root: Option<&Path>) -> PathBuf {
    if let Some(path) = nonempty_env_path("LUMEN_WHISPER_DIR") {
        return path;
    }
    let shared = shared_whisper_dir(models_root);
    if whisper_ready(&shared) {
        return shared;
    }
    for (path, _) in whisper_discovery_paths(models_root) {
        if path != shared && whisper_ready(&path) {
            return path;
        }
    }
    shared
}

fn sensevoice_discovery_paths(models_root: Option<&Path>) -> Vec<(PathBuf, &'static str)> {
    let shared_root = lumen_models_dir_with_override(models_root);
    let mut paths = vec![(shared_root.join("sensevoice"), "lumen-shared")];
    if let Ok(entries) = std::fs::read_dir(&shared_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && entry.file_name() != "sensevoice"
                && !entry.file_name().to_string_lossy().contains("extract")
                && sensevoice_ready(&path)
            {
                paths.push((path, "lumen-shared"));
            }
        }
    }
    for root in legacy_model_roots(&user_home_dir()) {
        let source = if root.to_string_lossy().contains("LumenAsr")
            || root.to_string_lossy().contains(".lumen-asr")
        {
            "legacy-lumen-asr"
        } else {
            "legacy-lumen-navi"
        };
        paths.push((root.join("sensevoice"), source));
    }
    for name in [
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
        "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
    ] {
        paths.push((
            user_home_dir().join(".coli/models").join(name),
            "coli-cache",
        ));
    }
    paths
}

fn whisper_discovery_paths(models_root: Option<&Path>) -> Vec<(PathBuf, &'static str)> {
    let shared_root = lumen_models_dir_with_override(models_root);
    let mut paths = vec![(shared_root.join("whisper"), "lumen-shared")];
    if let Ok(entries) = std::fs::read_dir(&shared_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && entry.file_name() != "whisper"
                && !entry.file_name().to_string_lossy().contains("extract")
                && whisper_ready(&path)
            {
                paths.push((path, "lumen-shared"));
            }
        }
    }
    for root in legacy_model_roots(&user_home_dir()) {
        let source = if root.to_string_lossy().contains("LumenAsr")
            || root.to_string_lossy().contains(".lumen-asr")
        {
            "legacy-lumen-asr"
        } else {
            "legacy-lumen-navi"
        };
        paths.push((root.join("whisper"), source));
    }
    for name in ["sherpa-onnx-whisper-tiny.en", "sherpa-onnx-whisper-base.en"] {
        paths.push((
            user_home_dir().join(".coli/models").join(name),
            "coli-cache",
        ));
    }
    paths
}

pub fn scan_model_candidates() -> Vec<ModelCandidate> {
    scan_model_candidates_with_root(None)
}

pub fn scan_model_candidates_with_root(models_root: Option<&Path>) -> Vec<ModelCandidate> {
    let mut out = Vec::new();
    if let Some(path) = nonempty_env_path("LUMEN_SENSEVOICE_DIR") {
        push_candidate(&mut out, "sensevoice", path, "env", false);
    }
    if let Some(path) = nonempty_env_path("LUMEN_WHISPER_DIR") {
        push_candidate(&mut out, "whisper", path, "env", false);
    }
    let shared_sensevoice = shared_sensevoice_dir(models_root);
    for (path, source) in sensevoice_discovery_paths(models_root) {
        let install_target = path == shared_sensevoice;
        push_candidate(&mut out, "sensevoice", path, source, install_target);
    }
    let shared_whisper = shared_whisper_dir(models_root);
    for (path, source) in whisper_discovery_paths(models_root) {
        let install_target = path == shared_whisper;
        push_candidate(&mut out, "whisper", path, source, install_target);
    }
    let mut seen = HashSet::new();
    out.retain(|candidate| seen.insert((candidate.engine.clone(), candidate.path.clone())));
    out.sort_by(|left, right| {
        candidate_score(right)
            .cmp(&candidate_score(left))
            .then_with(|| left.path.cmp(&right.path))
    });
    out
}

fn push_candidate(
    candidates: &mut Vec<ModelCandidate>,
    engine: &str,
    path: PathBuf,
    source: &str,
    install_target: bool,
) {
    let ready = match engine {
        "sensevoice" => sensevoice_ready(&path),
        "whisper" => whisper_ready(&path),
        _ => false,
    };
    if !ready && !install_target {
        return;
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let label = if ready {
        format!("{name} · {source}")
    } else {
        format!("{engine} · {source} — 下载目标（全 Lumen 应用共享）")
    };
    candidates.push(ModelCandidate {
        engine: engine.into(),
        path,
        label,
        ready,
        source: source.into(),
    });
}

fn candidate_score(candidate: &ModelCandidate) -> i32 {
    i32::from(candidate.ready) * 10
        + i32::from(candidate.source == "lumen-shared") * 5
        + i32::from(candidate.source == "env") * 8
}

pub fn sensevoice_ready(dir: &Path) -> bool {
    sensevoice_model_path(dir).is_some() && sensevoice_tokens_path(dir).is_some()
}

pub fn whisper_ready(dir: &Path) -> bool {
    whisper_encoder_path(dir).is_some()
        && whisper_decoder_path(dir).is_some()
        && whisper_tokens_path(dir).is_some()
}

pub fn sensevoice_model_path(dir: &Path) -> Option<PathBuf> {
    for name in ["model.int8.onnx", "model.onnx", "sensevoice.onnx"] {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn sensevoice_tokens_path(dir: &Path) -> Option<PathBuf> {
    let path = dir.join("tokens.txt");
    path.is_file().then_some(path)
}

pub fn whisper_encoder_path(dir: &Path) -> Option<PathBuf> {
    matching_file(dir, "encoder", ".onnx")
}

pub fn whisper_decoder_path(dir: &Path) -> Option<PathBuf> {
    matching_file(dir, "decoder", ".onnx")
}

pub fn whisper_tokens_path(dir: &Path) -> Option<PathBuf> {
    matching_file(dir, "tokens", ".txt").or_else(|| {
        let path = dir.join("tokens.txt");
        path.is_file().then_some(path)
    })
}

fn matching_file(dir: &Path, contains: &str, suffix: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(contains) && name.ends_with(suffix) {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("lumen-asr-{name}-{nonce}"))
    }

    #[test]
    fn shared_root_discovers_ready_model_in_custom_subdir() {
        let root = temp_dir("shared-custom");
        let custom = root.join("sherpa-sensevoice-custom");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("model.int8.onnx"), b"model").unwrap();
        std::fs::write(custom.join("tokens.txt"), b"tokens").unwrap();

        let candidates = scan_model_candidates_with_root(Some(&root));

        assert!(candidates.iter().any(|candidate| {
            candidate.engine == "sensevoice"
                && candidate.path == custom
                && candidate.source == "lumen-shared"
                && candidate.ready
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_roots_cover_macos_and_dot_directory_layouts() {
        let home = Path::new("/home/alice");

        assert_eq!(
            legacy_model_roots(home),
            vec![
                home.join("Library/Application Support/LumenAsr/models"),
                home.join("Library/Application Support/LumenNavi/models"),
                home.join(".lumen-asr/models"),
                home.join(".lumen-navi/models"),
            ]
        );
    }

    #[test]
    fn missing_shared_targets_are_still_listed_for_installation() {
        let root = temp_dir("shared-placeholder");
        let candidates = scan_model_candidates_with_root(Some(&root));

        assert!(candidates.iter().any(|candidate| {
            candidate.engine == "sensevoice"
                && candidate.path == root.join("sensevoice")
                && !candidate.ready
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.engine == "whisper"
                && candidate.path == root.join("whisper")
                && !candidate.ready
        }));
    }
}
