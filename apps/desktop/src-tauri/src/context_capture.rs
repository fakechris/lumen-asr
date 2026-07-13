//! Shadow-only context capture lifecycle and local persistence.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use lumen_context::{
    ArtifactPayload, BrowserSnapshotProvider, CaptureId, CaptureProfile, CaptureRequest,
    CaptureSession, CaptureTrigger, ContextCollector, ContextConfig, ContextSealer,
    ContextSnapshot, PrivacyPolicy, SourceKind, SourceSelection, SourceState, TargetHint,
    TriggerKind,
};
use lumen_store::{ContextSnapshotRecord, Store};
use uuid::Uuid;

use crate::config::ContextCaptureConfig;
use crate::AppState;

#[derive(Clone)]
pub struct ActiveContextCapture {
    pub session_id: Uuid,
    pub capture_id: CaptureId,
    pub target_generation: u64,
    session: Option<Arc<CaptureSession>>,
    sealer: Option<Arc<ContextSealer>>,
    epoch: u64,
    epoch_guard: Arc<AtomicU64>,
    persistence_lock: Arc<Mutex<()>>,
    root: PathBuf,
    freeze_deadline: Duration,
    late_deadline: Duration,
}

pub struct ContextCaptureState {
    enabled: bool,
    collector: Option<ContextCollector>,
    sealer: Option<Arc<ContextSealer>>,
    active: Mutex<Option<ActiveContextCapture>>,
    generation: AtomicU64,
    root: PathBuf,
    freeze_deadline: Duration,
    late_deadline: Duration,
    profile: CaptureProfile,
    retention: Duration,
    epoch: Arc<AtomicU64>,
    persistence_lock: Arc<Mutex<()>>,
}

impl ContextCaptureState {
    pub fn new(config: &ContextCaptureConfig, data_dir: &Path) -> Self {
        Self::new_with_browser(config, data_dir, None)
    }

    pub fn new_with_browser(
        config: &ContextCaptureConfig,
        data_dir: &Path,
        browser: Option<Arc<dyn BrowserSnapshotProvider>>,
    ) -> Self {
        let sealer = if config.enabled {
            ContextSealer::from_macos_keychain("com.lumen.asr.context", "capture-key-v1")
                .map(Arc::new)
                .map_err(|error| {
                    tracing::warn!(error = %error, "context encryption key initialization failed");
                })
                .ok()
        } else {
            None
        };
        Self::new_with_components(config, data_dir, browser, sealer)
    }

    fn new_with_components(
        config: &ContextCaptureConfig,
        data_dir: &Path,
        browser: Option<Arc<dyn BrowserSnapshotProvider>>,
        sealer: Option<Arc<ContextSealer>>,
    ) -> Self {
        let profile = parse_profile(&config.profile);
        let context_config = ContextConfig {
            capture_all_displays: config.capture_all_displays,
            screenshot_max_edge: config.screenshot_max_edge,
            ocr_helper_path: resolve_ocr_helper_path(&config.ocr_helper_path),
            browser_timeout_ms: config.browser_timeout_ms.clamp(1, 60_000),
            ..ContextConfig::default()
        };
        let collector = ContextCollector::new(context_config, browser)
            .map_err(|error| {
                tracing::warn!(error = %error, "context collector initialization failed");
                error
            })
            .ok();
        let root = data_dir.join("context");
        let _ = fs::create_dir_all(&root);
        Self {
            enabled: config.enabled && collector.is_some() && sealer.is_some(),
            collector,
            sealer,
            active: Mutex::new(None),
            generation: AtomicU64::new(0),
            root,
            freeze_deadline: Duration::from_millis(config.freeze_deadline_ms.clamp(1, 5_000)),
            late_deadline: Duration::from_millis(config.late_deadline_ms.clamp(1, 60_000)),
            profile,
            retention: Duration::from_secs(config.retention_hours.max(1).saturating_mul(3_600)),
            epoch: Arc::new(AtomicU64::new(0)),
            persistence_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn begin(&self, target_hint: Option<TargetHint>) -> Uuid {
        let session_id = Uuid::new_v4();
        let capture_id = CaptureId::new();
        let target_generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let epoch = self.epoch.load(Ordering::SeqCst);
        let session = if self.enabled {
            let now = Utc::now();
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
                            pressed_at: now,
                            released_at: None,
                        },
                        requested_at: now,
                        target_hint,
                        privacy_policy: PrivacyPolicy::default(),
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
        let active = ActiveContextCapture {
            session_id,
            capture_id,
            target_generation,
            session,
            sealer: self.sealer.clone(),
            epoch,
            epoch_guard: Arc::clone(&self.epoch),
            persistence_lock: Arc::clone(&self.persistence_lock),
            root: self.root.clone(),
            freeze_deadline: self.freeze_deadline,
            late_deadline: self.late_deadline,
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

    pub fn enforce_retention(&self, store: &Mutex<Option<Store>>) -> Result<usize, String> {
        let _persistence = self
            .persistence_lock
            .lock()
            .map_err(|_| "context persistence lock poisoned".to_owned())?;
        let cutoff = Utc::now()
            - chrono::Duration::from_std(self.retention)
                .map_err(|error| format!("invalid context retention: {error}"))?;
        let capture_ids = {
            let guard = store
                .lock()
                .map_err(|_| "store lock poisoned while pruning context".to_owned())?;
            match guard.as_ref() {
                Some(store) => store
                    .prune_context_captures_before(cutoff)
                    .map_err(|error| error.to_string())?,
                None => Vec::new(),
            }
        };
        for capture_id in &capture_ids {
            let _ = fs::remove_dir_all(self.root.join(capture_id.to_string()));
        }
        Ok(capture_ids.len())
    }

    pub fn wipe(&self, store: &Mutex<Option<Store>>) -> Result<usize, String> {
        let _persistence = self
            .persistence_lock
            .lock()
            .map_err(|_| "context persistence lock poisoned".to_owned())?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.clear_active();
        let deleted = {
            let guard = store
                .lock()
                .map_err(|_| "store lock poisoned while clearing context".to_owned())?;
            match guard.as_ref() {
                Some(store) => store
                    .clear_context_snapshots()
                    .map_err(|error| error.to_string())?,
                None => 0,
            }
        };
        if self.root.exists() {
            fs::remove_dir_all(&self.root).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(&self.root).map_err(|error| error.to_string())?;
        Ok(deleted)
    }
}

fn resolve_ocr_helper_path(configured: &str) -> Option<PathBuf> {
    if !configured.trim().is_empty() {
        return Some(PathBuf::from(configured));
    }
    let sibling = std::env::current_exe()
        .ok()?
        .parent()?
        .join("lumen-asr-context-ocr-helper");
    sibling.is_file().then_some(sibling)
}

impl ActiveContextCapture {
    pub async fn persist_partial(&self, store: &Mutex<Option<Store>>) -> Result<bool, String> {
        self.persist(store, self.freeze_deadline).await
    }

    pub async fn persist_late(&self, store: &Mutex<Option<Store>>) -> Result<bool, String> {
        self.persist(store, self.late_deadline).await
    }

    async fn persist(
        &self,
        store: &Mutex<Option<Store>>,
        deadline: Duration,
    ) -> Result<bool, String> {
        let Some(session) = self.session.as_ref() else {
            return Ok(false);
        };
        let Some(sealer) = self.sealer.as_ref() else {
            return Ok(false);
        };
        if self.epoch_guard.load(Ordering::SeqCst) != self.epoch {
            return Ok(false);
        }
        let snapshot = session.snapshot(Instant::now() + deadline).await;
        let session_id = self.session_id;
        let root = self.root.clone();
        let sealer = Arc::clone(sealer);
        let epoch = self.epoch;
        let epoch_guard = Arc::clone(&self.epoch_guard);
        let persistence_lock = Arc::clone(&self.persistence_lock);
        let record = tokio::task::spawn_blocking(move || {
            let _persistence = persistence_lock
                .lock()
                .map_err(|_| "context persistence lock poisoned".to_owned())?;
            if epoch_guard.load(Ordering::SeqCst) != epoch {
                return Ok(None);
            }
            persist_snapshot(&root, session_id, snapshot, &sealer).map(Some)
        })
        .await
        .map_err(|error| format!("context persistence task failed: {error}"))??;
        let Some(record) = record else {
            return Ok(false);
        };
        let _persistence = self
            .persistence_lock
            .lock()
            .map_err(|_| "context persistence lock poisoned".to_owned())?;
        if self.epoch_guard.load(Ordering::SeqCst) != self.epoch {
            return Ok(false);
        }
        let guard = store
            .lock()
            .map_err(|_| "store lock poisoned while saving context".to_owned())?;
        if let Some(store) = guard.as_ref() {
            store
                .save_context_snapshot(&record)
                .map_err(|error| error.to_string())?;
        }
        Ok(true)
    }
}

#[tauri::command]
pub fn clear_context_data(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    state.context.wipe(&state.store)
}

fn persist_snapshot(
    root: &Path,
    session_id: Uuid,
    snapshot: ContextSnapshot,
    sealer: &ContextSealer,
) -> Result<ContextSnapshotRecord, String> {
    let capture_dir = root.join(snapshot.manifest.capture_id.to_string());
    fs::create_dir_all(&capture_dir).map_err(|error| error.to_string())?;
    for artifact in &snapshot.payloads {
        let extension = extension_for_media(&artifact.descriptor.media_type);
        let destination = capture_dir.join(format!(
            "artifact-{}.{}.sealed.json",
            artifact.descriptor.artifact_id, extension
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
    let compressed =
        zstd::stream::encode_all(manifest_json.as_slice(), 3).map_err(|error| error.to_string())?;
    let manifest_path = capture_dir.join(format!(
        "manifest.r{:04}.v{}.json.zst.sealed.json",
        snapshot.manifest.revision, snapshot.manifest.schema_version
    ));
    let manifest_aad = format!(
        "lumen-context:v1:{}:manifest:{}",
        snapshot.manifest.capture_id, snapshot.manifest.revision
    );
    let sealed_manifest = sealer
        .seal_json(&compressed, manifest_aad.as_bytes())
        .map_err(|error| error.to_string())?;
    write_atomic(&manifest_path, &sealed_manifest)?;

    let source_presence_bitmap = source_presence_bitmap(&snapshot);
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
        source_presence_bitmap,
        source_status_json,
        sanitized_hash: blake3::hash(&manifest_json).to_hex().to_string(),
        encryption: "chacha20_poly1305".to_owned(),
        status: if terminal { "complete" } else { "partial" }.to_owned(),
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
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

fn extension_for_media(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "application/json" => "json",
        _ => "bin",
    }
}

fn parse_profile(value: &str) -> CaptureProfile {
    match value {
        "metadata" => CaptureProfile::Metadata,
        "editor" => CaptureProfile::Editor,
        "visible" => CaptureProfile::Visible,
        "vision" => CaptureProfile::Vision,
        _ => CaptureProfile::FullLocal,
    }
}

fn profile_name(profile: CaptureProfile) -> &'static str {
    match profile {
        CaptureProfile::Metadata => "metadata",
        CaptureProfile::Editor => "editor",
        CaptureProfile::Visible => "visible",
        CaptureProfile::Vision => "vision",
        CaptureProfile::FullLocal => "full_local",
    }
}

fn sources_for_profile(profile: CaptureProfile) -> SourceSelection {
    match profile {
        CaptureProfile::Metadata => SourceSelection::from_sources([SourceKind::Target]),
        CaptureProfile::Editor => {
            SourceSelection::from_sources([SourceKind::Target, SourceKind::EditorAx])
        }
        CaptureProfile::Visible => SourceSelection::from_sources([
            SourceKind::Target,
            SourceKind::EditorAx,
            SourceKind::AxVisible,
            SourceKind::Browser,
        ]),
        CaptureProfile::Vision | CaptureProfile::FullLocal => SourceSelection::full_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_context::ContextManifest;

    fn directory_bytes(path: &Path) -> u64 {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    directory_bytes(&path)
                } else {
                    entry.metadata().map_or(0, |metadata| metadata.len())
                }
            })
            .sum()
    }

    #[tokio::test]
    async fn metadata_capture_persists_manifest_and_database_revision() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            enabled: true,
            profile: "metadata".to_owned(),
            freeze_deadline_ms: 2_000,
            ..ContextCaptureConfig::default()
        };
        let sealer = Arc::new(ContextSealer::from_key([11_u8; 32]));
        let state = ContextCaptureState::new_with_components(
            &config,
            directory.path(),
            None,
            Some(Arc::clone(&sealer)),
        );
        let session_id = state.begin(None);
        let active = state.take_active().unwrap();
        assert_eq!(active.session_id, session_id);

        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite3")).unwrap(),
        ));
        assert!(active.persist_partial(&store).await.unwrap());
        let records = store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].capture_id, active.capture_id.0);
        assert_eq!(records[0].target_generation, active.target_generation);

        let envelope: lumen_context::SealedContextEnvelope =
            serde_json::from_slice(&fs::read(&records[0].manifest_path).unwrap()).unwrap();
        let aad = format!(
            "lumen-context:v1:{}:manifest:{}",
            records[0].capture_id, records[0].revision
        );
        let compressed = sealer.open(&envelope, aad.as_bytes()).unwrap();
        let json = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        let manifest: ContextManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(manifest.capture_id.0, records[0].capture_id);
        assert_eq!(manifest.consumer_session_id, session_id);
        assert_eq!(manifest.revision, records[0].revision);

        assert_eq!(state.wipe(&store).unwrap(), 1);
        assert!(store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap()
            .is_empty());
        assert!(fs::read_dir(directory.path().join("context"))
            .unwrap()
            .next()
            .is_none());
    }

    #[tokio::test]
    async fn disabled_capture_still_allocates_session_without_writing_context() {
        let directory = tempfile::tempdir().unwrap();
        let state = ContextCaptureState::new(&ContextCaptureConfig::default(), directory.path());
        let session_id = state.begin(None);
        let active = state.take_active().unwrap();
        assert_eq!(active.session_id, session_id);
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite3")).unwrap(),
        ));
        assert!(!active.persist_partial(&store).await.unwrap());
        assert!(store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn wipe_invalidates_an_in_flight_late_writer() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            enabled: true,
            profile: "metadata".to_owned(),
            ..ContextCaptureConfig::default()
        };
        let state = ContextCaptureState::new_with_components(
            &config,
            directory.path(),
            None,
            Some(Arc::new(ContextSealer::from_key([12_u8; 32]))),
        );
        let session_id = state.begin(None);
        let active = state.take_active().unwrap();
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite3")).unwrap(),
        ));

        assert_eq!(state.wipe(&store).unwrap(), 0);
        assert!(!active.persist_late(&store).await.unwrap());
        assert!(store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .list_context_snapshots(session_id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn retention_removes_expired_rows_and_capture_directories() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            retention_hours: 1,
            ..ContextCaptureConfig::default()
        };
        let state = ContextCaptureState::new(&config, directory.path());
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite3")).unwrap(),
        ));
        let now = Utc::now();
        let expired_capture = Uuid::new_v4();
        let current_capture = Uuid::new_v4();
        let expired_session = Uuid::new_v4();
        let current_session = Uuid::new_v4();

        for capture_id in [expired_capture, current_capture] {
            let capture_dir = directory
                .path()
                .join("context")
                .join(capture_id.to_string());
            fs::create_dir_all(&capture_dir).unwrap();
            fs::write(capture_dir.join("artifact.sealed.json"), b"fixture").unwrap();
        }
        let expired_at = now - chrono::Duration::hours(2);
        let records = [
            ContextSnapshotRecord {
                capture_id: expired_capture,
                session_id: expired_session,
                revision: 1,
                schema_version: 1,
                profile: "metadata".to_owned(),
                target_generation: 1,
                started_at: expired_at,
                frozen_at: expired_at,
                completed_at: Some(expired_at),
                manifest_path: directory
                    .path()
                    .join("context")
                    .join(expired_capture.to_string())
                    .join("manifest.sealed.json")
                    .display()
                    .to_string(),
                source_presence_bitmap: 1,
                source_status_json: "{}".to_owned(),
                sanitized_hash: "expired".to_owned(),
                encryption: "chacha20_poly1305".to_owned(),
                status: "complete".to_owned(),
            },
            ContextSnapshotRecord {
                capture_id: current_capture,
                session_id: current_session,
                revision: 1,
                schema_version: 1,
                profile: "metadata".to_owned(),
                target_generation: 2,
                started_at: now,
                frozen_at: now,
                completed_at: Some(now),
                manifest_path: directory
                    .path()
                    .join("context")
                    .join(current_capture.to_string())
                    .join("manifest.sealed.json")
                    .display()
                    .to_string(),
                source_presence_bitmap: 1,
                source_status_json: "{}".to_owned(),
                sanitized_hash: "current".to_owned(),
                encryption: "chacha20_poly1305".to_owned(),
                status: "complete".to_owned(),
            },
        ];
        for record in &records {
            store
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .save_context_snapshot(record)
                .unwrap();
        }

        assert_eq!(state.enforce_retention(&store).unwrap(), 1);
        assert!(!directory
            .path()
            .join("context")
            .join(expired_capture.to_string())
            .exists());
        assert!(directory
            .path()
            .join("context")
            .join(current_capture.to_string())
            .exists());
        let guard = store.lock().unwrap();
        let store = guard.as_ref().unwrap();
        assert!(store
            .list_context_snapshots(expired_session)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.list_context_snapshots(current_session).unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn metadata_persistence_has_bounded_disk_curve_and_wipes_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let config = ContextCaptureConfig {
            enabled: true,
            profile: "metadata".to_owned(),
            freeze_deadline_ms: 2_000,
            ..ContextCaptureConfig::default()
        };
        let state = ContextCaptureState::new_with_components(
            &config,
            directory.path(),
            None,
            Some(Arc::new(ContextSealer::from_key([13_u8; 32]))),
        );
        let store = Mutex::new(Some(
            Store::open(directory.path().join("lumen.sqlite3")).unwrap(),
        ));
        let mut curve = Vec::new();

        for sample in 1..=100 {
            state.begin(None);
            let active = state.take_active().unwrap();
            assert!(active.persist_partial(&store).await.unwrap());
            if matches!(sample, 10 | 50 | 100) {
                curve.push(directory_bytes(&directory.path().join("context")));
            }
        }

        assert_eq!(curve.len(), 3);
        assert!(curve[0] > 0);
        assert!(curve[1] >= curve[0]);
        assert!(curve[2] >= curve[1]);
        assert!(curve[1] <= curve[0].saturating_mul(8));
        assert!(curve[2] <= curve[1].saturating_mul(3));
        assert!(curve[2] < 10 * 1024 * 1024);
        eprintln!("context persistence disk curve bytes at 10/50/100: {curve:?}");
        assert_eq!(state.wipe(&store).unwrap(), 100);
        assert_eq!(directory_bytes(&directory.path().join("context")), 0);
    }
}
