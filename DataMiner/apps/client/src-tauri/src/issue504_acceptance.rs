//! Issue #504 的 post-capture 自动验收器。
//!
//! 它只读客户端 journal 和本地 immutable artifact，再用设备凭据查询现有 clip
//! status API。验收器不改 journal、不补传、不生成缺失 depth，也不把 API 返回的
//! expected hash 冒充直接 R2 回读；`verified` 的含义严格沿用服务端 worker 的流式
//! bytes + SHA-256 校验。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::services::capture_storage::SESSION_ROOT_NAME;
use crate::services::credentials::{
    CredentialKind, PRODUCTION_CREDENTIAL_SCOPE, load, platform_store,
};

const REPORT_SCHEMA: &str = "dm-issue504-depth-acceptance-1";
const REQUIRED_JOURNAL_SCHEMA: i64 = 28;
const FALLOUT_GAME_ID: &str = "fallout4-steam-release-test";
const DEFAULT_API_BASE_URL: &str = "https://api.diogenesmodel.com/";

#[derive(Clone, Debug)]
pub struct AcceptanceOptions {
    pub app_data_root: PathBuf,
    pub session_id: Option<String>,
    pub api_base_url: String,
    pub wait: Duration,
}

impl AcceptanceOptions {
    pub fn production(app_data_root: PathBuf) -> Self {
        Self {
            app_data_root,
            session_id: None,
            api_base_url: DEFAULT_API_BASE_URL.into(),
            wait: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedClip {
    pub segment_index: u32,
    pub clip_id: String,
    pub upload_intent_id: String,
    pub video_sha256: String,
    pub video_bytes: u64,
    pub depth_sha256: String,
    pub depth_bytes: u64,
    pub durable_part_count: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceReport {
    pub schema: &'static str,
    pub checked_at_unix_ms: u64,
    pub passed: bool,
    pub session_id: Option<String>,
    pub game_id: Option<String>,
    pub clips: Vec<AcceptedClip>,
    pub checks: Vec<AcceptanceCheck>,
    pub blockers: Vec<String>,
    pub assurance: &'static str,
}

impl AcceptanceReport {
    fn new() -> Self {
        Self {
            schema: REPORT_SCHEMA,
            checked_at_unix_ms: unix_ms(),
            passed: false,
            session_id: None,
            game_id: None,
            clips: Vec::new(),
            checks: Vec::new(),
            blockers: Vec::new(),
            assurance: "local immutable bytes plus API worker verification; no direct R2 readback",
        }
    }

    fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.checks.push(AcceptanceCheck {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }

    fn finish(mut self) -> Self {
        self.blockers = self
            .checks
            .iter()
            .filter(|check| !check.passed)
            .map(|check| check.name.clone())
            .collect();
        self.passed = self.blockers.is_empty() && !self.clips.is_empty();
        self
    }
}

#[derive(Debug)]
struct SessionRow {
    id: String,
    state: String,
    game_id: String,
}

#[derive(Debug)]
struct SegmentRow {
    id: String,
    seq: u32,
    state: String,
    video_path: Option<String>,
    video_sha256: Option<String>,
    depth_manifest_path: Option<String>,
    depth_manifest_sha256: Option<String>,
    depth_upload_path: Option<String>,
    depth_upload_sha256: Option<String>,
    depth_expected: bool,
    upload_state: Option<String>,
    upload_attempt_count: Option<u32>,
    remote_clip_id: Option<String>,
    remote_upload_intent_id: Option<String>,
    last_error_code: Option<String>,
}

#[derive(Clone, Debug)]
struct LocalArtifact {
    path: PathBuf,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DepthZipInventory {
    schema: String,
    segment_index: u32,
    frames: Vec<DepthZipFrame>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DepthZipFrame {
    session_frame: u64,
    present_seq: u64,
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipStatus {
    clip_id: String,
    storage_state: String,
    verification_state: String,
    retention_state: String,
    artifacts: Vec<RemoteArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteArtifact {
    artifact_type: String,
    state: String,
    byte_size: u64,
    expected_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureStorageConfig {
    schema_version: String,
    artifact_root: String,
}

#[derive(Clone, Debug)]
struct ClipExpectation {
    segment_index: u32,
    clip_id: String,
    upload_intent_id: String,
    video: LocalArtifact,
    depth: LocalArtifact,
    durable_part_count: u32,
}

pub fn run(options: &AcceptanceOptions) -> AcceptanceReport {
    let mut report = AcceptanceReport::new();
    let journal_path = options.app_data_root.join("journal.sqlite3");
    let connection = match Connection::open_with_flags(
        &journal_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => {
            report.check("journal-readable", true, "journal opened read-only");
            connection
        }
        Err(_) => {
            report.check(
                "journal-readable",
                false,
                "journal is missing or unreadable",
            );
            return report.finish();
        }
    };

    let schema_version = connection
        .query_row(
            "SELECT schema_version FROM journal_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .ok();
    let schema_ok = schema_version.is_some_and(|version| version >= REQUIRED_JOURNAL_SCHEMA);
    report.check(
        "journal-depth-schema",
        schema_ok,
        schema_version
            .map(|version| format!("schema v{version}"))
            .unwrap_or_else(|| "schema version unavailable".into()),
    );
    if !schema_ok {
        return report.finish();
    }

    let session = match select_session(&connection, options.session_id.as_deref()) {
        Ok(Some(session)) => session,
        Ok(None) => {
            report.check(
                "fallout-depth-session-selected",
                false,
                "no matching completed depth session",
            );
            return report.finish();
        }
        Err(_) => {
            report.check(
                "fallout-depth-session-selected",
                false,
                "session query failed",
            );
            return report.finish();
        }
    };
    report.session_id = Some(session.id.clone());
    report.game_id = Some(session.game_id.clone());
    let session_ok = session.state == "completed" && session.game_id == FALLOUT_GAME_ID;
    report.check(
        "fallout-depth-session-selected",
        session_ok,
        format!("state={}; game={}", session.state, session.game_id),
    );
    if !session_ok {
        return report.finish();
    }

    let all_runs_depth = connection
        .query_row(
            "SELECT COUNT(*) > 0 AND MIN(depth_expected) = 1
               FROM capture_runs WHERE session_id = ?1",
            params![session.id],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false);
    report.check(
        "all-session-runs-expect-depth",
        all_runs_depth,
        if all_runs_depth {
            "every run is fail-closed for depth"
        } else {
            "a run did not declare depth_expected=1"
        },
    );

    let segments = match load_segments(&connection, &session.id) {
        Ok(segments) => segments,
        Err(_) => {
            report.check("depth-segments-loaded", false, "segment query failed");
            return report.finish();
        }
    };
    report.check(
        "depth-segments-loaded",
        !segments.is_empty(),
        format!("{} segment(s)", segments.len()),
    );
    if segments.is_empty() {
        return report.finish();
    }

    let artifact_root = match capture_output_root(&options.app_data_root) {
        Ok(root) => root,
        Err(_) => {
            report.check(
                "capture-storage-resolved",
                false,
                "capture storage configuration is invalid or unavailable",
            );
            return report.finish();
        }
    };
    report.check(
        "capture-storage-resolved",
        true,
        "active session root resolved",
    );

    let mut expectations = Vec::new();
    for segment in &segments {
        let prefix = format!("segment-{:05}", segment.seq);
        let row_complete = segment.state == "ready"
            && segment.depth_expected
            && segment.video_path.is_some()
            && segment.video_sha256.is_some()
            && segment.depth_manifest_path.is_some()
            && segment.depth_manifest_sha256.is_some()
            && segment.depth_upload_path.is_some()
            && segment.depth_upload_sha256.is_some();
        report.check(
            &format!("{prefix}-depth-artifacts-declared"),
            row_complete,
            if row_complete {
                "ready video + durable depth manifest + real depth ZIP"
            } else {
                "missing, one-sided, ineligible, or non-ready depth artifact"
            },
        );
        if !row_complete {
            continue;
        }

        let video = verify_local_artifact(
            &artifact_root,
            &session.id,
            segment.video_path.as_deref().unwrap_or_default(),
            segment.video_sha256.as_deref().unwrap_or_default(),
        );
        report.check(
            &format!("{prefix}-video-sha256"),
            video.is_ok(),
            artifact_detail(video.as_ref().ok()),
        );
        let manifest = verify_local_artifact(
            &artifact_root,
            &session.id,
            segment.depth_manifest_path.as_deref().unwrap_or_default(),
            segment.depth_manifest_sha256.as_deref().unwrap_or_default(),
        );
        report.check(
            &format!("{prefix}-depth-manifest-sha256"),
            manifest.is_ok(),
            artifact_detail(manifest.as_ref().ok()),
        );
        let depth = verify_local_artifact(
            &artifact_root,
            &session.id,
            segment.depth_upload_path.as_deref().unwrap_or_default(),
            segment.depth_upload_sha256.as_deref().unwrap_or_default(),
        );
        report.check(
            &format!("{prefix}-depth-zip-sha256"),
            depth.is_ok(),
            artifact_detail(depth.as_ref().ok()),
        );
        let zip_inventory = depth
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .and_then(|artifact| verify_depth_zip(&artifact.path, segment.seq));
        report.check(
            &format!("{prefix}-depth-zip-inventory"),
            zip_inventory.is_ok(),
            zip_inventory
                .as_ref()
                .map(|frames| format!("{frames} EXR member(s) match the final inventory"))
                .unwrap_or_else(|_| "ZIP members or inventory are incomplete/mismatched".into()),
        );

        let upload_ok = segment.upload_state.as_deref() == Some("verified")
            && segment
                .upload_attempt_count
                .is_some_and(|attempts| attempts >= 1)
            && segment.remote_clip_id.is_some()
            && segment.remote_upload_intent_id.is_some()
            && segment.last_error_code.is_none();
        report.check(
            &format!("{prefix}-durable-upload-terminal"),
            upload_ok,
            format!(
                "state={}; attempts={}",
                segment.upload_state.as_deref().unwrap_or("missing"),
                segment.upload_attempt_count.unwrap_or(0)
            ),
        );

        let (Ok(video), Ok(depth), Ok(_), Some(clip_id), Some(upload_intent_id)) = (
            video,
            depth,
            zip_inventory,
            segment.remote_clip_id.clone(),
            segment.remote_upload_intent_id.clone(),
        ) else {
            continue;
        };
        let parts = validate_durable_parts(&connection, segment, &video, &depth);
        report.check(
            &format!("{prefix}-durable-video-depth-parts"),
            parts.is_ok(),
            parts
                .as_ref()
                .map(|count| format!("{count} contiguous persisted part(s)"))
                .unwrap_or_else(|_| "part journal is incomplete or non-contiguous".into()),
        );
        let Ok(durable_part_count) = parts else {
            continue;
        };
        if upload_ok {
            expectations.push(ClipExpectation {
                segment_index: segment.seq,
                clip_id,
                upload_intent_id,
                video,
                depth,
                durable_part_count,
            });
        }
    }

    let unique_clips = expectations
        .iter()
        .map(|expectation| expectation.clip_id.as_str())
        .collect::<BTreeSet<_>>();
    let clip_identity_ok =
        expectations.len() == segments.len() && unique_clips.len() == segments.len();
    report.check(
        "one-video-depth-pair-per-clip",
        clip_identity_ok,
        format!(
            "{} local pair(s), {} unique clip(s)",
            expectations.len(),
            unique_clips.len()
        ),
    );
    if !clip_identity_ok {
        return report.finish();
    }

    let remote = verify_remote(options, &expectations);
    match remote {
        Ok(statuses) => {
            report.check(
                "api-worker-verified-video-and-depth",
                true,
                format!(
                    "{} clip(s) stored, retained, and worker-verified",
                    statuses.len()
                ),
            );
            report.clips = expectations
                .into_iter()
                .map(|expectation| AcceptedClip {
                    segment_index: expectation.segment_index,
                    clip_id: expectation.clip_id,
                    upload_intent_id: expectation.upload_intent_id,
                    video_sha256: expectation.video.sha256,
                    video_bytes: expectation.video.bytes,
                    depth_sha256: expectation.depth.sha256,
                    depth_bytes: expectation.depth.bytes,
                    durable_part_count: expectation.durable_part_count,
                })
                .collect();
        }
        Err(detail) => report.check("api-worker-verified-video-and-depth", false, detail),
    }

    report.finish()
}

fn select_session(
    connection: &Connection,
    requested: Option<&str>,
) -> rusqlite::Result<Option<SessionRow>> {
    if let Some(session_id) = requested {
        return connection
            .query_row(
                "SELECT id, state, game_id FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        state: row.get(1)?,
                        game_id: row.get(2)?,
                    })
                },
            )
            .optional();
    }
    connection
        .query_row(
            "SELECT s.id, s.state, s.game_id
               FROM sessions s
              WHERE s.game_id = ?1
                AND s.state = 'completed'
                AND EXISTS (
                    SELECT 1 FROM capture_runs r
                     WHERE r.session_id = s.id AND r.depth_expected = 1
                )
              ORDER BY s.started_at_utc DESC, s.id DESC
              LIMIT 1",
            params![FALLOUT_GAME_ID],
            |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    game_id: row.get(2)?,
                })
            },
        )
        .optional()
}

fn load_segments(connection: &Connection, session_id: &str) -> rusqlite::Result<Vec<SegmentRow>> {
    let mut statement = connection.prepare(
        "SELECT g.id, g.seq, g.state,
                g.video_path, g.video_sha256,
                g.depth_path, g.depth_sha256,
                g.depth_upload_path, g.depth_upload_sha256,
                r.depth_expected,
                j.state, j.attempt_count, j.remote_clip_id,
                j.remote_upload_intent_id, j.last_error_code
           FROM segments g
           JOIN capture_runs r ON r.id = g.run_id
           LEFT JOIN upload_jobs j ON j.segment_id = g.id
          WHERE g.session_id = ?1
          ORDER BY r.seq, g.seq",
    )?;
    statement
        .query_map(params![session_id], |row| {
            Ok(SegmentRow {
                id: row.get(0)?,
                seq: row.get(1)?,
                state: row.get(2)?,
                video_path: row.get(3)?,
                video_sha256: row.get(4)?,
                depth_manifest_path: row.get(5)?,
                depth_manifest_sha256: row.get(6)?,
                depth_upload_path: row.get(7)?,
                depth_upload_sha256: row.get(8)?,
                depth_expected: row.get(9)?,
                upload_state: row.get(10)?,
                upload_attempt_count: row.get(11)?,
                remote_clip_id: row.get(12)?,
                remote_upload_intent_id: row.get(13)?,
                last_error_code: row.get(14)?,
            })
        })?
        .collect()
}

fn capture_output_root(app_data_root: &Path) -> anyhow::Result<PathBuf> {
    let config_path = app_data_root.join("capture-storage-v1.json");
    let artifact_root = if config_path.is_file() {
        let config: CaptureStorageConfig = serde_json::from_slice(&std::fs::read(config_path)?)?;
        anyhow::ensure!(
            config.schema_version == "dm-capture-storage-config-1",
            "unsupported capture storage schema"
        );
        PathBuf::from(config.artifact_root)
    } else {
        app_data_root.to_path_buf()
    };
    anyhow::ensure!(
        artifact_root.is_absolute(),
        "capture storage root is not absolute"
    );
    Ok(artifact_root.join(SESSION_ROOT_NAME))
}

fn verify_local_artifact(
    output_root: &Path,
    session_id: &str,
    relative_path: &str,
    expected_sha256: &str,
) -> anyhow::Result<LocalArtifact> {
    anyhow::ensure!(valid_sha256(expected_sha256), "invalid expected SHA-256");
    let relative = Path::new(relative_path);
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "unsafe artifact path"
    );
    let session_root = output_root.join(session_id).canonicalize()?;
    let path = session_root.join(relative).canonicalize()?;
    anyhow::ensure!(
        path.starts_with(&session_root) && path.is_file(),
        "artifact escaped session root"
    );
    let (bytes, sha256) = sha256_file(&path)?;
    anyhow::ensure!(bytes > 0, "artifact is empty");
    anyhow::ensure!(sha256 == expected_sha256, "artifact SHA-256 mismatch");
    Ok(LocalArtifact {
        path,
        sha256,
        bytes,
    })
}

fn verify_depth_zip(path: &Path, expected_segment_index: u32) -> anyhow::Result<usize> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    anyhow::ensure!(archive.len() >= 2, "depth ZIP has no EXR members");
    let inventory_index = archive.len() - 1;
    let inventory = {
        let mut member = archive.by_index(inventory_index)?;
        anyhow::ensure!(
            member.name() == "depth-files.json",
            "inventory is not final"
        );
        anyhow::ensure!(
            member.compression() == zip::CompressionMethod::Stored,
            "inventory compression is not deterministic"
        );
        anyhow::ensure!(member.size() <= 16 * 1024 * 1024, "inventory is too large");
        let mut bytes = Vec::with_capacity(member.size() as usize);
        member.read_to_end(&mut bytes)?;
        serde_json::from_slice::<DepthZipInventory>(&bytes)?
    };
    anyhow::ensure!(
        inventory.schema == "dm-depth-variant-r-zip-1"
            && inventory.segment_index == expected_segment_index,
        "depth ZIP inventory identity mismatch"
    );
    anyhow::ensure!(
        !inventory.frames.is_empty() && inventory.frames.len() + 1 == archive.len(),
        "depth ZIP inventory count mismatch"
    );
    let mut previous_session_frame = None;
    let mut previous_present_seq = None;
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (index, expected) in inventory.frames.iter().enumerate() {
        anyhow::ensure!(
            valid_sha256(&expected.sha256)
                && expected.size_bytes > 0
                && expected.path == format!("depth_{}.exr", expected.session_frame)
                && previous_session_frame.is_none_or(|previous| expected.session_frame > previous)
                && previous_present_seq.is_none_or(|previous| expected.present_seq > previous),
            "depth ZIP inventory frame is invalid or unordered"
        );
        let mut member = archive.by_index(index)?;
        anyhow::ensure!(
            member.name() == expected.path
                && member.size() == expected.size_bytes
                && member.compression() == zip::CompressionMethod::Stored,
            "depth ZIP EXR identity/size/compression mismatch"
        );
        let mut hasher = Sha256::new();
        let mut bytes = 0_u64;
        loop {
            let read = member.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            bytes = bytes
                .checked_add(read as u64)
                .ok_or_else(|| anyhow::anyhow!("depth EXR size overflow"))?;
        }
        anyhow::ensure!(
            bytes == expected.size_bytes
                && format!("sha256:{:x}", hasher.finalize()) == expected.sha256,
            "depth ZIP EXR content mismatch"
        );
        previous_session_frame = Some(expected.session_frame);
        previous_present_seq = Some(expected.present_seq);
    }
    Ok(inventory.frames.len())
}

fn validate_durable_parts(
    connection: &Connection,
    segment: &SegmentRow,
    video: &LocalArtifact,
    depth: &LocalArtifact,
) -> anyhow::Result<u32> {
    let mut statement = connection.prepare(
        "SELECT artifact_kind, part_number, byte_offset, byte_size
           FROM upload_parts
          WHERE segment_id = ?1 AND artifact_kind IN ('video', 'depth')
          ORDER BY artifact_kind, part_number",
    )?;
    let rows = statement
        .query_map(params![segment.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_kind: BTreeMap<String, Vec<(u32, u64, u64)>> = BTreeMap::new();
    for (kind, part, offset, bytes) in rows {
        by_kind.entry(kind).or_default().push((part, offset, bytes));
    }
    for (kind, expected_bytes) in [("video", video.bytes), ("depth", depth.bytes)] {
        let parts = by_kind
            .get(kind)
            .ok_or_else(|| anyhow::anyhow!("missing parts"))?;
        let mut expected_offset = 0_u64;
        for (expected_part, (part, offset, bytes)) in (1_u32..).zip(parts.iter()) {
            anyhow::ensure!(*part == expected_part, "part number gap");
            anyhow::ensure!(*offset == expected_offset && *bytes > 0, "part offset gap");
            expected_offset = expected_offset
                .checked_add(*bytes)
                .ok_or_else(|| anyhow::anyhow!("part size overflow"))?;
        }
        anyhow::ensure!(
            expected_offset == expected_bytes,
            "durable parts do not cover artifact"
        );
    }
    Ok(by_kind.values().map(Vec::len).sum::<usize>() as u32)
}

fn verify_remote(
    options: &AcceptanceOptions,
    expectations: &[ClipExpectation],
) -> Result<Vec<ClipStatus>, String> {
    let base_url =
        Url::parse(&options.api_base_url).map_err(|_| "API base URL is invalid".to_string())?;
    if base_url.as_str() != DEFAULT_API_BASE_URL {
        return Err("API base URL must be the pinned DataMiner production endpoint".into());
    }
    let store = platform_store();
    let credential = load(
        store.as_ref(),
        CredentialKind::Device,
        PRODUCTION_CREDENTIAL_SCOPE,
    )
    .map_err(|_| "production device credential could not be read".to_string())?
    .ok_or_else(|| "production device credential is absent".to_string())?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
        .map_err(|_| "production device credential is invalid".to_string())?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "HTTP client initialization failed".to_string())?;
    let deadline = Instant::now() + options.wait;
    loop {
        let mut statuses = Vec::new();
        let mut pending = false;
        for expected in expectations {
            let url = base_url
                .join(&format!("v1/clips/{}/status", expected.clip_id))
                .map_err(|_| "clip status URL could not be formed".to_string())?;
            let response = client
                .get(url)
                .header(AUTHORIZATION, bearer.clone())
                .send()
                .map_err(|_| "clip status request failed".to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "clip status returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let status: ClipStatus = response
                .json()
                .map_err(|_| "clip status response was invalid".to_string())?;
            if status.clip_id != expected.clip_id {
                return Err("clip status identity mismatch".into());
            }
            match remote_matches(&status, expected) {
                Ok(true) => statuses.push(status),
                Ok(false) => pending = true,
                Err(error) => return Err(error),
            }
        }
        if !pending && statuses.len() == expectations.len() {
            return Ok(statuses);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for worker verification".into());
        }
        thread::sleep(Duration::from_secs(3));
    }
}

fn remote_matches(status: &ClipStatus, expected: &ClipExpectation) -> Result<bool, String> {
    if status.verification_state == "rejected" {
        return Err("remote clip was rejected".into());
    }
    if status.retention_state != "retained" {
        return Err(format!(
            "remote retention state is {}",
            status.retention_state
        ));
    }
    if status.storage_state != "stored" || status.verification_state != "verified" {
        return Ok(false);
    }
    let artifacts = status
        .artifacts
        .iter()
        .map(|artifact| (artifact.artifact_type.as_str(), artifact))
        .collect::<BTreeMap<_, _>>();
    for (kind, local) in [("video", &expected.video), ("depth", &expected.depth)] {
        let remote = artifacts
            .get(kind)
            .ok_or_else(|| format!("remote clip is missing {kind}"))?;
        if remote.state != "verified"
            || remote.expected_sha256 != local.sha256
            || remote.byte_size != local.bytes
        {
            return Err(format!("remote {kind} bytes/hash/state mismatch"));
        }
    }
    Ok(true)
}

fn sha256_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("artifact size overflow"))?;
    }
    Ok((bytes, format!("sha256:{:x}", hasher.finalize())))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn artifact_detail(artifact: Option<&LocalArtifact>) -> String {
    artifact
        .map(|artifact| format!("{} bytes; SHA-256 matches journal", artifact.bytes))
        .unwrap_or_else(|| "missing, unsafe, empty, or hash mismatch".into())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn local_artifact_must_match_journal_hash_and_stay_inside_session() {
        let root = tempdir().unwrap();
        let session = root.path().join("session-1");
        std::fs::create_dir_all(session.join("runs/r0000/segments/00000")).unwrap();
        let relative = "runs/r0000/segments/00000/depth.zip";
        let path = session.join(relative);
        File::create(&path)
            .unwrap()
            .write_all(b"depth-bytes")
            .unwrap();
        let (_, hash) = sha256_file(&path).unwrap();

        let artifact = verify_local_artifact(root.path(), "session-1", relative, &hash).unwrap();
        assert_eq!(artifact.bytes, 11);
        assert!(
            verify_local_artifact(
                root.path(),
                "session-1",
                relative,
                &format!("sha256:{}", "0".repeat(64))
            )
            .is_err()
        );
        assert!(verify_local_artifact(root.path(), "session-1", "../outside", &hash).is_err());
    }

    #[test]
    fn remote_pair_requires_both_verified_artifacts_with_exact_bytes() {
        let expected = ClipExpectation {
            segment_index: 0,
            clip_id: "clip-1".into(),
            upload_intent_id: "intent-1".into(),
            video: LocalArtifact {
                path: PathBuf::new(),
                sha256: format!("sha256:{}", "a".repeat(64)),
                bytes: 10,
            },
            depth: LocalArtifact {
                path: PathBuf::new(),
                sha256: format!("sha256:{}", "b".repeat(64)),
                bytes: 20,
            },
            durable_part_count: 2,
        };
        let status = ClipStatus {
            clip_id: "clip-1".into(),
            storage_state: "stored".into(),
            verification_state: "verified".into(),
            retention_state: "retained".into(),
            artifacts: vec![
                RemoteArtifact {
                    artifact_type: "video".into(),
                    state: "verified".into(),
                    byte_size: 10,
                    expected_sha256: expected.video.sha256.clone(),
                },
                RemoteArtifact {
                    artifact_type: "depth".into(),
                    state: "verified".into(),
                    byte_size: 20,
                    expected_sha256: expected.depth.sha256.clone(),
                },
            ],
        };
        assert!(remote_matches(&status, &expected).unwrap());
    }

    #[test]
    fn depth_zip_members_must_match_the_final_inventory() {
        let root = tempdir().unwrap();
        let path = root.path().join("depth.zip");
        let exr = b"exact-exr-bytes";
        let hash = format!("sha256:{:x}", Sha256::digest(exr));
        let inventory = DepthZipInventory {
            schema: "dm-depth-variant-r-zip-1".into(),
            segment_index: 3,
            frames: vec![DepthZipFrame {
                session_frame: 42,
                present_seq: 90,
                path: "depth_42.exr".into(),
                size_bytes: exr.len() as u64,
                sha256: hash,
            }],
        };
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("depth_42.exr", options).unwrap();
        writer.write_all(exr).unwrap();
        writer.start_file("depth-files.json", options).unwrap();
        writer
            .write_all(&serde_json::to_vec(&inventory).unwrap())
            .unwrap();
        writer.finish().unwrap();

        assert_eq!(verify_depth_zip(&path, 3).unwrap(), 1);
        assert!(verify_depth_zip(&path, 4).is_err());
    }

    #[test]
    fn report_is_fail_closed_and_never_passes_without_a_clip() {
        let mut report = AcceptanceReport::new();
        report.check("one", true, "ok");
        assert!(!report.finish().passed);

        let mut report = AcceptanceReport::new();
        report.check("one", false, "nope");
        let report = report.finish();
        assert_eq!(report.blockers, vec!["one"]);
    }

    #[test]
    fn production_credential_is_never_sent_to_an_unpinned_host() {
        let mut options = AcceptanceOptions::production(PathBuf::new());
        options.api_base_url = "https://example.invalid/".into();
        let error = verify_remote(&options, &[]).unwrap_err();
        assert!(error.contains("pinned"), "{error}");
    }
}
