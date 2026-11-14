//! Raw-buffer ZIP provenance for the exact-build D3D12 games (KCD2, CP2077). Each uploaded
//! depth segment carries every frame's depth index row, camera row and player row plus the
//! Ready-bound DepthProfile/CameraBinding bytes, so a formal delivery can be rebuilt from the
//! cloud copy alone. This never promotes raw samples to metres.

use super::*;
use crate::camera_action::{CameraPoseProvenance, CameraPoseRow, PlayerPoseRow};
use crate::capture_timing::{DepthIndexEntry, INDEX_SCHEMA_V6};

pub const RAW_DEPTH_ZIP_SCHEMA: &str = "dm-depth-raw-zip-2";
pub const RAW_DEPTH_PROVENANCE_SCHEMA: &str = "dm-depth-raw-provenance-2";
pub const MAX_RAW_PROVENANCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_POSE_LINE_BYTES: u64 = 128 * 1024;
const MAX_SEGMENT_DEPTH_FRAMES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSnapshot {
    pub sha256: String,
    /// Exact UTF-8 snapshot bytes, including whitespace. The hash is over these bytes.
    pub source_json: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDepthFrameProvenance {
    pub session_frame: u64,
    pub depth: DepthIndexEntry,
    pub camera: CameraPoseRow,
    pub player: PlayerPoseRow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDepthProvenance {
    pub schema: String,
    pub storage_domain: String,
    pub identity: StreamIdentity,
    pub segment_index: u32,
    pub depth_profile: RawSnapshot,
    pub camera_binding: RawSnapshot,
    pub frames: Vec<RawDepthFrameProvenance>,
}

/// Game ids whose formal runs upload raw ZIP v2. Anything else keeps the legacy archive.
#[must_use]
pub fn raw_provenance_game(game_id: &str) -> bool {
    game_id == crate::KCD2_MAINLINE_GAME_ID || game_id == crate::CP2077_GAME_ID
}

/// Run snapshot hashes must come from the matching, verified Ready ACK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRunEvidence {
    identity: StreamIdentity,
    probe_root: PathBuf,
    depth_profile: RawSnapshot,
    camera_binding: RawSnapshot,
}

impl RawRunEvidence {
    pub fn load(
        identity: &StreamIdentity,
        probe_root: &Path,
        depth_profile_sha256: &str,
        camera_binding_sha256: &str,
    ) -> Result<Self, ClientArchiveError> {
        let snapshot = |stem: &str, digest: &str| {
            if !valid_sha256(digest) {
                return Err(ClientArchiveError::Contract(
                    "invalid Ready snapshot hash".into(),
                ));
            }
            let path = probe_root.join("provenance").join(format!(
                "{stem}-{}.json",
                digest.trim_start_matches("sha256:")
            ));
            let bytes = read_regular_file(&path, Some(MAX_POSE_LINE_BYTES))?;
            if sha256_bytes(&bytes) != digest {
                return Err(ClientArchiveError::Integrity(
                    "Ready snapshot hash mismatch".into(),
                ));
            }
            Ok(RawSnapshot {
                sha256: digest.into(),
                source_json: String::from_utf8(bytes)
                    .map_err(|error| ClientArchiveError::Contract(error.to_string()))?,
            })
        };
        let evidence = Self {
            identity: identity.clone(),
            probe_root: probe_root.into(),
            depth_profile: snapshot("depth-profile", depth_profile_sha256)?,
            camera_binding: snapshot("camera-binding", camera_binding_sha256)?,
        };
        validate_snapshots(&evidence.depth_profile, &evidence.camera_binding)?;
        Ok(evidence)
    }

    pub(super) fn materialize(
        &self,
        segment: &VerifiedCandidateSegment,
    ) -> Result<Vec<u8>, ClientArchiveError> {
        if self.identity != segment.identity || segment.frames.len() > MAX_SEGMENT_DEPTH_FRAMES {
            return Err(ClientArchiveError::Contract(
                "raw provenance segment identity/count mismatch".into(),
            ));
        }
        #[derive(Deserialize)]
        struct Indexed {
            session_frame: u64,
            #[serde(flatten)]
            entry: DepthIndexEntry,
        }
        let entries: Vec<Indexed> = read_json_lines(&segment.index_path)?;
        let wanted: BTreeSet<_> = segment
            .frames
            .iter()
            .map(|frame| frame.present_seq)
            .collect();
        let cameras = select_poses::<CameraPoseRow>(
            &self.probe_root.join("camera-pose-index.jsonl"),
            &wanted,
            |row| row.present_seq,
        )?;
        let players = select_poses::<PlayerPoseRow>(
            &self.probe_root.join("player-pose-index.jsonl"),
            &wanted,
            |row| row.present_seq,
        )?;
        let frames = entries
            .into_iter()
            .map(|row| {
                let seq = row.entry.present_seq;
                Ok(RawDepthFrameProvenance {
                    session_frame: row.session_frame,
                    depth: row.entry,
                    camera: cameras.get(&seq).cloned().ok_or_else(|| {
                        ClientArchiveError::Contract(format!("camera pose missing for {seq}"))
                    })?,
                    player: players.get(&seq).cloned().ok_or_else(|| {
                        ClientArchiveError::Contract(format!("player pose missing for {seq}"))
                    })?,
                })
            })
            .collect::<Result<Vec<_>, ClientArchiveError>>()?;
        let provenance = RawDepthProvenance {
            schema: RAW_DEPTH_PROVENANCE_SCHEMA.into(),
            storage_domain: "raw_buffer".into(),
            identity: self.identity.clone(),
            segment_index: segment.segment_index,
            depth_profile: self.depth_profile.clone(),
            camera_binding: self.camera_binding.clone(),
            frames,
        };
        provenance.validate()?;
        if provenance.frames.len() != segment.frames.len()
            || provenance
                .frames
                .iter()
                .zip(&segment.frames)
                .any(|(row, frame)| {
                    row.session_frame != frame.session_frame
                        || row.depth.present_seq != frame.present_seq
                })
        {
            return Err(ClientArchiveError::Integrity(
                "raw provenance differs from verified segment frames".into(),
            ));
        }
        let bytes = serde_json::to_vec(&provenance)
            .map_err(|error| ClientArchiveError::Contract(error.to_string()))?;
        if bytes.len() as u64 > MAX_RAW_PROVENANCE_BYTES {
            return Err(ClientArchiveError::Contract(
                "raw provenance exceeds byte bound".into(),
            ));
        }
        Ok(bytes)
    }
}

fn select_poses<T: DeserializeOwned>(
    path: &Path,
    wanted: &BTreeSet<u64>,
    seq: impl Fn(&T) -> u64,
) -> Result<BTreeMap<u64, T>, ClientArchiveError> {
    regular_file_metadata(path)?;
    let mut reader = BufReader::new(File::open(path).map_err(io_error)?);
    let mut selected = BTreeMap::new();
    let mut previous = None;
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_POSE_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(io_error)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_POSE_LINE_BYTES || line.last() != Some(&b'\n') {
            return Err(ClientArchiveError::Contract(
                "pose row is oversized or unterminated".into(),
            ));
        }
        let row: T = serde_json::from_slice(&line)
            .map_err(|error| ClientArchiveError::Contract(format!("invalid pose row: {error}")))?;
        let present = seq(&row);
        if previous.is_some_and(|last| present <= last) {
            return Err(ClientArchiveError::Contract(
                "pose Present order is not strictly increasing".into(),
            ));
        }
        previous = Some(present);
        if wanted.contains(&present) {
            selected.insert(present, row);
        }
    }
    if selected.len() != wanted.len() {
        return Err(ClientArchiveError::Contract(format!(
            "{} lacks selected poses",
            path.display()
        )));
    }
    Ok(selected)
}

/// The exact pair a raw ZIP v2 may carry. Each game keeps its own camera and player contract;
/// the depth rows share one.
enum RawPair {
    Kcd2(crate::Kcd2CombinedCameraBinding),
    Cp2077(crate::D3d12CameraBinding),
}

fn validate_snapshots(
    profile: &RawSnapshot,
    binding: &RawSnapshot,
) -> Result<(crate::DepthProfile, RawPair), ClientArchiveError> {
    for snapshot in [profile, binding] {
        if snapshot.source_json.len() as u64 > MAX_POSE_LINE_BYTES
            || sha256_bytes(snapshot.source_json.as_bytes()) != snapshot.sha256
        {
            return Err(ClientArchiveError::Integrity(
                "raw snapshot bytes/hash mismatch".into(),
            ));
        }
    }
    let profile: crate::DepthProfile = serde_json::from_str(&profile.source_json)
        .map_err(|error| ClientArchiveError::Contract(error.to_string()))?;
    let binding: crate::CameraBindingDocument = serde_json::from_str(&binding.source_json)
        .map_err(|error| ClientArchiveError::Contract(error.to_string()))?;
    let executable = match (profile.game_id.as_str(), &binding) {
        (crate::KCD2_MAINLINE_GAME_ID, crate::CameraBindingDocument::Kcd2Combined(_)) => {
            crate::KCD2_EXECUTABLE_SHA256
        }
        (crate::CP2077_GAME_ID, crate::CameraBindingDocument::V2(d3d12))
            if d3d12.schema == crate::CAMERA_BINDING_SCHEMA_V3
                && d3d12.holstered_camera.is_some() =>
        {
            crate::CP2077_BUILD_20383525_EXE_SHA256
        }
        _ => {
            return Err(ClientArchiveError::Contract(
                "raw ZIP v2 requires the exact KCD2 or CP2077 binding".into(),
            ));
        }
    };
    binding
        .validate_for_profile(&profile, executable)
        .map_err(ClientArchiveError::Contract)?;
    let pair = match binding {
        crate::CameraBindingDocument::Kcd2Combined(binding) => RawPair::Kcd2(binding),
        crate::CameraBindingDocument::V2(binding) => RawPair::Cp2077(binding),
        crate::CameraBindingDocument::V1(_) => unreachable!("checked schema"),
    };
    Ok((profile, pair))
}

/// The projection the D3D12 probe writes into a CP2077 depth row: the same-Present camera
/// projection matrix, mapped through the admitted draw's viewport depth range
/// (`resolve_observed_index_projection` / `apply_viewport_to_projection` in the probe).
fn cp2077_row_projection(
    projection_matrix: &crate::camera::Mat4,
    min_depth: f64,
    max_depth: f64,
) -> Option<(f64, f64)> {
    let observed = crate::camera_track::projection_from_matrix(projection_matrix)?;
    let range = max_depth - min_depth;
    Some((min_depth + range * observed.a, range * observed.b))
}

impl RawDepthProvenance {
    pub fn validate(&self) -> Result<(), ClientArchiveError> {
        if self.schema != RAW_DEPTH_PROVENANCE_SCHEMA
            || self.storage_domain != "raw_buffer"
            || self.frames.is_empty()
            || self.frames.len() > MAX_SEGMENT_DEPTH_FRAMES
        {
            return Err(ClientArchiveError::Contract(
                "raw provenance schema/domain/count mismatch".into(),
            ));
        }
        StreamIdentity::new(
            &self.identity.session_id,
            &self.identity.run_id,
            self.identity.generation,
        )
        .map_err(|error| ClientArchiveError::Contract(error.to_string()))?;
        let (profile, pair) = validate_snapshots(&self.depth_profile, &self.camera_binding)?;
        let mut previous_frame = None;
        let mut previous_present = None;
        for row in &self.frames {
            self.validate_shared_row(row, &profile, previous_frame, previous_present)?;
            match &pair {
                RawPair::Kcd2(binding) => validate_kcd2_row(row, &profile, binding)?,
                RawPair::Cp2077(binding) => validate_cp2077_row(row, &profile, binding)?,
            }
            previous_frame = Some(row.session_frame);
            previous_present = Some(row.depth.present_seq);
        }
        Ok(())
    }

    /// Depth, identity and ordering rules every raw v2 game shares.
    fn validate_shared_row(
        &self,
        row: &RawDepthFrameProvenance,
        profile: &crate::DepthProfile,
        previous_frame: Option<u64>,
        previous_present: Option<u64>,
    ) -> Result<(), ClientArchiveError> {
        let depth = &row.depth;
        let camera = &row.camera;
        let player = &row.player;
        crate::validate_camera_pose_row(camera)
            .map_err(|error| ClientArchiveError::Contract(error.into()))?;
        crate::validate_player_pose_row(player)
            .map_err(|error| ClientArchiveError::Contract(error.into()))?;
        let same_identity = |actual: &crate::StreamIdentity| {
            actual.session_id == self.identity.session_id
                && actual.run_id == self.identity.run_id
                && actual.generation == self.identity.generation
        };
        let pass = depth
            .resolve_pass()
            .map_err(ClientArchiveError::Contract)?
            .ok_or_else(|| {
                ClientArchiveError::Contract("raw v2 depth lacks pass evidence".into())
            })?;
        let transform = depth
            .resolve_transform()
            .map_err(ClientArchiveError::Contract)?;
        let health = depth
            .resolve_health()
            .map_err(ClientArchiveError::Contract)?
            .ok_or_else(|| {
                ClientArchiveError::Contract("raw v2 depth lacks health evidence".into())
            })?;
        if depth.schema != INDEX_SCHEMA_V6
            || depth.source_frame != depth.present_seq
            || depth.dsv_id != profile.dsv.dsv_id
            || pass.selection.selected_id != depth.dsv_id
            || !pass.selection.operator_exact_pin
            || pass.selection.any_msaa
            || !depth.projection.is_metric_resolvable()
            || depth.projection.metric_scale != Some(profile.metric_scale.value)
            || health.is_defective()
            || health.raw.pixels != u64::from(depth.width) * u64::from(depth.height)
            || depth.width != profile.delivery_width
            || depth.height != profile.delivery_height
            || depth.storage.domain != "raw_buffer"
            || depth.storage.encoding != "exr-half-zip"
            || depth.storage.channel != crate::EXR_CHANNEL_NAME
            || transform != crate::capture_timing::StorageTransform::Identity
            || depth.projection.reverse_z != Some(true)
            || depth.path != format!("raw/{:012}.exr", depth.present_seq)
            || !same_identity(&camera.identity)
            || !same_identity(&player.identity)
            || camera.present_seq != depth.present_seq
            || player.present_seq != depth.present_seq
            || camera.present_qpc != player.present_qpc
            || player.executable_sha256 != profile.executable_sha256
            || previous_frame.is_some_and(|last| row.session_frame <= last)
            || previous_present.is_some_and(|last| depth.present_seq < last)
        {
            return Err(ClientArchiveError::Integrity(format!(
                "raw depth/pose identity mismatch at {}",
                row.session_frame
            )));
        }
        Ok(())
    }
}

fn validate_kcd2_row(
    row: &RawDepthFrameProvenance,
    profile: &crate::DepthProfile,
    binding: &crate::Kcd2CombinedCameraBinding,
) -> Result<(), ClientArchiveError> {
    let depth = &row.depth;
    let camera = &row.camera;
    let player = &row.player;
    let observed_root = binding.live_pose_plan.schema == crate::KCD2_LIVE_POSE_PLAN_SCHEMA_V3;
    let expected_player_source = if observed_root {
        "kcd2-horizontal-player-root-exact-build/v2"
    } else {
        "kcd2-horizontal-player-root-exact-build/v1"
    };
    let root_source_matches_binding = match &player.kcd2_root_source {
        Some(source) if observed_root => {
            binding.live_pose_plan.classify_source(
                &source.root_signature_sha256,
                source.root_parameter_index,
                source.pass_ordinal,
                source.draw_ordinal,
                0,
                256,
            ) == Some(crate::Kcd2LivePoseSourceKind::HorizontalPlayerRoot)
        }
        None => !observed_root,
        Some(_) => false,
    };
    if player.source != expected_player_source || !root_source_matches_binding {
        return Err(ClientArchiveError::Integrity(format!(
            "raw depth/pose identity mismatch at {}",
            row.session_frame
        )));
    }
    let CameraPoseProvenance::ResolvedCombinedViewProjection {
        binding_id,
        binding_version,
        profile_id,
        profile_version,
        executable_sha256,
        render_width,
        render_height,
        view_metres_per_unit,
        axis_mapping,
        root_signature_sha256,
        root_parameter_index,
        pass_ordinal,
        draw_ordinal,
        dsv_id,
        source_qpc,
        projection_a,
        projection_b,
        ..
    } = &camera.provenance
    else {
        return Err(ClientArchiveError::Contract(
            "raw v2 camera is not KCD2 combined projection".into(),
        ));
    };
    if binding_id != &binding.binding_id
        || binding_version != &binding.binding_version
        || profile_id != &binding.profile_id
        || profile_version != &binding.profile_version
        || executable_sha256 != &binding.executable_sha256
        || *render_width != binding.render_width
        || *render_height != binding.render_height
        || view_metres_per_unit != &binding.view_metres_per_unit
        || axis_mapping != &binding.axis_mapping
        || binding.live_pose_plan.classify_source(
            root_signature_sha256,
            *root_parameter_index,
            *pass_ordinal,
            *draw_ordinal,
            0,
            256,
        ) != Some(crate::Kcd2LivePoseSourceKind::ViewProjection)
        || dsv_id != &depth.dsv_id
        || *source_qpc > camera.present_qpc
        || depth
            .projection
            .proj_a
            .is_none_or(|a| (a - projection_a).abs() > 1.0e-12)
        || depth
            .projection
            .proj_b
            .is_none_or(|b| (b - projection_b).abs() > 1.0e-12)
        || depth.projection.metric_scale != Some(profile.metric_scale.value)
    {
        return Err(ClientArchiveError::Integrity(
            "raw depth/camera projection mismatch".into(),
        ));
    }
    Ok(())
}

/// CP2077 rows carry the whole Ready-bound D3D12 binding and the raw same-Present matrices; the
/// player root is the state whose FPP camera matched this Present's rendered camera
/// (`cp2077_pose_join`), so its receipt is required and checked by `validate_player_pose_row`.
fn validate_cp2077_row(
    row: &RawDepthFrameProvenance,
    profile: &crate::DepthProfile,
    binding: &crate::D3d12CameraBinding,
) -> Result<(), ClientArchiveError> {
    let depth = &row.depth;
    let player = &row.player;
    if player.source != crate::CP2077_PLAYER_POSE_SOURCE
        || player.cp2077_camera_join.is_none()
        || player.kcd2_root_source.is_some()
    {
        return Err(ClientArchiveError::Integrity(format!(
            "raw depth/pose identity mismatch at {}",
            row.session_frame
        )));
    }
    let CameraPoseProvenance::ResolvedD3d12CameraHook {
        binding: row_binding,
        dsv_id,
        projection_matrix,
        ..
    } = &row.camera.provenance
    else {
        return Err(ClientArchiveError::Contract(
            "raw v2 camera is not the CP2077 D3D12 camera hook".into(),
        ));
    };
    let expected = match (
        depth.projection.viewport_min_depth,
        depth.projection.viewport_max_depth,
    ) {
        (Some(min_depth), Some(max_depth)) => {
            cp2077_row_projection(projection_matrix, min_depth, max_depth)
        }
        _ => None,
    };
    let projection_matches = expected.is_some_and(|(a, b)| {
        depth
            .projection
            .proj_a
            .is_some_and(|actual| (actual - a).abs() <= 1.0e-12)
            && depth
                .projection
                .proj_b
                .is_some_and(|actual| (actual - b).abs() <= 1.0e-12)
    });
    if **row_binding != *binding
        || dsv_id != &depth.dsv_id
        || binding.executable_sha256 != profile.executable_sha256
        || depth.projection.proj_source
            != crate::capture_timing::IndexProjectionSource::ConstantBuffer
        || !projection_matches
    {
        return Err(ClientArchiveError::Integrity(
            "raw depth/camera projection mismatch".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_timing::{DepthStorage, DsvPassEvidence, IndexProjection, StorageTransform};
    use crate::segment_finalize::{
        FinalizeRequest, FrameCaptureState, FrameStateBoundary, ProbeFlushedEvidence,
        SegmentBoundary, finalize_segments,
    };

    fn fixture(root: &Path) -> (RawDepthProvenance, Vec<f32>) {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let catalog = config.join("catalogs/dev/kingdom-come-deliverance-2-steam");
        let snapshot = |name| {
            // Keep legacy ZIP coverage independent of the current catalog's v3 rollout.
            // The observed-source test below explicitly promotes this sealed v2 fixture.
            let path = if name == "camera-binding.json" {
                config.join("depth-profiles/kcd2-camera-binding-candidate.json")
            } else {
                catalog.join(name)
            };
            let source_json = fs::read_to_string(path).unwrap();
            RawSnapshot {
                sha256: sha256_bytes(source_json.as_bytes()),
                source_json,
            }
        };
        let identity = StreamIdentity::new("raw-zip-fixture", "r0000", 7).unwrap();
        let pose_identity = crate::StreamIdentity {
            session_id: identity.session_id.clone(),
            run_id: identity.run_id.clone(),
            generation: identity.generation,
        };
        // Match the real KCD2 writer: the admitted DSV is 1920x1080, while
        // the existing GPU pool writes 1280x720 f32 readbacks to HALF EXR.
        // raw_buffer describes the unlinearized domain, not native geometry.
        let values: Vec<f32> = (0..1280 * 720)
            .map(|i| 0.1 + (i % 1000) as f32 / 2000.0)
            .collect();
        let depth = DepthIndexEntry {
            schema: INDEX_SCHEMA_V6.into(),
            present_seq: 10,
            source_frame: 10,
            depth_name: None,
            width: 1280,
            height: 720,
            format: crate::decode::DepthFormat::D32Float,
            dsv_id: crate::KCD2_MAIN_SCENE_DSV_ID.into(),
            storage: DepthStorage::raw_buffer_exr(StorageTransform::Identity),
            path: "raw/000000000010.exr".into(),
            readback_latency_frames: 1,
            projection: IndexProjection::observed(
                crate::Projection {
                    a: crate::KCD2_PROJECTION_A,
                    b: crate::KCD2_PROJECTION_B,
                    metric_scale: 1.0,
                },
                Some(1.0),
            ),
            health: Some(crate::frame_health::FrameHealthEvidence::inspect(
                &values,
                1280,
                StorageTransform::Identity,
            )),
            pass: Some(DsvPassEvidence {
                unbind_ordinal_from_end: 1,
                unbind_total: 1,
                draw_calls: 0,
                selection: crate::DsvSelectionAudit {
                    selected_id: crate::KCD2_MAIN_SCENE_DSV_ID.into(),
                    ranked_top_id: Some(crate::KCD2_MAIN_SCENE_DSV_ID.into()),
                    score_margin: None,
                    selected_matches_top: true,
                    ranking_confident: true,
                    any_msaa: false,
                    operator_exact_pin: true,
                },
            }),
        };
        let camera_binding = snapshot("camera-binding.json");
        let binding: crate::Kcd2CombinedCameraBinding =
            serde_json::from_str(&camera_binding.source_json).unwrap();
        let camera = CameraPoseRow {
            schema: crate::CAMERA_POSE_INDEX_SCHEMA_V1.into(),
            identity: pose_identity.clone(),
            present_seq: 10,
            present_qpc: 1000,
            source: crate::CameraPoseSource::ResolvedCombinedViewProjection,
            provenance: CameraPoseProvenance::ResolvedCombinedViewProjection {
                binding_id: binding.binding_id,
                binding_version: binding.binding_version,
                profile_id: binding.profile_id,
                profile_version: binding.profile_version,
                executable_sha256: binding.executable_sha256,
                dsv_id: crate::KCD2_MAIN_SCENE_DSV_ID.into(),
                render_width: 1920,
                render_height: 1080,
                view_metres_per_unit: "1.0".into(),
                axis_mapping: binding.axis_mapping,
                source_qpc: 990,
                root_signature_sha256: format!(
                    "sha256:{}",
                    crate::KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256
                ),
                root_parameter_index: 2,
                pass_ordinal: 0,
                draw_ordinal: 0,
                projection_a: crate::KCD2_PROJECTION_A,
                projection_b: crate::KCD2_PROJECTION_B,
                projection_decomposition_residual: 0.0,
                held_out_depth_row_residual: 0.0,
            },
            camera_position: [1.0, 2.0, 3.0],
            camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            camera_intrinsics: crate::camera::Intrinsics {
                fx: 960.0,
                fy: 960.0,
                cx: 960.0,
                cy: 540.0,
            },
            camera_speed: None,
        };
        let player = PlayerPoseRow {
            schema: crate::PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: pose_identity,
            present_seq: 10,
            present_qpc: 1000,
            executable_sha256: crate::KCD2_EXECUTABLE_SHA256.into(),
            source: "kcd2-horizontal-player-root-exact-build/v1".into(),
            kcd2_root_source: None,
            cp2077_camera_join: None,
            player_position: [1.0, 1.0, 3.0],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: Some([0.0; 3]),
        };
        fs::create_dir_all(root).unwrap();
        let evidence = RawDepthProvenance {
            schema: RAW_DEPTH_PROVENANCE_SCHEMA.into(),
            storage_domain: "raw_buffer".into(),
            identity,
            segment_index: 0,
            depth_profile: snapshot("depth-profile.json"),
            camera_binding,
            frames: vec![RawDepthFrameProvenance {
                session_frame: 0,
                depth,
                camera,
                player,
            }],
        };
        evidence.validate().unwrap();
        (evidence, values)
    }

    #[test]
    fn raw_source_alias_requires_matching_versioned_binding_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let (mut provenance, values) = fixture(&probe);
        let legacy_provenance = provenance.clone();
        let legacy_snapshot = provenance.camera_binding.clone();
        let mut binding: crate::Kcd2CombinedCameraBinding =
            serde_json::from_str(&legacy_snapshot.source_json).unwrap();
        binding.schema = crate::KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2.into();
        binding.binding_id = crate::KCD2_COMBINED_BINDING_ID_V2.into();
        binding.binding_version = crate::KCD2_COMBINED_BINDING_VERSION_V2.into();
        binding.live_pose_plan = crate::Kcd2LivePosePlan::pinned_v3();
        let alias = &binding.live_pose_plan.additional_view_projection_bindings[0];
        if let CameraPoseProvenance::ResolvedCombinedViewProjection {
            binding_id,
            binding_version,
            root_signature_sha256,
            root_parameter_index,
            pass_ordinal,
            draw_ordinal,
            ..
        } = &mut provenance.frames[0].camera.provenance
        {
            *binding_id = binding.binding_id.clone();
            *binding_version = binding.binding_version.clone();
            *root_signature_sha256 = format!("sha256:{}", alias.root_signature_sha256);
            *root_parameter_index = alias.root_parameter_index;
            *pass_ordinal = 32;
            *draw_ordinal = 0;
        }
        let source_json = serde_json::to_string_pretty(&binding).unwrap();
        provenance.camera_binding = RawSnapshot {
            sha256: sha256_bytes(source_json.as_bytes()),
            source_json,
        };
        // The new binding requires both actual sources; changing only the
        // camera identity cannot promote a legacy player row into v3 evidence.
        assert!(provenance.validate().is_err());
        let player = &mut provenance.frames[0].player;
        player.source = "kcd2-horizontal-player-root-exact-build/v2".into();
        player.kcd2_root_source = Some(crate::Kcd2LivePoseSourceIdentity {
            root_signature_sha256: binding
                .live_pose_plan
                .horizontal_player_root_binding
                .root_signature_sha256
                .clone(),
            root_parameter_index: 0,
            pass_ordinal: 15,
            draw_ordinal: 2,
        });
        provenance.validate().unwrap();
        let encoded = serde_json::to_vec(&provenance).unwrap();
        let decoded = serde_json::from_slice::<RawDepthProvenance>(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(
            decoded.frames[0].player.kcd2_root_source,
            provenance.frames[0].player.kcd2_root_source
        );
        assert_raw_zip_replay(&temp, &probe, &provenance, &values);
        for defect in 0..5 {
            let mut bad = provenance.clone();
            let player = &mut bad.frames[0].player;
            match defect {
                0 => player.kcd2_root_source = None,
                1 => {
                    player
                        .kcd2_root_source
                        .as_mut()
                        .unwrap()
                        .root_signature_sha256 = "0".repeat(64)
                }
                2 => {
                    player
                        .kcd2_root_source
                        .as_mut()
                        .unwrap()
                        .root_parameter_index = 1
                }
                3 => player.kcd2_root_source.as_mut().unwrap().draw_ordinal = 16,
                _ => {
                    player.source = "kcd2-horizontal-player-root-exact-build/v1".into();
                    player.kcd2_root_source = None;
                }
            }
            assert!(bad.validate().is_err(), "defect {defect}");
        }
        let mut legacy_with_observed_player = legacy_provenance;
        legacy_with_observed_player.frames[0].player = provenance.frames[0].player.clone();
        assert!(legacy_with_observed_player.validate().is_err());
        provenance.camera_binding = legacy_snapshot;
        assert!(provenance.validate().is_err());
    }

    /// One CP2077 frame built the way the formal D3D12 path writes it: the camera row from
    /// `assemble_pose` with the catalog binding, the depth projection from that row's projection
    /// matrix and the draw viewport, and a player row carrying its camera-content join receipt.
    fn cp2077_fixture() -> (RawDepthProvenance, Vec<f32>) {
        let catalog = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/catalogs/dev/cyberpunk-2077-steam");
        let snapshot = |name: &str| {
            let source_json = fs::read_to_string(catalog.join(name)).unwrap();
            RawSnapshot {
                sha256: sha256_bytes(source_json.as_bytes()),
                source_json,
            }
        };
        let depth_profile = snapshot("depth-profile.json");
        let camera_binding = snapshot("camera-binding.json");
        let profile: crate::DepthProfile =
            serde_json::from_str(&depth_profile.source_json).unwrap();
        let binding: crate::D3d12CameraBinding =
            serde_json::from_str(&camera_binding.source_json).unwrap();
        let identity = StreamIdentity::new("raw-zip-cp2077", "r0000", 3).unwrap();
        let pose_identity = crate::StreamIdentity {
            session_id: identity.session_id.clone(),
            run_id: identity.run_id.clone(),
            generation: identity.generation,
        };
        // Reverse-Z perspective with the profile's own a/b, and a proper view at (2, 3, 4).
        let projection: crate::camera::Mat4 = [
            [1., 0., 0., 0.],
            [0., 16. / 9., 0., 0.],
            [0., 0., profile.projection_a as f32, 1.],
            [0., 0., profile.projection_b as f32, 0.],
        ];
        let view: crate::camera::Mat4 = [
            [0., 1., 0., 0.],
            [1., 0., 0., 0.],
            [0., 0., 1., 0.],
            [-3., -2., -4., 1.],
        ];
        let camera = crate::d3d12_camera::assemble_pose(
            &binding,
            &crate::CameraPoseStreamContext {
                identity: pose_identity.clone(),
                dsv_id: profile.dsv.dsv_id.clone(),
            },
            10,
            1000,
            view,
            projection,
        )
        .unwrap();
        let (min_depth, max_depth) = (0.0, 1.0);
        let (a, b) = cp2077_row_projection(&projection, min_depth, max_depth).unwrap();
        let values: Vec<f32> = (0..1280 * 720)
            .map(|i| 0.1 + (i % 1000) as f32 / 2000.0)
            .collect();
        let depth = DepthIndexEntry {
            schema: INDEX_SCHEMA_V6.into(),
            present_seq: 10,
            source_frame: 10,
            depth_name: None,
            width: profile.delivery_width,
            height: profile.delivery_height,
            format: crate::decode::DepthFormat::D32Float,
            dsv_id: profile.dsv.dsv_id.clone(),
            storage: DepthStorage::raw_buffer_exr(StorageTransform::Identity),
            path: "raw/000000000010.exr".into(),
            readback_latency_frames: 1,
            projection: IndexProjection::observed_with_viewport(
                crate::Projection {
                    a,
                    b,
                    metric_scale: profile.metric_scale.value,
                },
                Some(profile.metric_scale.value),
                min_depth,
                max_depth,
            ),
            health: Some(crate::frame_health::FrameHealthEvidence::inspect(
                &values,
                1280,
                StorageTransform::Identity,
            )),
            pass: Some(DsvPassEvidence {
                unbind_ordinal_from_end: 4,
                unbind_total: 12,
                draw_calls: 0,
                selection: crate::DsvSelectionAudit {
                    selected_id: profile.dsv.dsv_id.clone(),
                    ranked_top_id: Some(profile.dsv.dsv_id.clone()),
                    score_margin: None,
                    selected_matches_top: true,
                    ranking_confident: true,
                    any_msaa: false,
                    operator_exact_pin: true,
                },
            }),
        };
        let player = PlayerPoseRow {
            schema: crate::PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: pose_identity,
            present_seq: 10,
            present_qpc: 1000,
            executable_sha256: crate::CP2077_BUILD_20383525_EXE_SHA256.into(),
            source: crate::CP2077_PLAYER_POSE_SOURCE.into(),
            kcd2_root_source: None,
            cp2077_camera_join: Some(crate::Cp2077CameraJoinReceipt {
                state_first_qpc: 960,
                state_last_qpc: 990,
                state_reads: 21,
                position_residual_metres: 0.0002,
                rotation_residual_degrees: 0.00001,
                candidates: 1,
                torn_candidates: 0,
            }),
            player_position: [2.0, 3.0, 2.3],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: None,
        };
        let evidence = RawDepthProvenance {
            schema: RAW_DEPTH_PROVENANCE_SCHEMA.into(),
            storage_domain: "raw_buffer".into(),
            identity,
            segment_index: 0,
            depth_profile,
            camera_binding,
            frames: vec![RawDepthFrameProvenance {
                session_frame: 0,
                depth,
                camera,
                player,
            }],
        };
        evidence.validate().unwrap();
        (evidence, values)
    }

    #[test]
    fn cp2077_raw_zip_v2_carries_the_joined_player_and_replays() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let (provenance, values) = cp2077_fixture();
        assert!(raw_provenance_game(crate::CP2077_GAME_ID));
        assert_raw_zip_replay(&temp, &probe, &provenance, &values);
    }

    #[test]
    fn cp2077_raw_provenance_rejects_binding_projection_and_player_drift() {
        let (provenance, _) = cp2077_fixture();
        let (kcd2, _) = fixture(&tempfile::tempdir().unwrap().path().join("probe"));
        let mut cases: Vec<(&str, RawDepthProvenance, &str)> = Vec::new();
        let projection_mismatch = "raw depth/camera projection mismatch";
        let identity_mismatch = "raw depth/pose identity mismatch";

        let mut bad = provenance.clone();
        if let CameraPoseProvenance::ResolvedD3d12CameraHook { binding, .. } =
            &mut bad.frames[0].camera.provenance
        {
            binding.binding_version.push_str("-other");
        }
        cases.push((
            "row binding differs from the Ready snapshot",
            bad,
            projection_mismatch,
        ));
        let mut bad = provenance.clone();
        if let CameraPoseProvenance::ResolvedD3d12CameraHook { dsv_id, .. } =
            &mut bad.frames[0].camera.provenance
        {
            *dsv_id = "another-dsv".into();
        }
        cases.push((
            "camera DSV differs from the depth row",
            bad,
            projection_mismatch,
        ));
        let mut bad = provenance.clone();
        bad.frames[0].depth.projection.proj_a =
            bad.frames[0].depth.projection.proj_a.map(|a| a + 1e-9);
        cases.push((
            "depth projection drifts from the camera matrix",
            bad,
            projection_mismatch,
        ));
        let mut bad = provenance.clone();
        bad.frames[0].depth.projection.viewport_min_depth = None;
        bad.frames[0].depth.projection.viewport_max_depth = None;
        cases.push((
            "depth row lost its viewport range",
            bad,
            projection_mismatch,
        ));
        let mut bad = provenance.clone();
        bad.frames[0].player.cp2077_camera_join = None;
        cases.push((
            "player row without its join receipt",
            bad,
            "CP2077 player pose lacks its camera join receipt",
        ));
        let mut bad = provenance.clone();
        bad.frames[0].player.source = "kcd2-horizontal-player-root-exact-build/v1".into();
        bad.frames[0].player.cp2077_camera_join = None;
        cases.push(("player from another game's source", bad, identity_mismatch));
        let mut bad = provenance.clone();
        bad.frames[0].player.present_qpc += 1;
        cases.push(("player sampled for another Present", bad, identity_mismatch));
        let mut bad = provenance.clone();
        bad.camera_binding = kcd2.camera_binding.clone();
        cases.push((
            "CP2077 profile with the KCD2 binding",
            bad,
            "requires the exact KCD2 or CP2077 binding",
        ));
        let mut bad = provenance.clone();
        let mut camera = kcd2.frames[0].camera.clone();
        camera.identity = provenance.frames[0].camera.identity.clone();
        camera.present_seq = provenance.frames[0].camera.present_seq;
        camera.present_qpc = provenance.frames[0].camera.present_qpc;
        bad.frames[0].camera = camera;
        cases.push((
            "KCD2 camera row under the CP2077 pair",
            bad,
            "not the CP2077 D3D12 camera hook",
        ));
        for (name, case, expected) in cases {
            let error = case.validate().expect_err(name).to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn raw_zip_v2_preserves_source_pose_and_replays_immutable_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let (provenance, values) = fixture(&probe);
        assert_raw_zip_replay(&temp, &probe, &provenance, &values);
    }

    fn assert_raw_zip_replay(
        temp: &tempfile::TempDir,
        probe: &Path,
        provenance: &RawDepthProvenance,
        values: &[f32],
    ) {
        let row = &provenance.frames[0];
        fs::create_dir_all(probe.join("provenance")).unwrap();
        for (stem, snapshot) in [
            ("depth-profile", &provenance.depth_profile),
            ("camera-binding", &provenance.camera_binding),
        ] {
            fs::write(
                probe.join("provenance").join(format!(
                    "{stem}-{}.json",
                    snapshot.sha256.trim_start_matches("sha256:")
                )),
                &snapshot.source_json,
            )
            .unwrap();
        }
        crate::exr_out::write_buffer_frame(&probe.join(&row.depth.path), values, 1280, 720)
            .unwrap();
        for (name, value) in [
            (
                "depth-index.jsonl",
                serde_json::to_value(&row.depth).unwrap(),
            ),
            (
                "camera-pose-index.jsonl",
                serde_json::to_value(&row.camera).unwrap(),
            ),
            (
                "player-pose-index.jsonl",
                serde_json::to_value(&row.player).unwrap(),
            ),
        ] {
            fs::write(
                probe.join(name),
                format!("{}\n", serde_json::to_string(&value).unwrap()),
            )
            .unwrap();
        }
        let frames = (0..2)
            .map(|n| crate::pipeline::RecorderFrameRow {
                session_frame: n,
                segment_index: 0,
                segment_frame: n as u32,
                source_qpc: 1000 + n as i64,
                duplicate: false,
                present_seq: Some(10 + n),
            })
            .collect::<Vec<_>>();
        let depth =
            vec![serde_json::from_value(serde_json::to_value(&row.depth).unwrap()).unwrap()];
        let (pairs, report) =
            crate::pipeline::run_selected_present(&frames, &depth, crate::CameraMode::Player)
                .unwrap();
        let pairing = crate::segment_finalize::VerifiedPairingEvidence::new(
            provenance.identity.clone(),
            pairs,
            report,
        )
        .unwrap();
        let flushed = ProbeFlushedEvidence {
            schema: crate::segment_finalize::FLUSH_EVIDENCE_SCHEMA.into(),
            identity: provenance.identity.clone(),
            writer_queue_depth: 0,
            last_present_seq: 11,
            cutoff_present_seq: 11,
        };
        let states = FrameStateEvidence {
            schema: crate::segment_finalize::FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: provenance.identity.clone(),
            boundaries: vec![FrameStateBoundary {
                session_frame_start: 0,
                frame_count: 2,
                state: FrameCaptureState::Gameplay,
                camera_coverage_proven: true,
                action_coverage_proven: true,
            }],
        };
        let published = finalize_segments(&FinalizeRequest {
            identity: &provenance.identity,
            archive_lane: ArchiveLane::Lane30,
            flushed: &flushed,
            pairing: &pairing,
            frame_states: &states,
            boundaries: &[SegmentBoundary {
                segment_index: 0,
                session_frame_start: 0,
                frame_count: 2,
            }],
            probe_root: probe,
            output_root: &temp.path().join("segments"),
        })
        .unwrap();
        let evidence = RawRunEvidence::load(
            &provenance.identity,
            probe,
            &provenance.depth_profile.sha256,
            &provenance.camera_binding.sha256,
        )
        .unwrap();
        let output = temp.path().join("depth.zip");
        let request = SegmentUploadRequest {
            identity: &provenance.identity,
            segment_index: 0,
            manifest_path: &published[0].manifest_path,
            manifest_sha256: &published[0].manifest_sha256,
            output_path: &output,
            raw_evidence: Some(&evidence),
        };
        let first = materialize_segment_upload(&request).unwrap();
        assert_eq!(first.frame_count, 1);
        assert_eq!(
            read_segment_upload_format(&output).unwrap(),
            RAW_DEPTH_ZIP_SCHEMA
        );
        assert_eq!(
            materialize_segment_upload(&request).unwrap().sha256,
            first.sha256
        );
        let second_path = temp.path().join("depth-copy.zip");
        assert_eq!(
            materialize_segment_upload(&SegmentUploadRequest {
                output_path: &second_path,
                ..request.clone()
            })
            .unwrap()
            .sha256,
            first.sha256
        );
        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let bytes =
            read_bounded_zip_member(&mut archive, 1, "provenance.json", MAX_RAW_PROVENANCE_BYTES)
                .unwrap();
        let decoded: RawDepthProvenance = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(&decoded, provenance);
        fs::write(probe.join("player-pose-index.jsonl"), b"").unwrap();
        assert!(
            materialize_segment_upload(&request).is_err(),
            "replay must reject missing live evidence"
        );
    }

    #[test]
    fn raw_provenance_rejects_identity_projection_and_snapshot_drift() {
        let temp = tempfile::tempdir().unwrap();
        let (provenance, _) = fixture(temp.path());
        let mut bad = provenance.clone();
        bad.frames[0].player.present_seq += 1;
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        bad.frames[0].camera.identity.generation += 1;
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        bad.frames[0].depth.dsv_id = "main-scene-v1".into();
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        bad.frames[0].depth.projection.proj_b = Some(0.2);
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        bad.depth_profile.source_json.push(' ');
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        if let CameraPoseProvenance::ResolvedCombinedViewProjection { binding_id, .. } =
            &mut bad.frames[0].camera.provenance
        {
            *binding_id = "another-binding".into();
        }
        assert!(bad.validate().is_err());
        let mut bad = provenance.clone();
        let mut binding: serde_json::Value =
            serde_json::from_str(&bad.camera_binding.source_json).unwrap();
        binding["render_width"] = serde_json::json!(1280);
        bad.camera_binding.source_json = serde_json::to_string(&binding).unwrap();
        bad.camera_binding.sha256 = sha256_bytes(bad.camera_binding.source_json.as_bytes());
        assert!(
            bad.validate().is_err(),
            "rehashing a substituted snapshot cannot authorize it"
        );
        let mut bad = provenance.clone();
        bad.frames[0].depth.storage.transform = Some(StorageTransform::OneMinusD);
        assert!(bad.validate().is_err());
        let mut bad = provenance;
        bad.frames[0].depth.storage.domain = "view_space_meters".into();
        assert!(bad.validate().is_err());
    }
}
