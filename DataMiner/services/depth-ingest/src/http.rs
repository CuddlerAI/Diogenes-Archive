use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use depth_lab::archive_contract::ingest::{
    ArchiveScope, AuthorizeManifestRequest, AuthorizePartRequest, CompleteObjectRequest,
    CreateArchiveRequest, CreateMultipartRequest, DEPTH_INGEST_AUDIENCE, DepthArchiveStorage,
    FinalizeManifestRequest, IngestError,
};
use serde::Serialize;
use tokio::task;
use tower_http::trace::TraceLayer;

use crate::auth::{AuthError, DEPTH_INGEST_WORKER_AUDIENCE, DepthTokenVerifier};
use crate::boundary::{BoundaryError, DurableDepthIngest};

const MAX_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;

pub trait Clock: Send + Sync {
    fn now_epoch_seconds(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
    }
}

pub struct HttpState<S> {
    boundary: Arc<Mutex<DurableDepthIngest<S>>>,
    auth: Arc<DepthTokenVerifier>,
    clock: Arc<dyn Clock>,
    cleanup_grace_seconds: u64,
}

impl<S> Clone for HttpState<S> {
    fn clone(&self) -> Self {
        Self {
            boundary: Arc::clone(&self.boundary),
            auth: Arc::clone(&self.auth),
            clock: Arc::clone(&self.clock),
            cleanup_grace_seconds: self.cleanup_grace_seconds,
        }
    }
}

impl<S> HttpState<S> {
    pub fn new(
        boundary: DurableDepthIngest<S>,
        auth: DepthTokenVerifier,
        clock: Arc<dyn Clock>,
        cleanup_grace_seconds: u64,
    ) -> Result<Self, ApiError> {
        if cleanup_grace_seconds == 0 {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid_cleanup_grace",
            ));
        }
        Ok(Self {
            boundary: Arc::new(Mutex::new(boundary)),
            auth: Arc::new(auth),
            clock,
            cleanup_grace_seconds,
        })
    }
}

pub fn build_router<S>(state: HttpState<S>) -> Router
where
    S: DepthArchiveStorage + Send + 'static,
{
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready::<S>))
        .route("/depth-ingest/v1/archives", post(create_archive::<S>))
        .route(
            "/depth-ingest/v1/archives/{archive_id}/multipart",
            post(create_multipart::<S>),
        )
        .route(
            "/depth-ingest/v1/archives/{archive_id}/multipart/{upload_id}/parts/authorize",
            post(authorize_part::<S>),
        )
        .route(
            "/depth-ingest/v1/archives/{archive_id}/multipart/{upload_id}/complete",
            post(complete_object::<S>),
        )
        .route(
            "/depth-ingest/v1/archives/{archive_id}/manifest/authorize",
            post(authorize_manifest::<S>),
        )
        .route(
            "/depth-ingest/v1/archives/{archive_id}/manifest/finalize",
            post(finalize_manifest::<S>),
        )
        .route(
            "/depth-ingest/v1/archives/{archive_id}/status",
            post(status::<S>),
        )
        .route(
            "/depth-ingest/internal/v1/cleanup/orphan-multipart",
            post(cleanup::<S>),
        )
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn ready<S>(State(state): State<HttpState<S>>) -> Result<Json<HealthResponse>, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    let is_ready = state
        .boundary
        .lock()
        .map_err(|_| ApiError::unavailable("depth_index_mutex_poisoned"))?
        .is_ready();
    if !is_ready {
        return Err(ApiError::unavailable("depth_index_not_ready"));
    }
    Ok(Json(HealthResponse { status: "ready" }))
}

async fn create_archive<S>(
    State(state): State<HttpState<S>>,
    headers: HeaderMap,
    Json(request): Json<CreateArchiveRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    let (principal, now) = authenticate_client(&state, &headers)?;
    let scope = run_boundary(state, move |boundary| {
        boundary.create_archive(&principal, now, request)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(scope)))
}

async fn create_multipart<S>(
    State(state): State<HttpState<S>>,
    Path(archive_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CreateMultipartRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &request.scope)?;
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.create_multipart(&principal, now, request)
    })
    .await?;
    Ok(Json(response))
}

async fn authorize_part<S>(
    State(state): State<HttpState<S>>,
    Path((archive_id, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<AuthorizePartRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &request.scope)?;
    if request.upload_id != upload_id {
        return Err(ApiError::bad_request("upload_path_binding_mismatch"));
    }
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.authorize_part(&principal, now, request)
    })
    .await?;
    Ok(Json(response))
}

async fn complete_object<S>(
    State(state): State<HttpState<S>>,
    Path((archive_id, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<CompleteObjectRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &request.scope)?;
    if request.upload_id != upload_id {
        return Err(ApiError::bad_request("upload_path_binding_mismatch"));
    }
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.complete_object(&principal, now, request)
    })
    .await?;
    Ok(Json(response))
}

async fn authorize_manifest<S>(
    State(state): State<HttpState<S>>,
    Path(archive_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AuthorizeManifestRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &request.scope)?;
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.authorize_manifest(&principal, now, request)
    })
    .await?;
    Ok(Json(response))
}

async fn finalize_manifest<S>(
    State(state): State<HttpState<S>>,
    Path(archive_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<FinalizeManifestRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &request.scope)?;
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.finalize_manifest(&principal, now, request)
    })
    .await?;
    Ok(Json(response))
}

async fn status<S>(
    State(state): State<HttpState<S>>,
    Path(archive_id): Path<String>,
    headers: HeaderMap,
    Json(scope): Json<ArchiveScope>,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    bind_archive_path(&archive_id, &scope)?;
    let (principal, now) = authenticate_client(&state, &headers)?;
    let response = run_boundary(state, move |boundary| {
        boundary.status(&principal, now, &scope)
    })
    .await?;
    Ok(Json(response))
}

async fn cleanup<S>(
    State(state): State<HttpState<S>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
{
    let now = state.clock.now_epoch_seconds();
    state
        .auth
        .authenticate(&headers, DEPTH_INGEST_WORKER_AUDIENCE, now)
        .map_err(ApiError::from_auth)?;
    let grace = state.cleanup_grace_seconds;
    let aborted_upload_ids = run_boundary(state, move |boundary| {
        boundary.abort_expired_multipart(now, grace)
    })
    .await?;
    Ok(Json(CleanupResponse { aborted_upload_ids }))
}

fn authenticate_client<S>(
    state: &HttpState<S>,
    headers: &HeaderMap,
) -> Result<(depth_lab::archive_contract::ingest::DepthPrincipal, u64), ApiError> {
    let now = state.clock.now_epoch_seconds();
    let principal = state
        .auth
        .authenticate(headers, DEPTH_INGEST_AUDIENCE, now)
        .map_err(ApiError::from_auth)?
        .into_depth_principal()
        .map_err(ApiError::from_auth)?;
    Ok((principal, now))
}

fn bind_archive_path(archive_id: &str, scope: &ArchiveScope) -> Result<(), ApiError> {
    if scope.archive_id != archive_id {
        return Err(ApiError::bad_request("archive_path_binding_mismatch"));
    }
    Ok(())
}

async fn run_boundary<S, T>(
    state: HttpState<S>,
    operation: impl FnOnce(&mut DurableDepthIngest<S>) -> Result<T, BoundaryError> + Send + 'static,
) -> Result<T, ApiError>
where
    S: DepthArchiveStorage + Send + 'static,
    T: Send + 'static,
{
    task::spawn_blocking(move || {
        let mut boundary = state
            .boundary
            .lock()
            .map_err(|_| ApiError::unavailable("depth_index_mutex_poisoned"))?;
        operation(&mut boundary).map_err(ApiError::from_boundary)
    })
    .await
    .map_err(|_| ApiError::unavailable("depth_ingest_worker_failed"))?
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct CleanupResponse {
    aborted_upload_ids: Vec<String>,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    fn unavailable(code: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code)
    }

    fn from_auth(error: AuthError) -> Self {
        match error {
            AuthError::WrongAudience => Self::new(StatusCode::FORBIDDEN, "wrong_depth_audience"),
            AuthError::WeakServerSecret => Self::unavailable("depth_auth_not_configured"),
            AuthError::MissingBearer | AuthError::InvalidBearer => {
                Self::new(StatusCode::UNAUTHORIZED, "invalid_depth_bearer")
            }
            AuthError::Expired => Self::new(StatusCode::UNAUTHORIZED, "depth_bearer_expired"),
            AuthError::InvalidSubject => {
                Self::new(StatusCode::UNAUTHORIZED, "invalid_depth_subject")
            }
        }
    }

    fn from_boundary(error: BoundaryError) -> Self {
        match error {
            BoundaryError::Repository(_) | BoundaryError::DurabilityPoisoned => {
                Self::unavailable("depth_index_unavailable")
            }
            BoundaryError::Ingest(error) => Self::from_ingest(error),
        }
    }

    fn from_ingest(error: IngestError) -> Self {
        use IngestError as E;
        match error {
            E::WrongCredentialAudience
            | E::PrincipalMismatch
            | E::ScopeMismatch
            | E::WrongBucket
            | E::WrongObjectKey
            | E::UploadBindingMismatch => Self::new(StatusCode::FORBIDDEN, "depth_scope_forbidden"),
            E::PrincipalExpired => Self::new(StatusCode::UNAUTHORIZED, "depth_bearer_expired"),
            E::UnknownArchive | E::UnknownObject | E::UnknownUpload => {
                Self::new(StatusCode::NOT_FOUND, "depth_resource_not_found")
            }
            E::ArchiveCollision
            | E::ArchiveFinalized
            | E::UploadAborted
            | E::PartAuthorizationReplay
            | E::CompletionReplayMismatch
            | E::ManifestNotReady
            | E::ManifestNotAuthorized => Self::new(StatusCode::CONFLICT, "depth_state_conflict"),
            E::Storage(_) => Self::new(StatusCode::BAD_GATEWAY, "depth_r2_unavailable"),
            E::InvalidStateSnapshot => Self::unavailable("depth_index_invalid"),
            E::InvalidGrantTtl | E::InvalidCleanupGrace => {
                Self::unavailable("depth_service_misconfigured")
            }
            E::InvalidIdentity
            | E::InvalidManifestHash
            | E::EmptyManifest
            | E::Target(_)
            | E::Contract(_)
            | E::ManifestBindingMismatch
            | E::ManifestRunMismatch(_)
            | E::InvalidPartSize
            | E::TooManyParts
            | E::InvalidPartNumber
            | E::InvalidPartHash
            | E::InvalidEtag
            | E::PartNotAuthorized(_)
            | E::PartAuthorizationExpired(_)
            | E::DuplicatePart(_)
            | E::MissingParts
            | E::PartMismatch(_)
            | E::ObjectSizeMismatch
            | E::ObjectSha256Mismatch
            | E::ManifestAuthorizationExpired
            | E::ManifestSizeMismatch
            | E::ManifestSha256Mismatch
            | E::ManifestContentMismatch => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "depth_contract_rejected")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: ErrorCode { code: self.code },
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorCode,
}

#[derive(Serialize)]
struct ErrorCode {
    code: &'static str,
}
