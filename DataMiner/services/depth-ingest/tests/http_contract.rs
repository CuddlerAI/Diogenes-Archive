use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use dataminer_depth_ingest::auth::{
    DEPTH_INGEST_WORKER_AUDIENCE, DepthTokenClaims, DepthTokenVerifier,
};
use dataminer_depth_ingest::boundary::DurableDepthIngest;
use dataminer_depth_ingest::http::{Clock, HttpState, build_router};
use dataminer_depth_ingest::repository::InMemoryDepthIndexRepository;
use depth_lab::archive_contract::ingest::{
    DepthArchiveStorage, PresignedGrant, StorageCompleteTarget, StorageManifestTarget,
    StorageObjectTarget, StoragePartTarget, VerifiedManifest, VerifiedObject,
};
use depth_lab::archive_contract::{
    ARCHIVE_BUCKET, ARCHIVE_RETENTION_DAYS, ARCHIVE_SCHEMA_VERSION, ArchiveLane, ArchiveManifest,
    ArchiveObject, ArchiveObjectKind, ArchivePriority, manifest_object_key, object_key,
};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const SECRET: &[u8] = b"depth-ingest-http-contract-test-secret";
const CLIENT_AUDIENCE: &str = "dataminer-depth-ingest";
const PRINCIPAL: &str = "depth-device-1";
const SESSION: &str = "depth-session-1";
const RUN: &str = "depth-run-1";

#[derive(Default)]
struct FakeState {
    next_upload_id: u64,
    objects: BTreeMap<String, Vec<u8>>,
    aborted_provider_ids: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeStorage {
    state: Arc<Mutex<FakeState>>,
}

impl FakeStorage {
    fn put(&self, key: String, bytes: Vec<u8>) {
        self.state.lock().unwrap().objects.insert(key, bytes);
    }
}

impl DepthArchiveStorage for FakeStorage {
    fn create_multipart(&mut self, _target: &StorageObjectTarget) -> Result<String, String> {
        let mut state = self.state.lock().unwrap();
        state.next_upload_id += 1;
        Ok(format!("fake-provider-{}", state.next_upload_id))
    }

    fn authorize_part(&mut self, target: &StoragePartTarget) -> Result<PresignedGrant, String> {
        Ok(PresignedGrant {
            url: format!("https://depth.invalid/{}", target.part_number),
            signed_headers: BTreeMap::from([
                ("content-length".to_string(), target.byte_size.to_string()),
                ("x-amz-content-sha256".to_string(), target.sha256.clone()),
            ]),
        })
    }

    fn complete_multipart(&mut self, _target: &StorageCompleteTarget) -> Result<(), String> {
        Ok(())
    }

    fn abort_multipart(
        &mut self,
        _bucket: &str,
        _object_key: &str,
        provider_upload_id: &str,
    ) -> Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .aborted_provider_ids
            .push(provider_upload_id.to_string());
        Ok(())
    }

    fn verify_object(&mut self, _bucket: &str, object_key: &str) -> Result<VerifiedObject, String> {
        let state = self.state.lock().unwrap();
        let bytes = state
            .objects
            .get(object_key)
            .ok_or_else(|| "missing fake object".to_string())?;
        Ok(VerifiedObject {
            size_bytes: bytes.len() as u64,
            sha256: contract_sha256(bytes),
        })
    }

    fn authorize_manifest_put(
        &mut self,
        target: &StorageManifestTarget,
    ) -> Result<PresignedGrant, String> {
        if !target.fail_if_exists {
            return Err("manifest overwrite was not fenced".to_string());
        }
        Ok(PresignedGrant {
            url: "https://depth.invalid/manifest".to_string(),
            signed_headers: BTreeMap::from([
                ("content-length".to_string(), target.size_bytes.to_string()),
                ("x-amz-content-sha256".to_string(), target.sha256.clone()),
                ("if-none-match".to_string(), "*".to_string()),
            ]),
        })
    }

    fn read_manifest(
        &mut self,
        _bucket: &str,
        object_key: &str,
    ) -> Result<VerifiedManifest, String> {
        let state = self.state.lock().unwrap();
        let bytes = state
            .objects
            .get(object_key)
            .ok_or_else(|| "missing fake manifest".to_string())?;
        Ok(VerifiedManifest {
            size_bytes: bytes.len() as u64,
            sha256: contract_sha256(bytes),
            manifest: serde_json::from_slice(bytes).map_err(|_| "invalid manifest".to_string())?,
        })
    }
}

struct ManualClock(AtomicU64);

impl ManualClock {
    fn new(now: u64) -> Self {
        Self(AtomicU64::new(now))
    }
}

impl Clock for ManualClock {
    fn now_epoch_seconds(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn full_http_contract_is_manifest_last_and_durable() {
    let clock = Arc::new(ManualClock::new(100));
    let storage = FakeStorage::default();
    let repository = InMemoryDepthIndexRepository::default();
    let boundary =
        DurableDepthIngest::open(storage.clone(), 60, Box::new(repository.clone())).unwrap();
    let state = HttpState::new(
        boundary,
        DepthTokenVerifier::new(SECRET).unwrap(),
        clock,
        10,
    )
    .unwrap();
    let app = build_router(state);
    let manifest = manifest();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let create = create_request(&manifest, &manifest_bytes);

    let (status, body) = call(
        &app,
        Method::POST,
        "/depth-ingest/v1/archives",
        &token("dataminer-production-uploader", 10_000),
        create.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "wrong_depth_audience");

    let (status, scope) = call(
        &app,
        Method::POST,
        "/depth-ingest/v1/archives",
        &token(CLIENT_AUDIENCE, 10_000),
        create,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let archive_id = scope["archive_id"].as_str().unwrap().to_string();

    let first_key = object_key(
        ArchiveLane::Lane30,
        SESSION,
        &manifest.objects[0].relative_key,
    )
    .unwrap();
    let (status, body) = call(
        &app,
        Method::POST,
        "/depth-ingest/v1/archives/wrong-archive/multipart",
        &token(CLIENT_AUDIENCE, 10_000),
        json!({
            "scope": scope,
            "bucket": ARCHIVE_BUCKET,
            "object_key": first_key,
            "part_size_bytes": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "archive_path_binding_mismatch");
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/multipart"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({
            "scope": scope,
            "bucket": "diogenes-model-training",
            "object_key": first_key,
            "part_size_bytes": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "depth_scope_forbidden");
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/manifest/authorize"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({
            "scope": scope,
            "bucket": ARCHIVE_BUCKET,
            "object_key": manifest_object_key(ArchiveLane::Lane30, SESSION).unwrap()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "depth_state_conflict");

    for object in &manifest.objects {
        let full_key = object_key(ArchiveLane::Lane30, SESSION, &object.relative_key).unwrap();
        let (status, intent) = call(
            &app,
            Method::POST,
            &format!("/depth-ingest/v1/archives/{archive_id}/multipart"),
            &token(CLIENT_AUDIENCE, 10_000),
            json!({
                "scope": scope,
                "bucket": ARCHIVE_BUCKET,
                "object_key": full_key,
                "part_size_bytes": 1
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let upload_id = intent["upload_id"].as_str().unwrap();
        let (status, authorized) = call(
            &app,
            Method::POST,
            &format!(
                "/depth-ingest/v1/archives/{archive_id}/multipart/{upload_id}/parts/authorize"
            ),
            &token(CLIENT_AUDIENCE, 10_000),
            json!({
                "scope": scope,
                "upload_id": upload_id,
                "bucket": ARCHIVE_BUCKET,
                "object_key": full_key,
                "part_number": 1,
                "byte_size": 1,
                "sha256": object.sha256
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(authorized["expected_object_size_bytes"], 1);
        storage.put(full_key.clone(), b"x".to_vec());
        let (status, _) = call(
            &app,
            Method::POST,
            &format!("/depth-ingest/v1/archives/{archive_id}/multipart/{upload_id}/complete"),
            &token(CLIENT_AUDIENCE, 10_000),
            json!({
                "scope": scope,
                "upload_id": upload_id,
                "bucket": ARCHIVE_BUCKET,
                "object_key": full_key,
                "parts": [{
                    "part_number": 1,
                    "byte_size": 1,
                    "sha256": object.sha256,
                    "etag": "fake-etag"
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    assert_ne!(first_key, "");
    let manifest_key = manifest_object_key(ArchiveLane::Lane30, SESSION).unwrap();
    let (status, authorized) = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/manifest/authorize"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({ "scope": scope, "bucket": ARCHIVE_BUCKET, "object_key": manifest_key }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(authorized["grant"]["signed_headers"]["if-none-match"], "*");
    storage.put(manifest_key.clone(), manifest_bytes);
    let (status, finalized) = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/manifest/finalize"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({ "scope": scope, "bucket": ARCHIVE_BUCKET, "object_key": manifest_key }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(finalized["phase"], "finalized");

    let restored = DurableDepthIngest::open(storage, 60, Box::new(repository)).unwrap();
    assert!(restored.revision() > 0);
    let restored_app = build_router(
        HttpState::new(
            restored,
            DepthTokenVerifier::new(SECRET).unwrap(),
            Arc::new(ManualClock::new(101)),
            10,
        )
        .unwrap(),
    );
    let (status, restored_status) = call(
        &restored_app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/status"),
        &token(CLIENT_AUDIENCE, 10_000),
        scope,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored_status["phase"], "finalized");
}

#[tokio::test]
async fn cleanup_requires_worker_audience_and_waits_past_grant_expiry() {
    let clock = Arc::new(ManualClock::new(100));
    let storage = FakeStorage::default();
    let storage_state = Arc::clone(&storage.state);
    let boundary = DurableDepthIngest::open(
        storage,
        60,
        Box::new(InMemoryDepthIndexRepository::default()),
    )
    .unwrap();
    let state = HttpState::new(
        boundary,
        DepthTokenVerifier::new(SECRET).unwrap(),
        clock.clone(),
        10,
    )
    .unwrap();
    let app = build_router(state);
    let manifest = manifest();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let (_, scope) = call(
        &app,
        Method::POST,
        "/depth-ingest/v1/archives",
        &token(CLIENT_AUDIENCE, 10_000),
        create_request(&manifest, &manifest_bytes),
    )
    .await;
    let archive_id = scope["archive_id"].as_str().unwrap();
    let key = object_key(
        ArchiveLane::Lane30,
        SESSION,
        &manifest.objects[0].relative_key,
    )
    .unwrap();
    let (_, intent) = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/multipart"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({
            "scope": scope,
            "bucket": ARCHIVE_BUCKET,
            "object_key": key,
            "part_size_bytes": 1
        }),
    )
    .await;
    let upload_id = intent["upload_id"].as_str().unwrap();
    let _ = call(
        &app,
        Method::POST,
        &format!("/depth-ingest/v1/archives/{archive_id}/multipart/{upload_id}/parts/authorize"),
        &token(CLIENT_AUDIENCE, 10_000),
        json!({
            "scope": scope,
            "upload_id": upload_id,
            "bucket": ARCHIVE_BUCKET,
            "object_key": key,
            "part_number": 1,
            "byte_size": 1,
            "sha256": manifest.objects[0].sha256
        }),
    )
    .await;

    clock.0.store(169, Ordering::SeqCst);
    let (status, _) = call(
        &app,
        Method::POST,
        "/depth-ingest/internal/v1/cleanup/orphan-multipart",
        &token(CLIENT_AUDIENCE, 10_000),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, before) = call(
        &app,
        Method::POST,
        "/depth-ingest/internal/v1/cleanup/orphan-multipart",
        &token(DEPTH_INGEST_WORKER_AUDIENCE, 10_000),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["aborted_upload_ids"], json!([]));

    clock.0.store(170, Ordering::SeqCst);
    let (status, after) = call(
        &app,
        Method::POST,
        "/depth-ingest/internal/v1/cleanup/orphan-multipart",
        &token(DEPTH_INGEST_WORKER_AUDIENCE, 10_000),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["aborted_upload_ids"], json!([upload_id]));
    assert_eq!(storage_state.lock().unwrap().aborted_provider_ids.len(), 1);
}

fn manifest() -> ArchiveManifest {
    let hash = contract_sha256(b"x");
    ArchiveManifest {
        schema_version: ARCHIVE_SCHEMA_VERSION.to_string(),
        session_id: SESSION.to_string(),
        lane: ArchiveLane::Lane30,
        non_trainable: true,
        objects: vec![
            object(
                "session/depth-profile-evidence.json",
                ArchiveObjectKind::DepthProfileEvidence,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                "session/dsv-thumbnail.png",
                ArchiveObjectKind::DsvThumbnail,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                "session/frame-pairs.jsonl.zst",
                ArchiveObjectKind::FramePairs,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                "session/action_camera_15fps.json",
                ArchiveObjectKind::ActionCamera,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                "session/systeminfo.json",
                ArchiveObjectKind::SystemInfo,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                &format!("runs/{RUN}/variant-s-recipe.json"),
                ArchiveObjectKind::VariantRecipe,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                &format!("runs/{RUN}/depth-index.jsonl.zst"),
                ArchiveObjectKind::DepthIndex,
                ArchivePriority::P0,
                &hash,
            ),
            object(
                &format!("runs/{RUN}/variant-r/depth_0001.zip"),
                ArchiveObjectKind::VariantRArchive,
                ArchivePriority::P2,
                &hash,
            ),
        ],
    }
}

fn object(
    relative_key: &str,
    kind: ArchiveObjectKind,
    priority: ArchivePriority,
    sha256: &str,
) -> ArchiveObject {
    ArchiveObject {
        relative_key: relative_key.to_string(),
        kind,
        priority,
        size_bytes: 1,
        sha256: sha256.to_string(),
    }
}

fn create_request(manifest: &ArchiveManifest, manifest_bytes: &[u8]) -> Value {
    json!({
        "identity": {
            "principal_id": PRINCIPAL,
            "session_id": SESSION,
            "run_id": RUN,
            "generation": 1,
            "lane": "lane-30",
            "non_trainable": true,
            "schema_version": ARCHIVE_SCHEMA_VERSION,
            "manifest_sha256": contract_sha256(manifest_bytes)
        },
        "target_bucket": ARCHIVE_BUCKET,
        "retention_days": ARCHIVE_RETENTION_DAYS,
        "manifest_size_bytes": manifest_bytes.len(),
        "manifest": manifest
    })
}

fn contract_sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn token(audience: &str, expiry: u64) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        &DepthTokenClaims {
            sub: PRINCIPAL.to_string(),
            aud: audience.to_string(),
            exp: expiry,
        },
        &EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(serde_json::to_vec(&body).unwrap())
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}
