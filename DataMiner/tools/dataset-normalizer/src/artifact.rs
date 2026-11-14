use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use capture_core::mapping::MappingSnapshot;
use capture_core::projection::{POINTER_CLAMP, foveated_bin};
use capture_core::qpc::frame_qpc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::hash::{sha256_bytes, sha256_file};
use crate::media::{MediaBackend, MediaIdentity, VideoProbe, probe_video};
use crate::model::{ARTIFACT_SCHEMA, FramePlanRow, NormalizedLabelRow};
use crate::plan::{PreparedNormalization, SourceEvidence, prepare};
use crate::spec::{LoadedSpec, RationalRate};
use crate::{TOOL_NAME, TOOL_VERSION};

const MANIFEST_SCHEMA: &str = "dm-normalized-manifest-1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationReport {
    pub session_id: String,
    pub spec_id: String,
    pub spec_sha256: String,
    pub frame_plan_sha256: String,
    pub frames: u64,
    pub runs: u32,
    pub duplicates: u64,
    pub trimmed_source_frames: u64,
    pub output: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReport {
    pub session_id: String,
    pub spec_id: String,
    pub spec_sha256: String,
    pub frame_plan_sha256: String,
    pub frames: u64,
    pub label_rows: u64,
    pub video_frames: u64,
    pub runs: u32,
    pub non_trainable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactMetadata {
    schema_version: String,
    session_id: String,
    tool: ToolMetadata,
    normalization_spec: SpecBinding,
    request: RequestBinding,
    mapping: BlobBinding,
    frame_plan: FileBinding,
    labels: FileBinding,
    media_backend: MediaIdentity,
    source: Vec<SourceEvidenceMetadata>,
    runs: Vec<RunMetadata>,
    totals: Totals,
    framepair_policy: FramePairDisclosure,
    is_trainable: bool,
    production_certified: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolMetadata {
    name: String,
    version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SpecBinding {
    spec_id: String,
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    schema_version: String,
    sha256: String,
    bytes: u64,
    source_fps: RationalRate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FileBinding {
    path: String,
    sha256: String,
    rows: u64,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BlobBinding {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceEvidenceMetadata {
    role: String,
    run_id: Option<String>,
    segment: Option<u32>,
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RunMetadata {
    run_id: String,
    qpc_anchor: i64,
    qpc_frequency: i64,
    plan_start: u64,
    frames: u64,
    video_path: String,
    video_sha256: String,
    video_bytes: u64,
    probe: VideoProbe,
    frame_plan_sha256: String,
    duplicate_frames: u64,
    source_frames: u64,
    trimmed_source_frames: u32,
    input_events: u64,
    covered_events: u64,
    trimmed_events: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Totals {
    frame_plan_rows: u64,
    label_rows: u64,
    video_frames: u64,
    duplicate_frames: u64,
    trimmed_source_frames: u64,
    input_events: u64,
    covered_events: u64,
    trimmed_events: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FramePairDisclosure {
    version: String,
    emitted: bool,
    owner_status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    tool: ToolMetadata,
    normalization_spec_id: String,
    normalization_spec_sha256: String,
    normalization_spec_bytes: u64,
    frame_plan_sha256: String,
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    sha256: String,
    bytes: u64,
}

pub fn normalize(
    request_path: impl AsRef<Path>,
    spec_path: impl AsRef<Path>,
    output: impl AsRef<Path>,
    media: &dyn MediaBackend,
) -> anyhow::Result<NormalizationReport> {
    let prepared = prepare(request_path, spec_path)?;
    let output = output.as_ref();
    ensure!(!output.exists(), "normalization output already exists");
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output parent {}", parent.display()))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .context("normalization output has no UTF-8 file name")?;
    let staging = parent.join(format!(".{name}.normalizing-{}", std::process::id()));
    ensure!(
        !staging.exists(),
        "normalization staging directory already exists"
    );
    fs::create_dir(&staging)
        .with_context(|| format!("failed to create staging directory {}", staging.display()))?;
    let result = write_artifact(&prepared, &staging, media).and_then(|report| {
        fs::rename(&staging, output).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                staging.display(),
                output.display()
            )
        })?;
        Ok(NormalizationReport {
            output: output.to_path_buf(),
            ..report
        })
    });
    if result.is_err() && staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn write_artifact(
    prepared: &PreparedNormalization,
    root: &Path,
    media: &dyn MediaBackend,
) -> anyhow::Result<NormalizationReport> {
    let spec_path = root.join("normalization-spec.json");
    let mapping_path = root.join("mapping.json");
    let plan_path = root.join("frame-plan.jsonl");
    let labels_path = root.join("labels.jsonl");
    write_exact(&spec_path, &prepared.spec.bytes)?;
    write_exact(&mapping_path, &prepared.mapping_bytes)?;
    write_exact(&plan_path, &prepared.plan_bytes)?;
    write_exact(&labels_path, &prepared.label_bytes)?;

    let media_identity = media.identity(&prepared.spec)?;
    ensure!(
        media_identity.settings_id == prepared.spec.spec.encoder.settings_id,
        "media backend settings id does not match the normalization spec"
    );
    let mut run_metadata = Vec::new();
    for run in &prepared.runs {
        let run_dir = root.join("runs").join(&run.run_id);
        fs::create_dir_all(&run_dir)?;
        let video_path = run_dir.join("video.mp4");
        let rows = &prepared.plan[run.plan_start..run.plan_start + run.plan_len];
        let probe = media.normalize_run(
            &prepared.spec,
            run,
            rows,
            prepared.request.source_width,
            prepared.request.source_height,
            &video_path,
        )?;
        ensure!(
            probe.sample_count as usize == rows.len(),
            "media backend output frame count does not match its plan range"
        );
        let duplicate_frames = rows.iter().filter(|row| row.duplicate).count() as u64;
        run_metadata.push(RunMetadata {
            run_id: run.run_id.clone(),
            qpc_anchor: run.qpc_anchor,
            qpc_frequency: run.qpc_frequency,
            plan_start: run.plan_start as u64,
            frames: run.plan_len as u64,
            video_path: normalized_path(root, &video_path)?,
            video_sha256: sha256_file(&video_path)?,
            video_bytes: fs::metadata(&video_path)?.len(),
            probe,
            frame_plan_sha256: prepared.plan_sha256.clone(),
            duplicate_frames,
            source_frames: run.source_frames as u64,
            trimmed_source_frames: run.trimmed_source_frames,
            input_events: run.input_events as u64,
            covered_events: run.covered_events as u64,
            trimmed_events: run.trimmed_events as u64,
        });
    }
    let totals = Totals {
        frame_plan_rows: prepared.plan.len() as u64,
        label_rows: prepared.labels.len() as u64,
        video_frames: run_metadata
            .iter()
            .map(|run| u64::from(run.probe.sample_count))
            .sum(),
        duplicate_frames: run_metadata.iter().map(|run| run.duplicate_frames).sum(),
        trimmed_source_frames: prepared
            .runs
            .iter()
            .map(|run| u64::from(run.trimmed_source_frames))
            .sum(),
        input_events: prepared
            .runs
            .iter()
            .map(|run| run.input_events as u64)
            .sum(),
        covered_events: prepared
            .runs
            .iter()
            .map(|run| run.covered_events as u64)
            .sum(),
        trimmed_events: prepared
            .runs
            .iter()
            .map(|run| run.trimmed_events as u64)
            .sum(),
    };
    ensure!(
        totals.frame_plan_rows == totals.label_rows && totals.label_rows == totals.video_frames,
        "video, label, and frame-plan counts do not close"
    );
    ensure!(
        totals.covered_events + totals.trimmed_events == totals.input_events,
        "Raw Input event accounting does not close"
    );
    let metadata = ArtifactMetadata {
        schema_version: ARTIFACT_SCHEMA.to_owned(),
        session_id: prepared.request.session_id.clone(),
        tool: tool_metadata(),
        normalization_spec: SpecBinding {
            spec_id: prepared.spec.spec.spec_id.clone(),
            path: "normalization-spec.json".to_owned(),
            sha256: prepared.spec.sha256.clone(),
            bytes: prepared.spec.bytes.len() as u64,
        },
        request: RequestBinding {
            schema_version: prepared.request.schema_version.clone(),
            sha256: prepared.request_sha256.clone(),
            bytes: prepared.request_bytes.len() as u64,
            source_fps: prepared.request.source_fps,
        },
        mapping: BlobBinding {
            path: "mapping.json".to_owned(),
            sha256: prepared.mapping_sha256.clone(),
            bytes: prepared.mapping_bytes.len() as u64,
        },
        frame_plan: FileBinding {
            path: "frame-plan.jsonl".to_owned(),
            sha256: prepared.plan_sha256.clone(),
            rows: prepared.plan.len() as u64,
            bytes: prepared.plan_bytes.len() as u64,
        },
        labels: FileBinding {
            path: "labels.jsonl".to_owned(),
            sha256: prepared.labels_sha256.clone(),
            rows: prepared.labels.len() as u64,
            bytes: prepared.label_bytes.len() as u64,
        },
        media_backend: media_identity,
        source: prepared
            .source_evidence
            .iter()
            .map(source_evidence_metadata)
            .collect(),
        runs: run_metadata,
        totals,
        framepair_policy: FramePairDisclosure {
            version: prepared.spec.spec.framepair_policy.version.clone(),
            emitted: false,
            owner_status: prepared.spec.spec.framepair_policy.owner_status.clone(),
        },
        is_trainable: false,
        production_certified: false,
    };
    let metadata_path = root.join("metadata.json");
    write_pretty_json(&metadata_path, &metadata)?;

    let files = regular_files(root)?
        .into_iter()
        .filter(|path| path != Path::new("MANIFEST.json"))
        .map(|relative| {
            let path = root.join(&relative);
            Ok((
                relative.to_string_lossy().replace('\\', "/"),
                ManifestFile {
                    sha256: sha256_file(&path)?,
                    bytes: fs::metadata(&path)?.len(),
                },
            ))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA.to_owned(),
        tool: tool_metadata(),
        normalization_spec_id: prepared.spec.spec.spec_id.clone(),
        normalization_spec_sha256: prepared.spec.sha256.clone(),
        normalization_spec_bytes: prepared.spec.bytes.len() as u64,
        frame_plan_sha256: prepared.plan_sha256.clone(),
        files,
    };
    write_pretty_json(&root.join("MANIFEST.json"), &manifest)?;
    let verified = verify(root)?;
    ensure!(
        verified.frames == prepared.plan.len() as u64,
        "fresh artifact did not independently verify"
    );
    Ok(NormalizationReport {
        session_id: prepared.request.session_id.clone(),
        spec_id: prepared.spec.spec.spec_id.clone(),
        spec_sha256: prepared.spec.sha256.clone(),
        frame_plan_sha256: prepared.plan_sha256.clone(),
        frames: prepared.plan.len() as u64,
        runs: prepared.runs.len() as u32,
        duplicates: metadata.totals.duplicate_frames,
        trimmed_source_frames: metadata.totals.trimmed_source_frames,
        output: root.to_path_buf(),
    })
}

pub fn verify(dataset: impl AsRef<Path>) -> anyhow::Result<VerificationReport> {
    let root = dataset.as_ref();
    ensure!(root.is_dir(), "normalized dataset directory is missing");
    ensure!(
        !root.join("framepairs.jsonl").exists(),
        "FramePair output exists before its owner policy is frozen"
    );
    let manifest: Manifest = read_json(&root.join("MANIFEST.json"))?;
    ensure!(
        manifest.schema_version == MANIFEST_SCHEMA,
        "unsupported normalized MANIFEST schema"
    );
    ensure!(
        manifest.tool == tool_metadata(),
        "normalized MANIFEST tool version does not match this verifier"
    );
    let actual_files = regular_files(root)?
        .into_iter()
        .filter(|path| path != Path::new("MANIFEST.json"))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<BTreeSet<_>>();
    let declared_files = manifest.files.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        actual_files == declared_files,
        "normalized MANIFEST file set does not match the dataset"
    );
    for (relative, declared) in &manifest.files {
        let path = safe_join(root, relative)?;
        ensure!(
            fs::metadata(&path)?.len() == declared.bytes && sha256_file(&path)? == declared.sha256,
            "normalized MANIFEST digest mismatch for {relative}"
        );
    }

    let metadata: ArtifactMetadata = read_json(&root.join("metadata.json"))?;
    ensure!(
        metadata.schema_version == ARTIFACT_SCHEMA,
        "unsupported normalized dataset schema"
    );
    ensure!(
        metadata.tool == tool_metadata(),
        "normalized metadata tool version does not match this verifier"
    );
    ensure!(
        !metadata.is_trainable
            && !metadata.production_certified
            && !metadata.framepair_policy.emitted
            && metadata.framepair_policy.owner_status == "pending",
        "normalized artifact overstates trainability or FramePair authorization"
    );
    let spec_path = safe_join(root, &metadata.normalization_spec.path)?;
    let spec_bytes = fs::read(&spec_path)?;
    let spec = LoadedSpec::from_bytes(&spec_path, spec_bytes)?;
    ensure!(
        spec.spec.spec_id == manifest.normalization_spec_id
            && spec.spec.spec_id == metadata.normalization_spec.spec_id
            && spec.sha256 == manifest.normalization_spec_sha256
            && spec.sha256 == metadata.normalization_spec.sha256
            && spec.bytes.len() as u64 == manifest.normalization_spec_bytes
            && spec.bytes.len() as u64 == metadata.normalization_spec.bytes,
        "normalization spec bytes/id/hash binding does not close"
    );
    ensure!(
        metadata.media_backend.settings_id == spec.spec.encoder.settings_id,
        "media implementation is not bound to the spec encoder version"
    );
    ensure!(
        metadata.request.source_fps == spec.spec.source,
        "metadata source fps does not exactly match the normalization spec"
    );
    let mapping_path = safe_join(root, &metadata.mapping.path)?;
    let mapping_bytes = fs::read(&mapping_path)?;
    ensure!(
        mapping_bytes.len() as u64 == metadata.mapping.bytes
            && sha256_bytes(&mapping_bytes) == metadata.mapping.sha256,
        "frozen mapping bytes/hash binding does not close"
    );
    MappingSnapshot::from_slice(&mapping_bytes)
        .context("frozen normalized mapping is not a valid mapping snapshot")?;

    let plan_path = safe_join(root, &metadata.frame_plan.path)?;
    let labels_path = safe_join(root, &metadata.labels.path)?;
    let plan: Vec<FramePlanRow> = read_jsonl(&plan_path)?;
    let labels: Vec<NormalizedLabelRow> = read_jsonl(&labels_path)?;
    ensure!(
        sha256_file(&plan_path)? == metadata.frame_plan.sha256
            && metadata.frame_plan.sha256 == manifest.frame_plan_sha256
            && fs::metadata(&plan_path)?.len() == metadata.frame_plan.bytes
            && sha256_file(&labels_path)? == metadata.labels.sha256
            && fs::metadata(&labels_path)?.len() == metadata.labels.bytes,
        "plan/label digest binding does not close"
    );
    ensure!(
        plan.len() as u64 == metadata.frame_plan.rows
            && labels.len() as u64 == metadata.labels.rows
            && plan.len() == labels.len(),
        "plan and label row counts do not close"
    );
    for (index, (plan, label)) in plan.iter().zip(&labels).enumerate() {
        ensure!(
            plan.out_frame == index as u64
                && label.out_frame == plan.out_frame
                && label.run_id == plan.run_id
                && label.run_frame == plan.run_frame
                && label.qpc_start == plan.qpc_start
                && label.qpc_end == plan.qpc_end
                && label.source_frame == plan.source_frame
                && label.source_qpc == plan.source_qpc
                && label.source_seq == plan.source_seq
                && label.duplicate == plan.duplicate
                && label.normalization_spec_sha256 == spec.sha256
                && label.frame_plan_sha256 == manifest.frame_plan_sha256,
            "label row {index} is not bound to its frame-plan row"
        );
        let (raw_dx, raw_dy) = label_pointer_raw_sum(label)?;
        ensure!(
            label.pointer_raw_dx == raw_dx
                && label.pointer_raw_dy == raw_dy
                && label.pointer_dx == clamp_pointer(raw_dx)
                && label.pointer_dy == clamp_pointer(raw_dy)
                && label.pointer_bins
                    == [
                        foveated_bin(label.pointer_dx),
                        foveated_bin(label.pointer_dy)
                    ],
            "label row {index} pointer fields are not one clamp/bin of the raw i64 sum"
        );
    }

    let mut video_frames = 0_u64;
    let mut expected_plan_start = 0_usize;
    for run in &metadata.runs {
        ensure!(
            run.plan_start as usize == expected_plan_start,
            "run plan ranges are not contiguous"
        );
        let end = expected_plan_start
            .checked_add(run.frames as usize)
            .context("run plan range overflow")?;
        ensure!(end <= plan.len(), "run plan range exceeds frame-plan");
        ensure!(run.qpc_frequency > 0, "run QPC frequency is not positive");
        let factor = u64::from(spec.spec.downsample_factor());
        let run_rows = &plan[expected_plan_start..end];
        ensure!(
            run_rows.iter().all(|row| row.run_id == run.run_id),
            "run plan range crosses a pause/resume boundary"
        );
        for (run_index, row) in run_rows.iter().enumerate() {
            ensure!(
                row.run_frame == run_index as u64
                    && row.qpc_start
                        == frame_qpc(
                            run.qpc_anchor,
                            run_index as u64,
                            run.qpc_frequency,
                            spec.spec.target.fps_num,
                            spec.spec.target.fps_den,
                        )
                    && row.qpc_end
                        == frame_qpc(
                            run.qpc_anchor,
                            run_index as u64 + 1,
                            run.qpc_frequency,
                            spec.spec.target.fps_num,
                            spec.spec.target.fps_den,
                        )
                    && row.source_frame == (run_index as u64 + 1) * factor - 1,
                "run frame-plan does not match the frozen QPC/source selector"
            );
            if let Some(previous) = run_index.checked_sub(1).and_then(|i| run_rows.get(i)) {
                ensure!(
                    row.duplicate
                        == ((row.source_qpc, row.source_seq)
                            == (previous.source_qpc, previous.source_seq)),
                    "run duplicate bookkeeping does not match source identity"
                );
            } else {
                ensure!(
                    !row.duplicate,
                    "first normalized run frame cannot be a duplicate"
                );
            }
        }
        let trimmed = run_rows
            .last()
            .context("run has no frame-plan rows")?
            .trimmed_tail_source_frames;
        ensure!(
            run.source_frames == run.frames * factor + u64::from(trimmed)
                && run.trimmed_source_frames == trimmed
                && run_rows.iter().filter(|row| row.trim).count() == usize::from(trimmed > 0)
                && run_rows
                    .iter()
                    .take(run_rows.len().saturating_sub(1))
                    .all(|row| !row.trim && row.trimmed_tail_source_frames == 0),
            "run tail trim bookkeeping does not close"
        );
        let video_path = safe_join(root, &run.video_path)?;
        ensure!(
            fs::metadata(&video_path)?.len() == run.video_bytes
                && sha256_file(&video_path)? == run.video_sha256,
            "run video digest binding does not close"
        );
        let probe = probe_video(&video_path, spec.spec.target_rate(), spec.spec.gop_frames())?;
        ensure!(
            probe == run.probe,
            "run video readback changed from metadata"
        );
        ensure!(
            u64::from(probe.sample_count) == run.frames,
            "run video frame count does not match plan range"
        );
        ensure!(
            run.frame_plan_sha256 == manifest.frame_plan_sha256,
            "run video is not bound to the global frame-plan hash"
        );
        ensure!(
            run.duplicate_frames == run_rows.iter().filter(|row| row.duplicate).count() as u64,
            "run duplicate total does not match frame-plan"
        );
        video_frames += u64::from(probe.sample_count);
        expected_plan_start = end;
    }
    ensure!(
        expected_plan_start == plan.len(),
        "run ranges do not cover frame-plan"
    );
    ensure!(
        metadata.totals.duplicate_frames
            == metadata
                .runs
                .iter()
                .map(|run| run.duplicate_frames)
                .sum::<u64>()
            && metadata.totals.trimmed_source_frames
                == metadata
                    .runs
                    .iter()
                    .map(|run| u64::from(run.trimmed_source_frames))
                    .sum::<u64>()
            && metadata.totals.input_events
                == metadata
                    .runs
                    .iter()
                    .map(|run| run.input_events)
                    .sum::<u64>()
            && metadata.totals.covered_events
                == metadata
                    .runs
                    .iter()
                    .map(|run| run.covered_events)
                    .sum::<u64>()
            && metadata.totals.trimmed_events
                == metadata
                    .runs
                    .iter()
                    .map(|run| run.trimmed_events)
                    .sum::<u64>(),
        "metadata run totals do not close"
    );
    ensure!(
        metadata.totals.frame_plan_rows == plan.len() as u64
            && metadata.totals.label_rows == labels.len() as u64
            && metadata.totals.video_frames == video_frames
            && plan.len() as u64 == video_frames,
        "video, label, and frame-plan totals do not close"
    );
    ensure!(
        metadata.totals.covered_events + metadata.totals.trimmed_events
            == metadata.totals.input_events,
        "Raw Input event totals do not close"
    );
    Ok(VerificationReport {
        session_id: metadata.session_id,
        spec_id: spec.spec.spec_id,
        spec_sha256: spec.sha256,
        frame_plan_sha256: manifest.frame_plan_sha256,
        frames: plan.len() as u64,
        label_rows: labels.len() as u64,
        video_frames,
        runs: metadata.runs.len() as u32,
        non_trainable: true,
    })
}

fn source_evidence_metadata(source: &SourceEvidence) -> SourceEvidenceMetadata {
    SourceEvidenceMetadata {
        role: source.role.clone(),
        run_id: source.run_id.clone(),
        segment: source.segment,
        path: source.path.clone(),
        sha256: source.sha256.clone(),
        bytes: source.bytes,
    }
}

fn tool_metadata() -> ToolMetadata {
    ToolMetadata {
        name: TOOL_NAME.to_owned(),
        version: TOOL_VERSION.to_owned(),
    }
}

fn write_exact(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn write_pretty_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_exact(path, &bytes)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("invalid JSON {}", path.display()))
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        ensure!(
            !line.trim().is_empty(),
            "blank JSONL row at line {}",
            index + 1
        );
        rows.push(
            serde_json::from_str(&line).with_context(|| {
                format!("invalid JSONL row {} in {}", index + 1, path.display())
            })?,
        );
    }
    Ok(rows)
}

fn regular_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "normalized artifact contains a symlink");
        if kind.is_dir() {
            collect_files(root, &path, files)?;
        } else if kind.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        } else {
            anyhow::bail!("normalized artifact contains a non-file entry");
        }
    }
    Ok(())
}

fn normalized_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    Ok(path
        .strip_prefix(root)
        .context("artifact path escapes output root")?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn safe_join(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(relative);
    ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "artifact relative path is unsafe"
    );
    Ok(root.join(relative))
}

fn label_pointer_raw_sum(label: &NormalizedLabelRow) -> anyhow::Result<(i64, i64)> {
    let mut dx = 0_i64;
    let mut dy = 0_i64;
    for event in &label.events {
        if event.event_type == "mouse_move" {
            dx = dx
                .checked_add(i64::from(
                    event.dx.context("normalized mouse_move has no dx")?,
                ))
                .context("normalized pointer dx overflows i64")?;
            dy = dy
                .checked_add(i64::from(
                    event.dy.context("normalized mouse_move has no dy")?,
                ))
                .context("normalized pointer dy overflows i64")?;
        }
    }
    Ok((dx, dy))
}

fn clamp_pointer(value: i64) -> i32 {
    value.clamp(i64::from(-POINTER_CLAMP), i64::from(POINTER_CLAMP)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{FfmpegBackend, MediaBackend, validate_source_video};
    use crate::model::SourceFrameTiming;
    use crate::plan::PreparedRun;
    use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};
    use std::io::BufWriter;
    use std::process::Command;
    use tempfile::TempDir;

    const MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.artifact-fixture.pc",
      "version":"1",
      "game":{"id":"artifact-fixture","name":"Artifact Fixture","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"artifact-fixture-recording-v1",
      "bindings":[
        {"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"},
        {"device":"mouse","control":"delta_x","semantic":"camera.yaw"}
      ]
    }"#;

    #[derive(Clone, Copy)]
    struct FixtureMedia;

    impl MediaBackend for FixtureMedia {
        fn identity(&self, spec: &LoadedSpec) -> anyhow::Result<MediaIdentity> {
            Ok(MediaIdentity {
                implementation: "test_fixture_backend".into(),
                version: "fixture-media-v1".into(),
                binary_sha256: format!("sha256:{}", "0".repeat(64)),
                settings_id: spec.spec.encoder.settings_id.clone(),
            })
        }

        fn normalize_run(
            &self,
            spec: &LoadedSpec,
            run: &PreparedRun,
            plan: &[FramePlanRow],
            source_width: u32,
            source_height: u32,
            output: &Path,
        ) -> anyhow::Result<VideoProbe> {
            ensure!(plan.len() == run.plan_len, "fixture plan mismatch");
            fs::create_dir_all(output.parent().unwrap())?;
            write_fixture_mp4(
                output,
                plan.len(),
                spec.spec.target_rate(),
                source_width as u16,
                source_height as u16,
                spec.spec.gop_frames(),
                None,
            );
            probe_video(output, spec.spec.target_rate(), spec.spec.gop_frames())
        }
    }

    fn spec(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("specs")
            .join(name)
    }

    fn write_jsonl(path: &Path, rows: &[impl Serialize]) {
        let mut file = File::create(path).unwrap();
        for row in rows {
            serde_json::to_writer(&mut file, row).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    fn request_fixture(temp: &TempDir, frames: usize) -> PathBuf {
        let root = temp.path();
        fs::write(root.join("mapping.json"), MAPPING).unwrap();
        let timing = (0..frames)
            .map(|frame| SourceFrameTiming {
                frame: frame as u64,
                session_frame: frame as u64,
                output_qpc_start: 1000 + frame as i64,
                output_qpc_end: 1001 + frame as i64,
                source_qpc: 1000 + frame as i64,
                source_seq: frame as u64 + 1,
                present_seq: None,
                duplicate: false,
                source_superseded: 0,
                queue_drop: 0,
            })
            .collect::<Vec<_>>();
        write_jsonl(&root.join("timing.jsonl"), &timing);
        let no_events: Vec<serde_json::Value> = Vec::new();
        write_jsonl(&root.join("events.jsonl"), &no_events);
        fs::write(root.join("source.mp4"), b"fixture source video identity").unwrap();
        let request = serde_json::json!({
            "schema_version": "dm-normalization-request-1",
            "session_id": "artifact-fixture-session",
            "source_fps": {"fps_num": 60, "fps_den": 1},
            "source_width": 1920,
            "source_height": 1080,
            "mapping_path": "mapping.json",
            "runs": [{
                "run_id": "r0000",
                "qpc_anchor": 1000,
                "qpc_frequency": 60,
                "segments": [{
                    "timing_path": "timing.jsonl",
                    "events_path": "events.jsonl",
                    "video_path": "source.mp4"
                }]
            }]
        });
        let path = root.join("request.json");
        fs::write(&path, serde_json::to_vec_pretty(&request).unwrap()).unwrap();
        path
    }

    fn write_fixture_mp4(
        path: &Path,
        frames: usize,
        rate: RationalRate,
        width: u16,
        height: u16,
        gop: u32,
        override_sync: Option<&[u32]>,
    ) {
        let timescale = 90_000;
        let duration = timescale * rate.fps_den / rate.fps_num;
        write_fixture_mp4_with_durations(
            path,
            timescale,
            &vec![duration; frames],
            width,
            height,
            gop,
            override_sync,
        );
    }

    fn write_fixture_mp4_with_durations(
        path: &Path,
        timescale: u32,
        durations: &[u32],
        width: u16,
        height: u16,
        gop: u32,
        override_sync: Option<&[u32]>,
    ) {
        let output = BufWriter::new(File::create(path).unwrap());
        let mut writer = Mp4Writer::write_start(
            output,
            &Mp4Config {
                major_brand: "isom".parse().unwrap(),
                minor_version: 512,
                compatible_brands: vec!["isom".parse().unwrap(), "avc1".parse().unwrap()],
                timescale: 1_000,
            },
        )
        .unwrap();
        writer
            .add_track(&TrackConfig {
                track_type: TrackType::Video,
                timescale,
                language: "und".into(),
                media_conf: MediaConfig::AvcConfig(AvcConfig {
                    width,
                    height,
                    seq_param_set: vec![0x67, 100, 0, 31, 0xac, 0xd9, 0x40],
                    pic_param_set: vec![0x68, 0xee, 0x3c, 0x80],
                }),
            })
            .unwrap();
        let sync = override_sync
            .map(<[_]>::to_vec)
            .unwrap_or_else(|| (0..durations.len() as u32).step_by(gop as usize).collect());
        let mut start_time = 0_u64;
        for (frame, duration) in durations.iter().copied().enumerate() {
            let is_sync = sync.contains(&(frame as u32));
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time,
                        duration,
                        rendering_offset: 0,
                        is_sync,
                        bytes: vec![0, 0, 0, 1, if is_sync { 0x65 } else { 0x41 }].into(),
                    },
                )
                .unwrap();
            start_time += u64::from(duration);
        }
        writer.write_end().unwrap();
        let mut output = writer.into_writer();
        output.flush().unwrap();
    }

    #[test]
    fn source_video_probe_requires_exact_60_cfr_sample_durations() {
        let temp = TempDir::new().unwrap();
        let exact = temp.path().join("exact-60.mp4");
        write_fixture_mp4_with_durations(&exact, 60_000, &[1_000; 6], 64, 64, 120, None);
        validate_source_video(
            &exact,
            6,
            64,
            64,
            RationalRate {
                fps_num: 60,
                fps_den: 1,
            },
        )
        .unwrap();

        for (name, timescale, durations) in [
            ("fps-5994.mp4", 60_000, vec![1_001; 6]),
            ("fps-30.mp4", 60_000, vec![2_000; 6]),
            (
                "vfr.mp4",
                60_000,
                vec![1_000, 1_000, 2_000, 1_000, 1_000, 1_000],
            ),
        ] {
            let path = temp.path().join(name);
            write_fixture_mp4_with_durations(&path, timescale, &durations, 64, 64, 120, None);
            let error = validate_source_video(
                &path,
                6,
                64,
                64,
                RationalRate {
                    fps_num: 60,
                    fps_den: 1,
                },
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("does not match exact source fps 60/1"),
                "unexpected rejection for {name}: {error}"
            );
        }
    }

    #[test]
    #[ignore = "requires DATAMINER_TEST_FFMPEG and DATAMINER_TEST_FFMPEG_SHA256"]
    fn pinned_ffmpeg_worker_closes_all_targets_and_is_deterministic() -> anyhow::Result<()> {
        let executable = std::env::var_os("DATAMINER_TEST_FFMPEG")
            .context("DATAMINER_TEST_FFMPEG is required")?;
        let expected_sha256 = std::env::var("DATAMINER_TEST_FFMPEG_SHA256")
            .context("DATAMINER_TEST_FFMPEG_SHA256 is required")?;
        let backend = FfmpegBackend::pinned(&executable, &expected_sha256)?;
        let temp = TempDir::new()?;
        let request = request_fixture(&temp, 61);
        let mut request_value: serde_json::Value = serde_json::from_slice(&fs::read(&request)?)?;
        request_value["source_width"] = serde_json::json!(64);
        request_value["source_height"] = serde_json::json!(64);
        fs::write(&request, serde_json::to_vec_pretty(&request_value)?)?;

        let source = temp.path().join("source.mp4");
        let generated = Command::new(&executable)
            .args([
                "-hide_banner",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x64:rate=60",
                "-frames:v",
                "61",
                "-an",
                "-fps_mode",
                "cfr",
                "-r",
                "60/1",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-preset",
                "medium",
                "-crf",
                "18",
                "-threads",
                "1",
                "-g",
                "240",
                "-keyint_min",
                "240",
                "-sc_threshold",
                "0",
                "-bf",
                "0",
                "-video_track_timescale",
                "60000",
                "-map_metadata",
                "-1",
                "-map_chapters",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags:v",
                "+bitexact",
                "-movflags",
                "+faststart",
            ])
            .arg(&source)
            .output()
            .context("failed to generate the exact-60/1 FFmpeg fixture")?;
        ensure!(
            generated.status.success(),
            "pinned FFmpeg fixture generation failed: {}",
            String::from_utf8_lossy(&generated.stderr).trim()
        );

        for (spec_name, expected_frames) in [
            ("fps60-to-training30-v1.json", 30_u64),
            ("fps60-to-training20-v1.json", 20),
            ("fps60-to-training12-v1.json", 12),
        ] {
            let mut first_hashes = None;
            for replay in 0..2 {
                let output = temp.path().join(format!("{spec_name}-normalized-{replay}"));
                let report = normalize(&request, spec(spec_name), &output, &backend)?;
                let verified = verify(&output)?;
                ensure!(
                    report.frames == expected_frames
                        && report.trimmed_source_frames == 1
                        && verified.frames == expected_frames
                        && verified.video_frames == expected_frames
                        && verified.label_rows == expected_frames,
                    "real FFmpeg normalization counts do not close for {spec_name}"
                );
                let hashes = [
                    "frame-plan.jsonl",
                    "labels.jsonl",
                    "metadata.json",
                    "MANIFEST.json",
                    "runs/r0000/video.mp4",
                ]
                .map(|relative| sha256_file(&output.join(relative)))
                .into_iter()
                .collect::<anyhow::Result<Vec<_>>>()?;
                if let Some(first) = &first_hashes {
                    ensure!(
                        first == &hashes,
                        "real FFmpeg replay is not deterministic for {spec_name}"
                    );
                } else {
                    first_hashes = Some(hashes);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn artifact_matrix_closes_video_labels_plan_and_preserves_exact_spec_bytes() {
        for (name, factor) in [
            ("fps60-to-training30-v1.json", 2_usize),
            ("fps60-to-training20-v1.json", 3),
            ("fps60-to-training12-v1.json", 5),
        ] {
            let temp = TempDir::new().unwrap();
            let request = request_fixture(&temp, factor * 4 + 1);
            let output = temp.path().join("normalized");
            let report = normalize(&request, spec(name), &output, &FixtureMedia).unwrap();
            let verified = verify(&output).unwrap();
            assert_eq!(report.frames, 4);
            assert_eq!(verified.frames, 4);
            assert_eq!(verified.label_rows, 4);
            assert_eq!(verified.video_frames, 4);
            assert!(verified.non_trainable);
            assert_eq!(
                fs::read(output.join("normalization-spec.json")).unwrap(),
                fs::read(spec(name)).unwrap()
            );
            assert!(!output.join("framepairs.jsonl").exists());
        }
    }

    #[test]
    fn identical_input_spec_and_tool_produce_identical_artifact_hashes() {
        let temp = TempDir::new().unwrap();
        let request = request_fixture(&temp, 9);
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        normalize(
            &request,
            spec("fps60-to-training30-v1.json"),
            &first,
            &FixtureMedia,
        )
        .unwrap();
        normalize(
            &request,
            spec("fps60-to-training30-v1.json"),
            &second,
            &FixtureMedia,
        )
        .unwrap();
        for relative in [
            "normalization-spec.json",
            "mapping.json",
            "frame-plan.jsonl",
            "labels.jsonl",
            "runs/r0000/video.mp4",
            "metadata.json",
            "MANIFEST.json",
        ] {
            assert_eq!(
                sha256_file(&first.join(relative)).unwrap(),
                sha256_file(&second.join(relative)).unwrap(),
                "artifact drifted at {relative}"
            );
        }
    }

    #[test]
    fn independent_verify_rejects_tampering_and_unauthorized_framepairs() {
        let temp = TempDir::new().unwrap();
        let request = request_fixture(&temp, 8);
        let output = temp.path().join("normalized");
        normalize(
            &request,
            spec("fps60-to-training30-v1.json"),
            &output,
            &FixtureMedia,
        )
        .unwrap();
        fs::write(output.join("framepairs.jsonl"), b"{}\n").unwrap();
        assert!(
            verify(&output)
                .unwrap_err()
                .to_string()
                .contains("FramePair output exists")
        );
        fs::remove_file(output.join("framepairs.jsonl")).unwrap();
        fs::write(output.join("labels.jsonl"), b"{}\n").unwrap();
        assert!(
            verify(&output)
                .unwrap_err()
                .to_string()
                .contains("MANIFEST digest mismatch")
        );
    }

    #[test]
    fn video_probe_rejects_an_extra_idr_outside_the_frozen_gop() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("bad-gop.mp4");
        write_fixture_mp4(
            &path,
            4,
            RationalRate {
                fps_num: 30,
                fps_den: 1,
            },
            1920,
            1080,
            120,
            Some(&[0, 2]),
        );
        assert!(
            probe_video(
                &path,
                RationalRate {
                    fps_num: 30,
                    fps_den: 1,
                },
                120,
            )
            .unwrap_err()
            .to_string()
            .contains("GOP/IDR schedule")
        );
    }
}
