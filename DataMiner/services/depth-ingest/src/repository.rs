use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use depth_lab::archive_contract::ingest::DepthIngestSnapshot;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const REPOSITORY_SCHEMA_VERSION: &str = "dm-depth-index-repository-1";

#[derive(Debug, Clone)]
pub struct VersionedSnapshot {
    pub revision: u64,
    pub snapshot: DepthIngestSnapshot,
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("depth index I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("depth index serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("depth index revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("depth index envelope is corrupt")]
    CorruptEnvelope,
    #[error("depth index revision overflow")]
    RevisionOverflow,
    #[error("depth index mutex is poisoned")]
    Poisoned,
}

/// 独立 depth index 的 CAS repository。实现必须持久保存完整状态机快照，禁止映射到
/// 生产 `artifact_objects`。revision 冲突必须拒绝，不能 last-writer-wins。
pub trait DepthIndexRepository: Send + Sync {
    fn load(&self) -> Result<Option<VersionedSnapshot>, RepositoryError>;

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        snapshot: &DepthIngestSnapshot,
    ) -> Result<u64, RepositoryError>;
}

#[derive(Clone, Default)]
pub struct InMemoryDepthIndexRepository {
    inner: Arc<Mutex<Option<VersionedSnapshot>>>,
}

impl DepthIndexRepository for InMemoryDepthIndexRepository {
    fn load(&self) -> Result<Option<VersionedSnapshot>, RepositoryError> {
        self.inner
            .lock()
            .map_err(|_| RepositoryError::Poisoned)
            .map(|guard| guard.clone())
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        snapshot: &DepthIngestSnapshot,
    ) -> Result<u64, RepositoryError> {
        let mut guard = self.inner.lock().map_err(|_| RepositoryError::Poisoned)?;
        let actual = guard.as_ref().map_or(0, |current| current.revision);
        if actual != expected_revision {
            return Err(RepositoryError::RevisionConflict {
                expected: expected_revision,
                actual,
            });
        }
        let revision = actual
            .checked_add(1)
            .ok_or(RepositoryError::RevisionOverflow)?;
        *guard = Some(VersionedSnapshot {
            revision,
            snapshot: snapshot.clone(),
        });
        Ok(revision)
    }
}

/// 单实例可部署的 append-only 文件 repository。每次提交写新 revision，再原子 rename；
/// 因而不会覆盖上一份可恢复状态。多实例生产部署应实现同一 trait 的托管数据库 CAS，
/// 不能让多个实例共享普通 Cloud Run 临时盘。
pub struct FileDepthIndexRepository {
    directory: PathBuf,
}

impl FileDepthIndexRepository {
    pub fn new(directory: impl Into<PathBuf>) -> Result<Self, RepositoryError> {
        let directory = directory.into();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    fn lock_file(&self) -> Result<File, RepositoryError> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory.join("depth-index.lock"))?)
    }

    fn load_unlocked(&self) -> Result<Option<VersionedSnapshot>, RepositoryError> {
        let Some((path, file_revision)) = latest_snapshot_file(&self.directory)? else {
            return Ok(None);
        };
        let bytes = fs::read(path)?;
        let envelope: SnapshotEnvelope = serde_json::from_slice(&bytes)?;
        if envelope.schema_version != REPOSITORY_SCHEMA_VERSION
            || envelope.revision != file_revision
            || envelope.snapshot_sha256 != snapshot_sha256(&envelope.snapshot)?
        {
            return Err(RepositoryError::CorruptEnvelope);
        }
        Ok(Some(VersionedSnapshot {
            revision: envelope.revision,
            snapshot: envelope.snapshot,
        }))
    }
}

impl DepthIndexRepository for FileDepthIndexRepository {
    fn load(&self) -> Result<Option<VersionedSnapshot>, RepositoryError> {
        let lock = self.lock_file()?;
        FileExt::lock_shared(&lock)?;
        let result = self.load_unlocked();
        FileExt::unlock(&lock)?;
        result
    }

    fn compare_and_swap(
        &self,
        expected_revision: u64,
        snapshot: &DepthIngestSnapshot,
    ) -> Result<u64, RepositoryError> {
        let lock = self.lock_file()?;
        FileExt::lock_exclusive(&lock)?;
        let actual = self
            .load_unlocked()?
            .as_ref()
            .map_or(0, |current| current.revision);
        if actual != expected_revision {
            FileExt::unlock(&lock)?;
            return Err(RepositoryError::RevisionConflict {
                expected: expected_revision,
                actual,
            });
        }
        let revision = actual
            .checked_add(1)
            .ok_or(RepositoryError::RevisionOverflow)?;
        let envelope = SnapshotEnvelope {
            schema_version: REPOSITORY_SCHEMA_VERSION.to_string(),
            revision,
            snapshot_sha256: snapshot_sha256(snapshot)?,
            snapshot: snapshot.clone(),
        };
        let bytes = serde_json::to_vec(&envelope)?;
        let temporary_path = self
            .directory
            .join(format!(".state-{revision:020}-{}.tmp", Uuid::new_v4()));
        let final_path = self.directory.join(snapshot_filename(revision));
        let mut temporary = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;
        temporary.write_all(&bytes)?;
        temporary.write_all(b"\n")?;
        temporary.sync_all()?;
        drop(temporary);
        fs::rename(&temporary_path, &final_path)?;
        sync_directory(&self.directory)?;
        FileExt::unlock(&lock)?;
        Ok(revision)
    }
}

#[derive(Serialize, Deserialize)]
struct SnapshotEnvelope {
    schema_version: String,
    revision: u64,
    snapshot_sha256: String,
    snapshot: DepthIngestSnapshot,
}

fn snapshot_sha256(snapshot: &DepthIngestSnapshot) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(snapshot)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn snapshot_filename(revision: u64) -> String {
    format!("state-{revision:020}.json")
}

fn latest_snapshot_file(directory: &Path) -> Result<Option<(PathBuf, u64)>, RepositoryError> {
    let mut latest: Option<(PathBuf, u64)> = None;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(raw_revision) = name
            .strip_prefix("state-")
            .and_then(|value| value.strip_suffix(".json"))
            .filter(|value| value.len() == 20 && value.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            continue;
        };
        let revision = raw_revision
            .parse::<u64>()
            .map_err(|_| RepositoryError::CorruptEnvelope)?;
        if latest
            .as_ref()
            .is_none_or(|(_, current_revision)| revision > *current_revision)
        {
            latest = Some((entry.path(), revision));
        }
    }
    Ok(latest)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), std::io::Error> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use depth_lab::archive_contract::ingest::DepthIngestService;

    use super::*;

    #[derive(Default)]
    struct EmptyStorage;

    impl depth_lab::archive_contract::ingest::DepthArchiveStorage for EmptyStorage {
        fn create_multipart(
            &mut self,
            _target: &depth_lab::archive_contract::ingest::StorageObjectTarget,
        ) -> Result<String, String> {
            unreachable!()
        }

        fn authorize_part(
            &mut self,
            _target: &depth_lab::archive_contract::ingest::StoragePartTarget,
        ) -> Result<depth_lab::archive_contract::ingest::PresignedGrant, String> {
            unreachable!()
        }

        fn complete_multipart(
            &mut self,
            _target: &depth_lab::archive_contract::ingest::StorageCompleteTarget,
        ) -> Result<(), String> {
            unreachable!()
        }

        fn abort_multipart(
            &mut self,
            _bucket: &str,
            _object_key: &str,
            _provider_upload_id: &str,
        ) -> Result<(), String> {
            unreachable!()
        }

        fn verify_object(
            &mut self,
            _bucket: &str,
            _object_key: &str,
        ) -> Result<depth_lab::archive_contract::ingest::VerifiedObject, String> {
            unreachable!()
        }

        fn authorize_manifest_put(
            &mut self,
            _target: &depth_lab::archive_contract::ingest::StorageManifestTarget,
        ) -> Result<depth_lab::archive_contract::ingest::PresignedGrant, String> {
            unreachable!()
        }

        fn read_manifest(
            &mut self,
            _bucket: &str,
            _object_key: &str,
        ) -> Result<depth_lab::archive_contract::ingest::VerifiedManifest, String> {
            unreachable!()
        }
    }

    #[test]
    fn file_repository_reopens_and_rejects_stale_revision() {
        let directory = tempfile::tempdir().unwrap();
        let repository = FileDepthIndexRepository::new(directory.path()).unwrap();
        let snapshot = DepthIngestService::new(EmptyStorage, 60)
            .unwrap()
            .snapshot();
        assert_eq!(repository.compare_and_swap(0, &snapshot).unwrap(), 1);

        let reopened = FileDepthIndexRepository::new(directory.path()).unwrap();
        let loaded = reopened.load().unwrap().unwrap();
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.snapshot, snapshot);
        assert!(matches!(
            reopened.compare_and_swap(0, &snapshot),
            Err(RepositoryError::RevisionConflict {
                expected: 0,
                actual: 1
            })
        ));
    }
}
