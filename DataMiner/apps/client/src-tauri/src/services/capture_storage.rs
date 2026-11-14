use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf, Prefix};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::platform::windows::storage::volume_space;

const CONFIG_FILE_NAME: &str = "capture-storage-v1.json";
const CONFIG_SCHEMA: &str = "dm-capture-storage-config-1";
const STATUS_SCHEMA: &str = "dm-capture-storage-status-1";
const ARTIFACT_ROOT_NAME: &str = "DiogenesDataMiner";
pub(crate) const SESSION_ROOT_NAME: &str = "sessions";
const LEGACY_SESSION_ROOT_NAME: &str = "non-trainable-sessions";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const DRIVE_FIXED_TYPE: u32 = 3;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureStorageConfig {
    schema_version: String,
    artifact_root: String,
}

#[derive(Clone, Debug)]
pub struct CaptureStorageLayout {
    pub artifact_root: PathBuf,
    pub output_root: PathBuf,
    pub custom: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStorageStatus {
    pub schema_version: &'static str,
    pub mode: &'static str,
    pub active_location: String,
    pub configured_location: Option<String>,
    pub restart_required: bool,
    pub can_change: bool,
    pub free_bytes: Option<u64>,
}

pub struct CaptureStorageManager {
    control_root: PathBuf,
    active: CaptureStorageLayout,
    configured_root: PathBuf,
}

impl CaptureStorageManager {
    pub fn load(control_root: &Path) -> anyhow::Result<Self> {
        let config_path = control_root.join(CONFIG_FILE_NAME);
        let (artifact_root, custom) = if config_path.exists() {
            let bytes = fs::read(&config_path)
                .context("failed to read the capture storage configuration")?;
            let config: CaptureStorageConfig = serde_json::from_slice(&bytes)
                .context("capture storage configuration is invalid")?;
            ensure!(
                config.schema_version == CONFIG_SCHEMA,
                "unsupported capture storage configuration schema"
            );
            let root = PathBuf::from(config.artifact_root);
            validate_active_root(&root)?;
            (
                fs::canonicalize(root).context("configured capture storage is unavailable")?,
                true,
            )
        } else {
            (control_root.to_path_buf(), false)
        };
        let output_root = prepare_session_root(&artifact_root)?;
        let active = CaptureStorageLayout {
            output_root,
            artifact_root: artifact_root.clone(),
            custom,
        };
        Ok(Self {
            control_root: control_root.to_path_buf(),
            configured_root: artifact_root,
            active,
        })
    }

    pub fn layout(&self) -> &CaptureStorageLayout {
        &self.active
    }

    pub fn restart_required(&self) -> bool {
        self.configured_root != self.active.artifact_root
    }

    pub fn configure_parent(&mut self, selected_parent: &Path) -> anyhow::Result<()> {
        let parent = validate_selected_parent(selected_parent)?;
        let artifact_root = parent.join(ARTIFACT_ROOT_NAME);
        fs::create_dir_all(&artifact_root)
            .context("failed to create the dedicated capture storage directory")?;
        let artifact_root = fs::canonicalize(&artifact_root)
            .context("failed to resolve the dedicated capture storage directory")?;
        validate_active_root(&artifact_root)?;
        probe_atomic_storage(&artifact_root)?;
        write_config_atomic(&self.control_root, &artifact_root)?;
        self.configured_root = artifact_root;
        Ok(())
    }

    pub fn status(&self, can_change: bool) -> CaptureStorageStatus {
        let restart_required = self.restart_required();
        CaptureStorageStatus {
            schema_version: STATUS_SCHEMA,
            mode: if self.active.custom {
                "custom"
            } else {
                "app_local"
            },
            active_location: display_location(&self.active.artifact_root, self.active.custom),
            configured_location: restart_required
                .then(|| display_location(&self.configured_root, true)),
            restart_required,
            can_change,
            free_bytes: volume_space(&self.active.artifact_root)
                .ok()
                .map(|space| space.available_bytes),
        }
    }
}

/// Promote the pre-0.7.15 RAW root to the neutral name without copying bytes.
///
/// The rename stays below one artifact root, so existing journal-relative paths,
/// upload identities and hashes remain unchanged. A split layout is never merged
/// automatically: two non-empty roots could contain colliding session IDs, and
/// guessing which one wins would turn a cosmetic migration into data loss.
fn prepare_session_root(artifact_root: &Path) -> anyhow::Result<PathBuf> {
    let current = artifact_root.join(SESSION_ROOT_NAME);
    let legacy = artifact_root.join(LEGACY_SESSION_ROOT_NAME);
    let current_exists = validated_session_root_exists(&current, "neutral")?;
    let legacy_exists = validated_session_root_exists(&legacy, "legacy")?;

    match (current_exists, legacy_exists) {
        (false, true) => {
            fs::rename(&legacy, &current)
                .context("failed to atomically promote the legacy session root")?;
            ensure_no_reparse_ancestors(&current)?;
        }
        (true, true) => {
            let current_empty = fs::read_dir(&current)?.next().is_none();
            let legacy_empty = fs::read_dir(&legacy)?.next().is_none();
            if legacy_empty {
                fs::remove_dir(&legacy)
                    .context("failed to remove the empty legacy session root")?;
            } else if current_empty {
                fs::remove_dir(&current)
                    .context("failed to remove the empty neutral session root")?;
                fs::rename(&legacy, &current)
                    .context("failed to atomically promote the legacy session root")?;
                ensure_no_reparse_ancestors(&current)?;
            } else {
                anyhow::bail!(
                    "both sessions and non-trainable-sessions contain data; refusing to merge session roots automatically"
                );
            }
        }
        _ => {}
    }
    Ok(current)
}

fn validated_session_root_exists(path: &Path, label: &str) -> anyhow::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("failed to inspect the session root"),
    };
    ensure!(metadata.is_dir(), "{label} session root is not a directory");
    ensure_no_reparse_ancestors(path)?;
    Ok(true)
}

fn validate_selected_parent(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "capture storage selection must be absolute"
    );
    ensure!(
        path.is_dir(),
        "capture storage selection is not a directory"
    );
    ensure_no_reparse_ancestors(path)?;
    let canonical = fs::canonicalize(path).context("selected capture storage is unavailable")?;
    ensure_fixed_local_drive(&canonical)?;
    ensure_no_reparse_ancestors(&canonical)?;
    Ok(canonical)
}

fn validate_active_root(path: &Path) -> anyhow::Result<()> {
    ensure!(
        path.is_absolute(),
        "configured capture storage must be absolute"
    );
    ensure!(path.is_dir(), "configured capture storage is unavailable");
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some(ARTIFACT_ROOT_NAME),
        "configured capture storage is not a dedicated DataMiner directory"
    );
    ensure_fixed_local_drive(path)?;
    ensure_no_reparse_ancestors(path)
}

fn ensure_fixed_local_drive(path: &Path) -> anyhow::Result<()> {
    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => anyhow::bail!("capture storage must use a fixed local drive"),
        },
        _ => anyhow::bail!("capture storage must use a fixed local drive"),
    };
    let root = format!("{}:\\", char::from(drive));
    let wide = root
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
    ensure!(
        kind == DRIVE_FIXED_TYPE,
        "capture storage must use a fixed local drive"
    );
    Ok(())
}

fn ensure_no_reparse_ancestors(path: &Path) -> anyhow::Result<()> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .context("failed to inspect the capture storage directory chain")?;
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "capture storage directory chain contains a reparse point"
        );
    }
    Ok(())
}

fn probe_atomic_storage(root: &Path) -> anyhow::Result<()> {
    let id = Uuid::new_v4();
    let staging = root.join(format!(".capture-storage-probe-{id}.tmp"));
    let published = root.join(format!(".capture-storage-probe-{id}.ready"));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .context("capture storage write probe failed")?;
        file.write_all(b"dm-capture-storage-probe-1\n")?;
        file.sync_all()
            .context("capture storage flush probe failed")?;
        drop(file);
        fs::rename(&staging, &published).context("capture storage rename probe failed")?;
        fs::remove_file(&published).context("capture storage cleanup probe failed")?;
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_file(&staging);
    }
    if published.exists() {
        let _ = fs::remove_file(&published);
    }
    result
}

fn write_config_atomic(control_root: &Path, artifact_root: &Path) -> anyhow::Result<()> {
    let path_text = artifact_root
        .to_str()
        .context("capture storage path is not valid Unicode")?;
    let bytes = serde_json::to_vec_pretty(&CaptureStorageConfig {
        schema_version: CONFIG_SCHEMA.into(),
        artifact_root: path_text.into(),
    })?;
    let destination = control_root.join(CONFIG_FILE_NAME);
    let staging = control_root.join(format!(".{CONFIG_FILE_NAME}.{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .context("failed to create capture storage configuration staging file")?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    let source = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("failed to atomically publish capture storage configuration");
    if result.is_err() && staging.exists() {
        let _ = fs::remove_file(&staging);
    }
    result
}

fn display_location(path: &Path, custom: bool) -> String {
    if !custom {
        return "系统盘 · 应用默认位置".into();
    }
    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => char::from(letter),
            _ => '?',
        },
        _ => '?',
    };
    format!("{drive}: · {ARTIFACT_ROOT_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_uses_the_neutral_app_local_layout() {
        let control = tempfile::tempdir().unwrap();
        let manager = CaptureStorageManager::load(control.path()).unwrap();
        assert_eq!(manager.layout().artifact_root, control.path());
        assert_eq!(
            manager.layout().output_root,
            control.path().join(SESSION_ROOT_NAME)
        );
        let status = manager.status(true);
        assert_eq!(status.mode, "app_local");
        assert!(!status.restart_required);
        assert!(status.configured_location.is_none());
    }

    #[test]
    fn custom_selection_is_atomic_and_only_applies_after_reload() {
        let control = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        let mut manager = CaptureStorageManager::load(control.path()).unwrap();
        manager.configure_parent(selected.path()).unwrap();

        let pending = manager.status(true);
        assert!(pending.restart_required);
        assert_eq!(pending.mode, "app_local");
        assert_eq!(
            pending.configured_location,
            Some(display_location(
                &fs::canonicalize(selected.path().join(ARTIFACT_ROOT_NAME)).unwrap(),
                true,
            ))
        );
        assert!(
            !pending
                .active_location
                .contains(&control.path().to_string_lossy()[..])
        );

        let reloaded = CaptureStorageManager::load(control.path()).unwrap();
        assert!(reloaded.layout().custom);
        assert_eq!(
            reloaded.layout().artifact_root,
            fs::canonicalize(selected.path().join(ARTIFACT_ROOT_NAME)).unwrap()
        );
        assert!(!reloaded.status(true).restart_required);
    }

    #[test]
    fn a_second_selection_atomically_replaces_the_pending_configuration() {
        let control = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut manager = CaptureStorageManager::load(control.path()).unwrap();
        manager.configure_parent(first.path()).unwrap();
        manager.configure_parent(second.path()).unwrap();

        let reloaded = CaptureStorageManager::load(control.path()).unwrap();
        assert_eq!(
            reloaded.layout().artifact_root,
            fs::canonicalize(second.path().join(ARTIFACT_ROOT_NAME)).unwrap()
        );
    }

    #[test]
    fn manually_edited_relative_or_non_dedicated_roots_fail_closed() {
        let control = tempfile::tempdir().unwrap();
        fs::write(
            control.path().join(CONFIG_FILE_NAME),
            br#"{"schemaVersion":"dm-capture-storage-config-1","artifactRoot":"relative"}"#,
        )
        .unwrap();
        assert!(CaptureStorageManager::load(control.path()).is_err());

        let selected = tempfile::tempdir().unwrap();
        fs::write(
            control.path().join(CONFIG_FILE_NAME),
            serde_json::to_vec(&CaptureStorageConfig {
                schema_version: CONFIG_SCHEMA.into(),
                artifact_root: selected.path().to_string_lossy().into_owned(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(CaptureStorageManager::load(control.path()).is_err());
    }

    #[test]
    fn legacy_session_root_is_atomically_promoted() {
        let control = tempfile::tempdir().unwrap();
        let legacy = control.path().join(LEGACY_SESSION_ROOT_NAME);
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("dev-1-0000.raw"), b"preserved").unwrap();

        let manager = CaptureStorageManager::load(control.path()).unwrap();
        assert_eq!(
            fs::read(manager.layout().output_root.join("dev-1-0000.raw")).unwrap(),
            b"preserved"
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn split_non_empty_session_roots_fail_closed() {
        let control = tempfile::tempdir().unwrap();
        let legacy = control.path().join(LEGACY_SESSION_ROOT_NAME);
        let current = control.path().join(SESSION_ROOT_NAME);
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(&current).unwrap();
        fs::write(legacy.join("legacy.raw"), b"legacy").unwrap();
        fs::write(current.join("current.raw"), b"current").unwrap();

        let error = CaptureStorageManager::load(control.path())
            .err()
            .expect("split roots must be rejected");
        assert!(error.to_string().contains("refusing to merge"));
        assert!(legacy.join("legacy.raw").is_file());
        assert!(current.join("current.raw").is_file());
    }

    #[test]
    fn empty_legacy_root_recreated_after_upgrade_is_removed() {
        let control = tempfile::tempdir().unwrap();
        let legacy = control.path().join(LEGACY_SESSION_ROOT_NAME);
        let current = control.path().join(SESSION_ROOT_NAME);
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(&current).unwrap();
        fs::write(current.join("session-1.raw"), b"current").unwrap();

        let manager = CaptureStorageManager::load(control.path()).unwrap();
        assert_eq!(manager.layout().output_root, current);
        assert!(!legacy.exists());
    }

    #[test]
    fn empty_neutral_root_does_not_block_legacy_promotion() {
        let control = tempfile::tempdir().unwrap();
        let legacy = control.path().join(LEGACY_SESSION_ROOT_NAME);
        let current = control.path().join(SESSION_ROOT_NAME);
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(&current).unwrap();
        fs::write(legacy.join("dev-1.raw"), b"preserved").unwrap();

        let manager = CaptureStorageManager::load(control.path()).unwrap();
        assert_eq!(
            fs::read(manager.layout().output_root.join("dev-1.raw")).unwrap(),
            b"preserved"
        );
        assert!(!legacy.exists());
    }
}
