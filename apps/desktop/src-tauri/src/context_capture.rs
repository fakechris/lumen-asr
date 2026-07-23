//! Immutable local context capture and exact per-stage input provenance.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use lumen_context::{
    ArtifactPayload, CaptureId, CaptureProfile, CaptureRequest, CaptureSession, CaptureTrigger,
    ContextCollector, ContextConfig, ContextSealer, ContextSnapshot, PrivacyPolicy, SourceKind,
    SourceSelection, SourceState, TargetHint, TriggerKind,
};
use lumen_store::{
    ContextInputRef, ContextSnapshotRecord, ContextStageUsage, PipelineStage, Store,
};
use uuid::Uuid;

use crate::config::ContextCaptureConfig;

#[derive(Clone)]
pub struct ActiveContextCapture {
    pub session_id: Uuid,
    pub capture_id: CaptureId,
    pub target_generation: u64,
    session: Option<Arc<CaptureSession>>,
    unavailable_reason: Option<String>,
    sealer: Option<Arc<ContextSealer>>,
    encryption: String,
    root: PathBuf,
    profile: CaptureProfile,
    started_at: chrono::DateTime<Utc>,
    freeze_deadline: Duration,
    late_deadline: Duration,
    persistence_lock: Arc<Mutex<()>>,
}

pub struct ContextRecorder {
    enabled: bool,
    collector: Option<ContextCollector>,
    sealer: Option<Arc<ContextSealer>>,
    encryption: String,
    initialization_error: Option<String>,
    active: Mutex<Option<ActiveContextCapture>>,
    generation: AtomicU64,
    root: PathBuf,
    freeze_deadline: Duration,
    late_deadline: Duration,
    profile: CaptureProfile,
    persistence_lock: Arc<Mutex<()>>,
}

pub struct StageUsageInput<'a> {
    pub capture_id: Option<Uuid>,
    pub attempt_id: Uuid,
    pub stage: PipelineStage,
    pub sources: Vec<String>,
    pub projection: Option<&'a [u8]>,
    pub captured: bool,
    pub selected: bool,
    pub consumed: bool,
    pub sent: bool,
    pub not_used_reason: Option<String>,
}

impl ContextRecorder {
    pub fn new(config: &ContextCaptureConfig, data_dir: &Path) -> Self {
        let root = data_dir.join("context");
        let _ = fs::create_dir_all(&root);
        let (sealer, encryption, key_error) = initialize_sealer(&root);
        Self::new_with_components(config, data_dir, sealer, encryption, key_error)
    }

    fn new_with_components(
        config: &ContextCaptureConfig,
        data_dir: &Path,
        sealer: Option<Arc<ContextSealer>>,
        encryption: String,
        key_error: Option<String>,
    ) -> Self {
        let profile = parse_profile(&config.profile);
        let collector = ContextCollector::new(
            ContextConfig {
                ax_max_chars: config.max_chars.clamp(1_000, 1_000_000),
                capture_all_displays: false,
                max_payload_bytes_per_capture: 8 * 1024 * 1024,
                ..ContextConfig::default()
            },
            None,
        )
        .map_err(|error| error.to_string());
        let (collector, collector_error) = match collector {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        let initialization_error = key_error.or(collector_error);
        let root = data_dir.join("context");
        let _ = fs::create_dir_all(&root);
        Self {
            enabled: config.enabled,
            collector,
            sealer,
            encryption,
            initialization_error,
            active: Mutex::new(None),
            generation: AtomicU64::new(0),
            root,
            freeze_deadline: Duration::from_millis(config.freeze_deadline_ms.clamp(1, 5_000)),
            late_deadline: Duration::from_millis(config.late_deadline_ms.clamp(1, 60_000)),
            profile,
            persistence_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Start one capture generation and allocate the session identity used by
    /// audio, context, attempts, and later edit feedback.
    pub fn begin(&self, target_hint: Option<TargetHint>) -> Uuid {
        let session_id = Uuid::new_v4();
        let capture_id = CaptureId::new();
        let target_generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let started_at = Utc::now();
        let unavailable_reason = if !self.enabled {
            Some("context_capture_disabled".to_owned())
        } else if self.sealer.is_none() {
            Some(
                self.initialization_error
                    .clone()
                    .unwrap_or_else(|| "context_encryption_unavailable".to_owned()),
            )
        } else if self.collector.is_none() {
            Some(
                self.initialization_error
                    .clone()
                    .unwrap_or_else(|| "context_collector_unavailable".to_owned()),
            )
        } else {
            None
        };
        let session = if unavailable_reason.is_none() {
            self.collector.as_ref().and_then(|collector| {
                collector
                    .begin(CaptureRequest {
                        capture_id,
                        consumer_session_id: session_id,
                        target_generation,
                        profile: self.profile,
                        sources: sources_for_profile(self.profile),
                        trigger: CaptureTrigger {
                            kind: TriggerKind::DictationHotkey,
                            pressed_at: started_at,
                            released_at: None,
                        },
                        requested_at: started_at,
                        target_hint,
                        privacy_policy: PrivacyPolicy {
                            capture_raw_text: true,
                            capture_screenshots: false,
                            ..PrivacyPolicy::default()
                        },
                    })
                    .map(Arc::new)
                    .map_err(|error| {
                        tracing::warn!(error = %error, "context capture start failed");
                    })
                    .ok()
            })
        } else {
            None
        };
        let unavailable_reason = if session.is_none() && unavailable_reason.is_none() {
            Some("context_capture_start_failed".to_owned())
        } else {
            unavailable_reason
        };
        let active = ActiveContextCapture {
            session_id,
            capture_id,
            target_generation,
            session,
            unavailable_reason,
            sealer: self.sealer.clone(),
            encryption: self.encryption.clone(),
            root: self.root.clone(),
            profile: self.profile,
            started_at,
            freeze_deadline: self.freeze_deadline,
            late_deadline: self.late_deadline,
            persistence_lock: Arc::clone(&self.persistence_lock),
        };
        if let Ok(mut current) = self.active.lock() {
            *current = Some(active);
        }
        session_id
    }

    pub fn take_active(&self) -> Option<ActiveContextCapture> {
        self.active.lock().ok().and_then(|mut active| active.take())
    }

    pub fn clear_active(&self) {
        if let Ok(mut active) = self.active.lock() {
            *active = None;
        }
    }

    /// Persist the exact serialized projection offered to one pipeline stage.
    pub fn record_stage_usage(
        &self,
        input: StageUsageInput<'_>,
    ) -> Result<ContextStageUsage, String> {
        persist_stage_usage(
            &self.root,
            self.sealer.as_deref(),
            &self.persistence_lock,
            input,
        )
    }
}

impl ActiveContextCapture {
    /// Freeze and persist the exact revision linked by the attempt.
    pub async fn freeze(&self, store: &Mutex<Option<Store>>) -> Result<ContextInputRef, String> {
        self.persist(store, self.freeze_deadline).await
    }

    /// Append a later archival revision without changing the attempt input.
    pub async fn archive(&self, store: &Mutex<Option<Store>>) -> Result<ContextInputRef, String> {
        self.persist(store, self.late_deadline).await
    }

    async fn persist(
        &self,
        store: &Mutex<Option<Store>>,
        deadline: Duration,
    ) -> Result<ContextInputRef, String> {
        let record = if let (Some(session), Some(sealer)) = (&self.session, &self.sealer) {
            let snapshot = session.snapshot(Instant::now() + deadline).await;
            let root = self.root.clone();
            let session_id = self.session_id;
            let sealer = Arc::clone(sealer);
            let encryption = self.encryption.clone();
            let persistence_lock = Arc::clone(&self.persistence_lock);
            tokio::task::spawn_blocking(move || {
                let _guard = persistence_lock
                    .lock()
                    .map_err(|_| "context persistence lock poisoned".to_owned())?;
                persist_snapshot(&root, session_id, snapshot, &sealer, &encryption)
            })
            .await
            .map_err(|error| format!("context persistence task failed: {error}"))??
        } else {
            unavailable_record(self)
        };
        {
            let guard = store
                .lock()
                .map_err(|_| "store lock poisoned while saving context".to_owned())?;
            let store = guard
                .as_ref()
                .ok_or_else(|| "database unavailable while saving context".to_owned())?;
            store
                .save_context_snapshot(&record)
                .map_err(|error| error.to_string())?;
        }
        Ok(context_ref(&record))
    }
}

fn persist_stage_usage(
    root: &Path,
    sealer: Option<&ContextSealer>,
    persistence_lock: &Mutex<()>,
    input: StageUsageInput<'_>,
) -> Result<ContextStageUsage, String> {
    let StageUsageInput {
        capture_id,
        attempt_id,
        stage,
        sources,
        projection,
        captured,
        selected,
        consumed,
        sent,
        not_used_reason,
    } = input;
    let Some(projection) = projection else {
        return Ok(ContextStageUsage {
            stage,
            sources,
            projection_schema_version: 1,
            projection_path: None,
            projection_hash: None,
            projection_chars: 0,
            captured,
            selected,
            consumed,
            sent,
            not_used_reason,
        });
    };
    let hash = blake3::hash(projection).to_hex().to_string();
    let chars = String::from_utf8_lossy(projection).chars().count();
    let capture_segment = capture_id
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unlinked".to_owned());
    let path = root
        .join(&capture_segment)
        .join("usage")
        .join(attempt_id.to_string())
        .join(format!("{}.sealed.json", stage_name(stage)));
    let sealer = sealer.ok_or_else(|| "context projection encryption unavailable".to_owned())?;
    let aad = format!(
        "lumen-context:v1:{}:attempt:{}:stage:{}",
        capture_segment,
        attempt_id,
        stage_name(stage)
    );
    let sealed = sealer
        .seal_json(projection, aad.as_bytes())
        .map_err(|error| error.to_string())?;
    let _guard = persistence_lock
        .lock()
        .map_err(|_| "context persistence lock poisoned".to_owned())?;
    write_atomic(&path, &sealed)?;
    Ok(ContextStageUsage {
        stage,
        sources,
        projection_schema_version: 1,
        projection_path: Some(path.display().to_string()),
        projection_hash: Some(hash),
        projection_chars: u32::try_from(chars).unwrap_or(u32::MAX),
        captured,
        selected,
        consumed,
        sent,
        not_used_reason,
    })
}

fn context_ref(record: &ContextSnapshotRecord) -> ContextInputRef {
    ContextInputRef {
        capture_id: record.capture_id,
        revision: record.revision,
        snapshot_hash: record.sanitized_hash.clone(),
        context_schema_version: record.schema_version,
        capture_profile: record.profile.clone(),
        source_presence_bitmap: record.source_presence_bitmap,
        source_status_summary: record.status.clone(),
    }
}

fn unavailable_record(active: &ActiveContextCapture) -> ContextSnapshotRecord {
    let reason = active
        .unavailable_reason
        .clone()
        .unwrap_or_else(|| "context_capture_unavailable".to_owned());
    let status_json = serde_json::json!({
        "capture": {
            "state": "unavailable",
            "reason": reason,
        }
    })
    .to_string();
    let now = Utc::now();
    ContextSnapshotRecord {
        capture_id: active.capture_id.0,
        session_id: active.session_id,
        revision: 1,
        schema_version: 1,
        profile: profile_name(active.profile).to_owned(),
        target_generation: active.target_generation,
        started_at: active.started_at,
        frozen_at: now,
        completed_at: Some(now),
        manifest_path: String::new(),
        source_presence_bitmap: 0,
        source_status_json: status_json.clone(),
        sanitized_hash: blake3::hash(status_json.as_bytes()).to_hex().to_string(),
        encryption: "none".to_owned(),
        status: "unavailable".to_owned(),
    }
}

fn persist_snapshot(
    root: &Path,
    session_id: Uuid,
    snapshot: ContextSnapshot,
    sealer: &ContextSealer,
    encryption: &str,
) -> Result<ContextSnapshotRecord, String> {
    let capture_dir = root.join(snapshot.manifest.capture_id.to_string());
    fs::create_dir_all(&capture_dir).map_err(|error| error.to_string())?;
    for artifact in &snapshot.payloads {
        let destination = capture_dir.join(format!(
            "artifact.r{:04}-{}.sealed.json",
            snapshot.manifest.revision, artifact.descriptor.artifact_id
        ));
        let plaintext = match &artifact.payload {
            ArtifactPayload::Bytes { bytes, .. } => bytes.to_vec(),
            ArtifactPayload::File {
                path,
                delete_after_import,
                ..
            } => {
                let bytes = fs::read(path).map_err(|error| error.to_string())?;
                if *delete_after_import {
                    let _ = fs::remove_file(path);
                }
                bytes
            }
        };
        let aad = format!(
            "lumen-context:v1:{}:artifact:{}",
            snapshot.manifest.capture_id, artifact.descriptor.artifact_id
        );
        let sealed = sealer
            .seal_json(&plaintext, aad.as_bytes())
            .map_err(|error| error.to_string())?;
        write_atomic(&destination, &sealed)?;
    }

    let manifest_json =
        serde_json::to_vec(&snapshot.manifest).map_err(|error| error.to_string())?;
    let manifest_path = capture_dir.join(format!(
        "manifest.r{:04}.v{}.sealed.json",
        snapshot.manifest.revision, snapshot.manifest.schema_version
    ));
    let aad = format!(
        "lumen-context:v1:{}:manifest:{}",
        snapshot.manifest.capture_id, snapshot.manifest.revision
    );
    let sealed = sealer
        .seal_json(&manifest_json, aad.as_bytes())
        .map_err(|error| error.to_string())?;
    write_atomic(&manifest_path, &sealed)?;

    let source_status_json = serde_json::to_string(&snapshot.manifest.source_status)
        .map_err(|error| error.to_string())?;
    let terminal = snapshot.manifest.all_requested_sources_terminal();
    Ok(ContextSnapshotRecord {
        capture_id: snapshot.manifest.capture_id.0,
        session_id,
        revision: snapshot.manifest.revision,
        schema_version: snapshot.manifest.schema_version,
        profile: profile_name(snapshot.manifest.profile).to_owned(),
        target_generation: snapshot.manifest.target_generation,
        started_at: snapshot.manifest.requested_at,
        frozen_at: snapshot.manifest.frozen_at,
        completed_at: terminal.then_some(snapshot.manifest.frozen_at),
        manifest_path: manifest_path.display().to_string(),
        source_presence_bitmap: source_presence_bitmap(&snapshot),
        source_status_json,
        sanitized_hash: blake3::hash(&manifest_json).to_hex().to_string(),
        encryption: encryption.to_owned(),
        status: if terminal { "complete" } else { "partial" }.to_owned(),
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("data"),
        Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    Ok(())
}

fn initialize_sealer(root: &Path) -> (Option<Arc<ContextSealer>>, String, Option<String>) {
    match ContextSealer::from_macos_keychain("com.lumenopen.asr.context", "capture-key-v1") {
        Ok(sealer) => (
            Some(Arc::new(sealer)),
            "chacha20_poly1305:macos_keychain".to_owned(),
            None,
        ),
        Err(keychain_error) => match load_or_create_local_key(root) {
            Ok(key) => (
                Some(Arc::new(ContextSealer::from_key(key))),
                "chacha20_poly1305:local_key_file".to_owned(),
                Some(format!("keychain fallback: {keychain_error}")),
            ),
            Err(file_error) => (
                None,
                "none".to_owned(),
                Some(format!(
                    "keychain error: {keychain_error}; local key error: {file_error}"
                )),
            ),
        },
    }
}

fn load_or_create_local_key(root: &Path) -> Result<[u8; 32], String> {
    let path = root.join(".capture-key");
    if let Ok(bytes) = fs::read(&path) {
        return bytes
            .try_into()
            .map_err(|_| "local context key has invalid length".to_owned());
    }
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(first.as_bytes());
    key[16..].copy_from_slice(second.as_bytes());
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&key).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = fs::read(&path).map_err(|read_error| read_error.to_string())?;
            bytes
                .try_into()
                .map_err(|_| "local context key has invalid length".to_owned())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn source_presence_bitmap(snapshot: &ContextSnapshot) -> u64 {
    SourceKind::ALL
        .iter()
        .enumerate()
        .filter(|(_, source)| {
            snapshot
                .manifest
                .source_status
                .get(source)
                .is_some_and(|status| {
                    matches!(status.state, SourceState::Succeeded | SourceState::Empty)
                })
        })
        .fold(0_u64, |bitmap, (index, _)| bitmap | (1_u64 << index))
}

fn parse_profile(value: &str) -> CaptureProfile {
    match value {
        "metadata" => CaptureProfile::Metadata,
        "editor" => CaptureProfile::Editor,
        _ => CaptureProfile::Visible,
    }
}

fn profile_name(profile: CaptureProfile) -> &'static str {
    match profile {
        CaptureProfile::Metadata => "metadata",
        CaptureProfile::Editor => "editor",
        CaptureProfile::Visible => "visible",
        CaptureProfile::Vision | CaptureProfile::FullLocal => "visible",
    }
}

fn sources_for_profile(profile: CaptureProfile) -> SourceSelection {
    match profile {
        CaptureProfile::Metadata => SourceSelection::from_sources([SourceKind::Target]),
        CaptureProfile::Editor => {
            SourceSelection::from_sources([SourceKind::Target, SourceKind::EditorAx])
        }
        CaptureProfile::Visible | CaptureProfile::Vision | CaptureProfile::FullLocal => {
            SourceSelection::from_sources([
                SourceKind::Target,
                SourceKind::EditorAx,
                SourceKind::AxVisible,
                SourceKind::Browser,
            ])
        }
    }
}

fn stage_name(stage: PipelineStage) -> &'static str {
    match stage {
        PipelineStage::Capture => "capture",
        PipelineStage::Preprocess => "preprocess",
        PipelineStage::Asr => "asr",
        PipelineStage::Enhancement => "enhancement",
        PipelineStage::Corrector => "corrector",
        PipelineStage::Insert => "insert",
        PipelineStage::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_context::{ContextManifest, SealedContextEnvelope};
    use std::process::Command;

    fn test_recorder(config: &ContextCaptureConfig, root: &Path) -> ContextRecorder {
        ContextRecorder::new_with_components(
            config,
            root,
            Some(Arc::new(ContextSealer::from_key([17_u8; 32]))),
            "chacha20_poly1305:test".to_owned(),
            None,
        )
    }

    #[tokio::test]
    async fn freeze_persists_decryptable_snapshot_and_returns_exact_reference() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            profile: "metadata".into(),
            freeze_deadline_ms: 2_000,
            ..ContextCaptureConfig::default()
        };
        let recorder = test_recorder(&config, directory.path());
        let session_id = recorder.begin(None);
        let active = recorder.take_active().unwrap();
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite")).unwrap(),
        ));

        let input_ref = active.freeze(&store).await.unwrap();
        let records = store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(input_ref.capture_id, records[0].capture_id);
        assert_eq!(input_ref.revision, records[0].revision);
        assert_eq!(input_ref.snapshot_hash, records[0].sanitized_hash);

        let envelope: SealedContextEnvelope =
            serde_json::from_slice(&fs::read(&records[0].manifest_path).unwrap()).unwrap();
        let aad = format!(
            "lumen-context:v1:{}:manifest:{}",
            records[0].capture_id, records[0].revision
        );
        let json = ContextSealer::from_key([17_u8; 32])
            .open(&envelope, aad.as_bytes())
            .unwrap();
        let manifest: ContextManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(manifest.consumer_session_id, session_id);
    }

    #[tokio::test]
    async fn unavailable_capture_is_still_auditable() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            enabled: false,
            ..ContextCaptureConfig::default()
        };
        let recorder = ContextRecorder::new_with_components(
            &config,
            directory.path(),
            None,
            "none".to_owned(),
            Some("test unavailable".to_owned()),
        );
        let session_id = recorder.begin(None);
        let active = recorder.take_active().unwrap();
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite")).unwrap(),
        ));

        let input_ref = active.freeze(&store).await.unwrap();
        assert_eq!(input_ref.source_status_summary, "unavailable");
        let records = store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, "unavailable");
        assert!(records[0].source_status_json.contains("disabled"));
    }

    #[test]
    fn stage_usage_preserves_the_exact_serialized_projection() {
        let directory = tempfile::tempdir().unwrap();
        let recorder = test_recorder(&ContextCaptureConfig::default(), directory.path());
        recorder.begin(None);
        let active = recorder.take_active().unwrap();
        let attempt_id = Uuid::new_v4();
        let projection = br#"{"terms":["Codex"],"replacements":[]}"#;

        let usage = recorder
            .record_stage_usage(StageUsageInput {
                capture_id: Some(active.capture_id.0),
                attempt_id,
                stage: PipelineStage::Corrector,
                sources: vec!["personal_dictionary".into()],
                projection: Some(projection),
                captured: true,
                selected: true,
                consumed: true,
                sent: true,
                not_used_reason: None,
            })
            .unwrap();

        assert!(usage.sent);
        assert_eq!(
            usage.projection_hash.as_deref(),
            Some(blake3::hash(projection).to_hex().as_str())
        );
        let envelope: SealedContextEnvelope =
            serde_json::from_slice(&fs::read(usage.projection_path.as_ref().unwrap()).unwrap())
                .unwrap();
        let aad = format!(
            "lumen-context:v1:{}:attempt:{}:stage:corrector",
            active.capture_id, attempt_id
        );
        let opened = ContextSealer::from_key([17_u8; 32])
            .open(&envelope, aad.as_bytes())
            .unwrap();
        assert_eq!(opened, projection);
    }

    #[tokio::test]
    #[ignore = "requires a logged-in macOS session and Accessibility permission"]
    async fn live_textedit_capture_round_trips_visible_context() {
        let directory = tempfile::tempdir().unwrap();
        let document = directory.path().join("lumen-context-e2e.txt");
        let marker = format!("LUMEN_CONTEXT_E2E_{}", Uuid::new_v4());
        fs::write(&document, &marker).unwrap();
        assert!(Command::new("open")
            .args(["-a", "TextEdit"])
            .arg(&document)
            .status()
            .unwrap()
            .success());
        std::thread::sleep(Duration::from_millis(1_000));

        let config = ContextCaptureConfig {
            freeze_deadline_ms: 3_000,
            ..ContextCaptureConfig::default()
        };
        let recorder = test_recorder(&config, directory.path());
        let session_id = recorder.begin(Some(TargetHint {
            app_name: Some("TextEdit".into()),
            bundle_id: Some("com.apple.TextEdit".into()),
            ..TargetHint::default()
        }));
        let active = recorder.take_active().unwrap();
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite")).unwrap(),
        ));
        active.freeze(&store).await.unwrap();

        let record = store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap()
            .remove(0);
        let envelope: SealedContextEnvelope =
            serde_json::from_slice(&fs::read(&record.manifest_path).unwrap()).unwrap();
        let aad = format!(
            "lumen-context:v1:{}:manifest:{}",
            record.capture_id, record.revision
        );
        let json = ContextSealer::from_key([17_u8; 32])
            .open(&envelope, aad.as_bytes())
            .unwrap();
        let manifest: ContextManifest = serde_json::from_slice(&json).unwrap();
        let serialized = serde_json::to_string(&manifest).unwrap();

        let _ = Command::new("osascript")
            .args([
                "-e",
                "tell application \"TextEdit\" to close front document saving no",
            ])
            .status();
        assert!(
            serialized.contains(&marker),
            "captured sources did not include the visible TextEdit marker: {}",
            record.source_status_json
        );
    }
}
