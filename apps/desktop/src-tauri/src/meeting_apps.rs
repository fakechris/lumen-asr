//! Runtime meeting/recording application catalog.
//!
//! The catalog is deliberately stored as an ordinary TOML file beside the
//! user's other application data. The app bundle contains only a seed file;
//! on first launch it is copied to the user-data location and is never
//! overwritten afterward. Detection, labels, and process-audio selection all
//! consult this shared registry, so adding an application never requires a
//! new binary.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use lumen_core::AppClass;
use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

const CATALOG_FILE_NAME: &str = "meeting-apps.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingAppKind {
    Meeting,
    Browser,
}

impl MeetingAppKind {
    fn app_class(self) -> AppClass {
        match self {
            Self::Meeting => AppClass::NativeMeeting,
            Self::Browser => AppClass::Browser,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeetingAppEntry {
    pub name: String,
    pub kind: MeetingAppKind,
    pub bundle_ids: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub detect: bool,
    #[serde(default = "enabled_by_default")]
    pub capture: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeetingAppCatalog {
    #[serde(default = "catalog_version")]
    pub version: u32,
    #[serde(default, rename = "application")]
    pub applications: Vec<MeetingAppEntry>,
}

fn catalog_version() -> u32 {
    1
}

impl Default for MeetingAppCatalog {
    fn default() -> Self {
        Self {
            version: catalog_version(),
            applications: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingAppCatalogDto {
    pub path: String,
    pub version: u32,
    pub applications: Vec<MeetingAppEntry>,
    pub load_error: Option<String>,
}

#[derive(Clone)]
pub struct MeetingAppRegistry {
    path: PathBuf,
    catalog: Arc<RwLock<MeetingAppCatalog>>,
    io_lock: Arc<Mutex<()>>,
    last_load_error: Arc<RwLock<Option<String>>>,
}

impl Default for MeetingAppRegistry {
    fn default() -> Self {
        Self::new(lumen_platform::default_data_dir().join(CATALOG_FILE_NAME))
    }
}

impl MeetingAppRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            catalog: Arc::new(RwLock::new(MeetingAppCatalog::default())),
            io_lock: Arc::new(Mutex::new(())),
            last_load_error: Arc::new(RwLock::new(None)),
        }
    }

    pub fn install_and_load(&self, app: &AppHandle) -> Result<(), String> {
        let result = (|| {
            if !self.path.exists() {
                let resource = app
                    .path()
                    .resolve(CATALOG_FILE_NAME, BaseDirectory::Resource)
                    .map_err(|error| format!("resolve {CATALOG_FILE_NAME}: {error}"))?;
                self.install_template(&resource)?;
            }
            self.reload()
        })();
        if let Err(error) = &result {
            self.set_load_error(Some(error.clone()));
        }
        result
    }

    fn install_template(&self, template: &Path) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("invalid catalog path: {}", self.path.display()))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("create meeting-app catalog directory: {error}"))?;
        let bytes = fs::read(template)
            .map_err(|error| format!("read catalog template {}: {error}", template.display()))?;
        atomic_create(&self.path, &bytes)?;
        Ok(())
    }

    pub fn reload(&self) -> Result<(), String> {
        let result = (|| {
            let _io = self
                .io_lock
                .lock()
                .map_err(|_| "meeting-app catalog I/O lock poisoned".to_string())?;
            let text = fs::read_to_string(&self.path)
                .map_err(|error| format!("read {}: {error}", self.path.display()))?;
            let catalog: MeetingAppCatalog = toml::from_str(&text)
                .map_err(|error| format!("parse {}: {error}", self.path.display()))?;
            validate_catalog(&catalog)?;
            let mut current = self
                .catalog
                .write()
                .map_err(|_| "meeting-app catalog lock poisoned".to_string())?;
            *current = catalog;
            Ok(())
        })();
        self.set_load_error(result.as_ref().err().cloned());
        result
    }

    pub fn save(&self, catalog: MeetingAppCatalog) -> Result<MeetingAppCatalogDto, String> {
        validate_catalog(&catalog)?;
        let text = toml::to_string_pretty(&catalog)
            .map_err(|error| format!("serialize meeting-app catalog: {error}"))?;
        let _io = self
            .io_lock
            .lock()
            .map_err(|_| "meeting-app catalog I/O lock poisoned".to_string())?;
        atomic_write(&self.path, text.as_bytes())?;
        let mut current = self
            .catalog
            .write()
            .map_err(|_| "meeting-app catalog lock poisoned".to_string())?;
        *current = catalog;
        drop(current);
        self.set_load_error(None);
        Ok(self.snapshot())
    }

    pub fn snapshot(&self) -> MeetingAppCatalogDto {
        let catalog = self
            .catalog
            .read()
            .map(|catalog| catalog.clone())
            .unwrap_or_default();
        MeetingAppCatalogDto {
            path: crate::display_path(&self.path),
            version: catalog.version,
            applications: catalog.applications,
            load_error: self
                .last_load_error
                .read()
                .ok()
                .and_then(|error| error.clone()),
        }
    }

    fn set_load_error(&self, error: Option<String>) {
        if let Ok(mut current) = self.last_load_error.write() {
            *current = error;
        }
    }

    pub fn classify(&self, bundle_id: &str) -> AppClass {
        self.find(bundle_id, true)
            .map(|entry| entry.kind.app_class())
            .unwrap_or(AppClass::Other)
    }

    pub fn label(&self, bundle_id: &str) -> String {
        self.find(bundle_id, false)
            .map(|entry| entry.name)
            .unwrap_or_else(|| bundle_id.to_string())
    }

    pub fn capture_enabled(&self, bundle_id: &str) -> bool {
        self.find(bundle_id, false)
            .is_some_and(|entry| entry.capture)
    }

    /// All configured native meeting-app bundle ids whose output may be
    /// captured for a manually started meeting. Browsers default to excluded:
    /// their whole-process scope needs an explicit per-prompt choice.
    pub fn manual_capture_bundle_ids(&self) -> Vec<String> {
        self.catalog
            .read()
            .map(|catalog| {
                catalog
                    .applications
                    .iter()
                    .filter(|entry| entry.capture && entry.kind == MeetingAppKind::Meeting)
                    .flat_map(|entry| entry.bundle_ids.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn find(&self, bundle_id: &str, require_detect: bool) -> Option<MeetingAppEntry> {
        let wanted = lumen_core::normalize_bundle_id(bundle_id).to_ascii_lowercase();
        self.catalog.read().ok().and_then(|catalog| {
            catalog
                .applications
                .iter()
                .find(|entry| {
                    (!require_detect || entry.detect)
                        && entry.bundle_ids.iter().any(|candidate| {
                            lumen_core::normalize_bundle_id(candidate).to_ascii_lowercase()
                                == wanted
                        })
                })
                .cloned()
        })
    }
}

fn validate_catalog(catalog: &MeetingAppCatalog) -> Result<(), String> {
    if catalog.version != catalog_version() {
        return Err(format!(
            "unsupported meeting-app catalog version {}; expected {}",
            catalog.version,
            catalog_version()
        ));
    }
    let mut seen = HashSet::new();
    for (index, entry) in catalog.applications.iter().enumerate() {
        if entry.name.trim().is_empty() {
            return Err(format!("application {} has an empty name", index + 1));
        }
        if entry.bundle_ids.is_empty() {
            return Err(format!(
                "application '{}' must contain at least one bundle id",
                entry.name
            ));
        }
        for bundle_id in &entry.bundle_ids {
            let normalized = lumen_core::normalize_bundle_id(bundle_id);
            if normalized.is_empty() {
                return Err(format!(
                    "application '{}' has an empty bundle id",
                    entry.name
                ));
            }
            let key = normalized.to_ascii_lowercase();
            if key.contains("com.lumenopen.asr") {
                return Err("Lumen itself cannot be a meeting capture target".to_string());
            }
            if !seen.insert(key) {
                return Err(format!("bundle id '{normalized}' appears more than once"));
            }
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid catalog path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create meeting-app catalog directory: {error}"))?;
    let temp = unique_temp_path(parent);
    fs::write(&temp, bytes).map_err(|error| format!("write {}: {error}", temp.display()))?;
    replace_file(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("replace {}: {error}", path.display())
    })
}

/// Install a fully-written seed without ever replacing a user-created file.
/// Linking a unique sibling temp into place makes the no-clobber decision
/// atomic even when two app processes start for the first time together.
fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid catalog path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create meeting-app catalog directory: {error}"))?;
    let temp = unique_temp_path(parent);
    fs::write(&temp, bytes).map_err(|error| format!("write {}: {error}", temp.display()))?;
    let linked = fs::hard_link(&temp, path);
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("install {}: {error}", path.display())),
    }
}

fn unique_temp_path(parent: &Path) -> PathBuf {
    parent.join(format!(
        ".{CATALOG_FILE_NAME}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ))
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> MeetingAppCatalog {
        MeetingAppCatalog {
            version: 1,
            applications: vec![
                MeetingAppEntry {
                    name: "Zoom".into(),
                    kind: MeetingAppKind::Meeting,
                    bundle_ids: vec!["us.zoom.xos".into()],
                    detect: true,
                    capture: true,
                },
                MeetingAppEntry {
                    name: "Browser".into(),
                    kind: MeetingAppKind::Browser,
                    bundle_ids: vec!["com.google.Chrome".into()],
                    detect: true,
                    capture: false,
                },
            ],
        }
    }

    #[test]
    fn save_reload_and_classify_use_external_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CATALOG_FILE_NAME);
        let registry = MeetingAppRegistry::new(path.clone());
        registry.save(sample_catalog()).unwrap();

        let fresh = MeetingAppRegistry::new(path);
        fresh.reload().unwrap();
        assert_eq!(fresh.classify("US.ZOOM.XOS"), AppClass::NativeMeeting);
        assert_eq!(
            fresh.classify("com.google.Chrome.helper.Renderer"),
            AppClass::Browser
        );
        assert_eq!(fresh.label("us.zoom.xos"), "Zoom");
        assert_eq!(fresh.manual_capture_bundle_ids(), vec!["us.zoom.xos"]);
    }

    #[test]
    fn save_replaces_an_existing_catalog_and_keeps_disk_in_sync() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CATALOG_FILE_NAME);
        let registry = MeetingAppRegistry::new(path.clone());
        registry.save(sample_catalog()).unwrap();

        let mut replacement = sample_catalog();
        replacement.applications[0].name = "Zoom Workplace".into();
        let saved = registry.save(replacement.clone()).unwrap();
        let on_disk: MeetingAppCatalog =
            toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.applications, replacement.applications);
        assert_eq!(on_disk, replacement);
    }

    #[test]
    fn concurrent_saves_leave_snapshot_equal_to_the_complete_disk_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CATALOG_FILE_NAME);
        let registry = MeetingAppRegistry::new(path.clone());
        let mut first = sample_catalog();
        first.applications[0].name = "First".into();
        let mut second = sample_catalog();
        second.applications[0].name = "Second".into();

        let first_registry = registry.clone();
        let first_save = std::thread::spawn(move || first_registry.save(first).unwrap());
        let second_registry = registry.clone();
        let second_save = std::thread::spawn(move || second_registry.save(second).unwrap());
        first_save.join().unwrap();
        second_save.join().unwrap();

        let on_disk: MeetingAppCatalog =
            toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.applications, on_disk.applications);
        assert_eq!(snapshot.version, on_disk.version);
    }

    #[test]
    fn template_install_never_overwrites_an_existing_user_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CATALOG_FILE_NAME);
        let template = directory.path().join("seed.toml");
        fs::write(&path, "user-owned").unwrap();
        fs::write(&template, "seed").unwrap();
        let registry = MeetingAppRegistry::new(path.clone());

        registry.install_template(&template).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "user-owned");
    }

    #[test]
    fn malformed_external_catalog_remains_visible_as_a_load_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(CATALOG_FILE_NAME);
        fs::write(&path, "not = [valid").unwrap();
        let registry = MeetingAppRegistry::new(path);

        let error = registry.reload().unwrap_err();
        let snapshot = registry.snapshot();
        assert!(error.contains("parse"));
        assert_eq!(snapshot.load_error.as_deref(), Some(error.as_str()));
        assert!(snapshot.applications.is_empty());
    }

    #[test]
    fn duplicate_bundle_ids_are_rejected_case_insensitively() {
        let mut catalog = sample_catalog();
        catalog.applications.push(MeetingAppEntry {
            name: "Duplicate".into(),
            kind: MeetingAppKind::Meeting,
            bundle_ids: vec!["US.ZOOM.XOS".into()],
            detect: true,
            capture: true,
        });
        assert!(validate_catalog(&catalog)
            .unwrap_err()
            .contains("appears more than once"));
    }

    #[test]
    fn browser_is_not_in_manual_capture_allowlist() {
        let directory = tempfile::tempdir().unwrap();
        let registry = MeetingAppRegistry::new(directory.path().join(CATALOG_FILE_NAME));
        registry.save(sample_catalog()).unwrap();
        assert!(!registry.capture_enabled("com.google.Chrome"));
        assert_eq!(registry.manual_capture_bundle_ids(), vec!["us.zoom.xos"]);
    }

    #[test]
    fn shipped_seed_is_valid_external_toml() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/meeting-apps.toml");
        let text = fs::read_to_string(path).unwrap();
        let catalog: MeetingAppCatalog = toml::from_str(&text).unwrap();
        validate_catalog(&catalog).unwrap();
        assert!(catalog.applications.iter().any(|entry| {
            entry.name == "腾讯会议" && entry.bundle_ids == ["com.tencent.meeting"]
        }));
        assert!(catalog
            .applications
            .iter()
            .any(|entry| entry.kind == MeetingAppKind::Browser));
    }
}
