use depth_lab::archive_contract::ingest::{
    ArchiveScope, ArchiveStatus, AuthorizeManifestRequest, AuthorizePartRequest,
    AuthorizedManifest, AuthorizedPart, CompleteObjectRequest, CreateArchiveRequest,
    CreateMultipartRequest, DepthArchiveStorage, DepthIngestService, DepthPrincipal,
    FinalizeManifestRequest, IngestError, MultipartIntent,
};

use crate::repository::{DepthIndexRepository, RepositoryError};

#[derive(Debug)]
pub enum BoundaryError {
    Ingest(IngestError),
    Repository(RepositoryError),
    DurabilityPoisoned,
}

impl From<IngestError> for BoundaryError {
    fn from(value: IngestError) -> Self {
        Self::Ingest(value)
    }
}

impl From<RepositoryError> for BoundaryError {
    fn from(value: RepositoryError) -> Self {
        Self::Repository(value)
    }
}

/// 把 #535 状态机与独立 depth index 绑定。每次可能改变状态的调用，不论业务结果
/// 成功还是失败，只要快照发生变化就 CAS 持久化。持久化失败后实例立即 poisoned，
/// readiness 关闭并拒绝后续请求，避免内存事实继续领先 durable index。
pub struct DurableDepthIngest<S> {
    core: DepthIngestService<S>,
    repository: Box<dyn DepthIndexRepository>,
    revision: u64,
    poisoned: bool,
}

impl<S: DepthArchiveStorage> DurableDepthIngest<S> {
    pub fn open(
        storage: S,
        grant_ttl_seconds: u64,
        repository: Box<dyn DepthIndexRepository>,
    ) -> Result<Self, BoundaryError> {
        let (core, revision) = match repository.load()? {
            Some(versioned) => (
                DepthIngestService::from_snapshot(storage, grant_ttl_seconds, versioned.snapshot)?,
                versioned.revision,
            ),
            None => (DepthIngestService::new(storage, grant_ttl_seconds)?, 0),
        };
        Ok(Self {
            core,
            repository,
            revision,
            poisoned: false,
        })
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.poisoned
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn create_archive(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateArchiveRequest,
    ) -> Result<ArchiveScope, BoundaryError> {
        self.mutate(|core| core.create_archive(principal, now_epoch_seconds, request))
    }

    pub fn create_multipart(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateMultipartRequest,
    ) -> Result<MultipartIntent, BoundaryError> {
        self.mutate(|core| core.create_multipart(principal, now_epoch_seconds, request))
    }

    pub fn authorize_part(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizePartRequest,
    ) -> Result<AuthorizedPart, BoundaryError> {
        self.mutate(|core| core.authorize_part(principal, now_epoch_seconds, request))
    }

    pub fn complete_object(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CompleteObjectRequest,
    ) -> Result<ArchiveStatus, BoundaryError> {
        self.mutate(|core| core.complete_object(principal, now_epoch_seconds, request))
    }

    pub fn authorize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizeManifestRequest,
    ) -> Result<AuthorizedManifest, BoundaryError> {
        self.mutate(|core| core.authorize_manifest(principal, now_epoch_seconds, request))
    }

    pub fn finalize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: FinalizeManifestRequest,
    ) -> Result<ArchiveStatus, BoundaryError> {
        self.mutate(|core| core.finalize_manifest(principal, now_epoch_seconds, request))
    }

    pub fn status(
        &self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        scope: &ArchiveScope,
    ) -> Result<ArchiveStatus, BoundaryError> {
        self.ensure_ready()?;
        self.core
            .status(principal, now_epoch_seconds, scope)
            .map_err(BoundaryError::Ingest)
    }

    pub fn abort_expired_multipart(
        &mut self,
        now_epoch_seconds: u64,
        grace_seconds: u64,
    ) -> Result<Vec<String>, BoundaryError> {
        self.mutate(|core| core.abort_expired_multipart(now_epoch_seconds, grace_seconds))
    }

    fn ensure_ready(&self) -> Result<(), BoundaryError> {
        if self.poisoned {
            Err(BoundaryError::DurabilityPoisoned)
        } else {
            Ok(())
        }
    }

    fn mutate<T>(
        &mut self,
        operation: impl FnOnce(&mut DepthIngestService<S>) -> Result<T, IngestError>,
    ) -> Result<T, BoundaryError> {
        self.ensure_ready()?;
        let before = self.core.snapshot();
        let result = operation(&mut self.core);
        let after = self.core.snapshot();
        if after != before {
            match self.repository.compare_and_swap(self.revision, &after) {
                Ok(revision) => self.revision = revision,
                Err(error) => {
                    self.poisoned = true;
                    return Err(BoundaryError::Repository(error));
                }
            }
        }
        result.map_err(BoundaryError::Ingest)
    }
}
