use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use capture_core::input::RawInputEvent;
use capture_core::mapping::MappingSnapshot;
use capture_core::profile::CaptureProfile;
use capture_core::projection::{FrameEvent, FrameProjector, FrameTiming, POINTER_CLAMP};
use capture_core::qpc::frame_qpc;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::hash::{sha256_bytes, sha256_file};
use crate::model::{
    FramePlanRow, NormalizationRequest, NormalizedEvent, NormalizedLabelRow, REQUEST_SCHEMA,
    SourceFrameTiming,
};
use crate::spec::{LoadedSpec, RationalRate};

#[derive(Clone, Debug)]
pub struct PreparedRun {
    pub run_id: String,
    pub qpc_anchor: i64,
    pub qpc_frequency: i64,
    pub plan_start: usize,
    pub plan_len: usize,
    pub source_frames: usize,
    pub trimmed_source_frames: u32,
    pub input_events: usize,
    pub covered_events: usize,
    pub trimmed_events: usize,
    pub source_videos: Vec<PathBuf>,
    pub source_video_frames: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct SourceEvidence {
    pub role: String,
    pub run_id: Option<String>,
    pub segment: Option<u32>,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct PreparedNormalization {
    pub request_path: PathBuf,
    pub request_bytes: Vec<u8>,
    pub request_sha256: String,
    pub request: NormalizationRequest,
    pub spec: LoadedSpec,
    pub mapping_bytes: Vec<u8>,
    pub mapping_sha256: String,
    pub plan: Vec<FramePlanRow>,
    pub plan_bytes: Vec<u8>,
    pub plan_sha256: String,
    pub labels: Vec<NormalizedLabelRow>,
    pub label_bytes: Vec<u8>,
    pub labels_sha256: String,
    pub runs: Vec<PreparedRun>,
    pub source_evidence: Vec<SourceEvidence>,
}

#[derive(Clone, Debug)]
struct SourceSlot {
    timing: SourceFrameTiming,
    segment: u32,
}

#[derive(Clone, Debug)]
struct LoadedRun {
    run_id: String,
    anchor: i64,
    frequency: i64,
    source: Vec<SourceSlot>,
    events: Vec<RawInputEvent>,
    source_videos: Vec<PathBuf>,
    source_video_frames: Vec<u32>,
}

pub fn prepare(
    request_path: impl AsRef<Path>,
    spec_path: impl AsRef<Path>,
) -> anyhow::Result<PreparedNormalization> {
    let request_path = request_path.as_ref();
    let request_bytes = fs::read(request_path)
        .with_context(|| format!("failed to read request {}", request_path.display()))?;
    let request: NormalizationRequest = serde_json::from_slice(&request_bytes)
        .with_context(|| format!("invalid request {}", request_path.display()))?;
    let spec = LoadedSpec::load(spec_path)?;
    prepare_loaded(request_path, request_bytes, request, spec)
}

fn prepare_loaded(
    request_path: &Path,
    request_bytes: Vec<u8>,
    request: NormalizationRequest,
    spec: LoadedSpec,
) -> anyhow::Result<PreparedNormalization> {
    validate_request(&request, &spec)?;
    let base = request_path.parent().unwrap_or_else(|| Path::new("."));
    let mapping_path = resolve(base, &request.mapping_path);
    let mapping_bytes = fs::read(&mapping_path)
        .with_context(|| format!("failed to read mapping {}", mapping_path.display()))?;
    let mapping = MappingSnapshot::from_slice(&mapping_bytes)?;
    let mapping_sha256 = sha256_bytes(&mapping_bytes);

    let mut loaded_runs = Vec::with_capacity(request.runs.len());
    let mut evidence = vec![source_evidence(
        "mapping",
        None,
        None,
        &request.mapping_path,
        &mapping_path,
    )?];
    for run in &request.runs {
        loaded_runs.push(load_run(base, run, &mut evidence)?);
    }

    let (plan, run_summaries) = build_plan(&loaded_runs, &spec)?;
    let plan_bytes = jsonl_bytes(&plan)?;
    let plan_sha256 = sha256_bytes(&plan_bytes);
    let labels = project_labels(
        &request,
        &spec,
        &mapping,
        &loaded_runs,
        &run_summaries,
        &plan,
        &plan_sha256,
    )?;
    ensure!(
        labels.len() == plan.len(),
        "label rows do not match frame-plan rows"
    );
    let label_bytes = jsonl_bytes(&labels)?;
    let labels_sha256 = sha256_bytes(&label_bytes);

    Ok(PreparedNormalization {
        request_path: request_path.to_path_buf(),
        request_sha256: sha256_bytes(&request_bytes),
        request_bytes,
        request,
        spec,
        mapping_bytes,
        mapping_sha256,
        plan,
        plan_bytes,
        plan_sha256,
        labels,
        label_bytes,
        labels_sha256,
        runs: run_summaries,
        source_evidence: evidence,
    })
}

fn validate_request(request: &NormalizationRequest, spec: &LoadedSpec) -> anyhow::Result<()> {
    ensure!(
        request.schema_version == REQUEST_SCHEMA,
        "unsupported normalization request schema {}",
        request.schema_version
    );
    ensure!(
        request.source_fps == spec.spec.source,
        "request source fps does not exactly match the normalization spec"
    );
    ensure!(
        request.source_fps
            == (RationalRate {
                fps_num: 60,
                fps_den: 1
            }),
        "normalization v1 rejects source fps other than exact 60/1"
    );
    ensure!(
        !request.session_id.trim().is_empty(),
        "request session id is empty"
    );
    ensure!(
        request.source_width > 0
            && request.source_height > 0
            && request.source_width.is_multiple_of(2)
            && request.source_height.is_multiple_of(2),
        "source dimensions must be positive and even"
    );
    ensure!(!request.runs.is_empty(), "request has no runs");
    let mut run_ids = BTreeSet::new();
    for run in &request.runs {
        ensure_safe_component(&run.run_id, "run id")?;
        ensure!(
            run_ids.insert(&run.run_id),
            "duplicate run id {}",
            run.run_id
        );
        ensure!(run.qpc_frequency > 0, "run QPC frequency must be positive");
        ensure!(
            !run.segments.is_empty(),
            "run {} has no segments",
            run.run_id
        );
    }
    Ok(())
}

fn load_run(
    base: &Path,
    run: &crate::model::RequestRun,
    evidence: &mut Vec<SourceEvidence>,
) -> anyhow::Result<LoadedRun> {
    let mut source = Vec::new();
    let mut events = Vec::new();
    let mut source_videos = Vec::new();
    let mut source_video_frames = Vec::new();
    for (segment_index, segment) in run.segments.iter().enumerate() {
        let segment_index = u32::try_from(segment_index).context("segment index exceeds u32")?;
        let timing_path = resolve(base, &segment.timing_path);
        let events_path = resolve(base, &segment.events_path);
        let video_path = resolve(base, &segment.video_path);
        ensure!(
            video_path.is_file(),
            "source video {} is missing",
            video_path.display()
        );

        let timing: Vec<SourceFrameTiming> = read_jsonl(&timing_path)?;
        ensure!(
            !timing.is_empty(),
            "run {} segment {segment_index} has no source timing rows",
            run.run_id
        );
        let segment_start = timing[0].output_qpc_start;
        let segment_end = timing.last().unwrap().output_qpc_end;
        for (index, row) in timing.into_iter().enumerate() {
            ensure!(
                row.frame == index as u64,
                "run {} segment {segment_index} source frame index is not local",
                run.run_id
            );
            source.push(SourceSlot {
                timing: row,
                segment: segment_index,
            });
        }

        let segment_events: Vec<RawInputEvent> = read_jsonl(&events_path)?;
        for event in &segment_events {
            ensure!(
                event.run_id == run.run_id,
                "event run id does not match request run {}",
                run.run_id
            );
            ensure!(
                event.qpc >= segment_start && event.qpc < segment_end,
                "event is outside its source segment QPC range"
            );
        }
        source_video_frames.push(
            u32::try_from(
                source
                    .iter()
                    .rev()
                    .take_while(|row| row.segment == segment_index)
                    .count(),
            )
            .context("source segment frame count exceeds u32")?,
        );
        events.extend(segment_events);
        source_videos.push(video_path.clone());
        evidence.push(source_evidence(
            "timing",
            Some(&run.run_id),
            Some(segment_index),
            &segment.timing_path,
            &timing_path,
        )?);
        evidence.push(source_evidence(
            "events",
            Some(&run.run_id),
            Some(segment_index),
            &segment.events_path,
            &events_path,
        )?);
        evidence.push(source_evidence(
            "video",
            Some(&run.run_id),
            Some(segment_index),
            &segment.video_path,
            &video_path,
        )?);
    }
    validate_source_timeline(run, &source)?;
    validate_event_order(run, &events, &source)?;
    Ok(LoadedRun {
        run_id: run.run_id.clone(),
        anchor: run.qpc_anchor,
        frequency: run.qpc_frequency,
        source,
        events,
        source_videos,
        source_video_frames,
    })
}

fn validate_source_timeline(
    run: &crate::model::RequestRun,
    source: &[SourceSlot],
) -> anyhow::Result<()> {
    ensure!(
        !source.is_empty(),
        "run {} has no source frames",
        run.run_id
    );
    for (index, slot) in source.iter().enumerate() {
        let index = index as u64;
        let row = &slot.timing;
        ensure!(
            row.output_qpc_start == frame_qpc(run.qpc_anchor, index, run.qpc_frequency, 60, 1)
                && row.output_qpc_end
                    == frame_qpc(run.qpc_anchor, index + 1, run.qpc_frequency, 60, 1),
            "run {} source frame {index} is not on the exact 60/1 QPC grid",
            run.run_id
        );
        ensure!(row.queue_drop == 0, "source timing records a queue drop");
        if let Some(previous) = index.checked_sub(1).and_then(|i| source.get(i as usize)) {
            ensure!(
                row.session_frame == previous.timing.session_frame + 1,
                "source session_frame is not monotonic"
            );
            if row.duplicate {
                ensure!(
                    row.source_qpc == previous.timing.source_qpc
                        && row.source_seq == previous.timing.source_seq,
                    "source duplicate does not reference its previous source"
                );
            } else {
                ensure!(
                    row.source_qpc > previous.timing.source_qpc
                        && row.source_seq > previous.timing.source_seq,
                    "unique source frame identity is not monotonic"
                );
            }
        } else {
            ensure!(
                !row.duplicate,
                "the first run source frame cannot be a duplicate"
            );
        }
    }
    Ok(())
}

fn validate_event_order(
    run: &crate::model::RequestRun,
    events: &[RawInputEvent],
    source: &[SourceSlot],
) -> anyhow::Result<()> {
    let start = source.first().unwrap().timing.output_qpc_start;
    let end = source.last().unwrap().timing.output_qpc_end;
    ensure!(
        start == run.qpc_anchor,
        "run source timing does not start at its QPC anchor"
    );
    for (index, event) in events.iter().enumerate() {
        ensure!(
            event.qpc >= start && event.qpc < end,
            "Raw Input event is outside the run timing range"
        );
        if let Some(previous) = index.checked_sub(1).and_then(|i| events.get(i)) {
            ensure!(
                (event.qpc, event.seq) > (previous.qpc, previous.seq),
                "Raw Input events are not ordered by QPC and sequence"
            );
        }
    }
    Ok(())
}

fn build_plan(
    runs: &[LoadedRun],
    spec: &LoadedSpec,
) -> anyhow::Result<(Vec<FramePlanRow>, Vec<PreparedRun>)> {
    let factor = usize::try_from(spec.spec.downsample_factor()).unwrap();
    let mut plan = Vec::new();
    let mut summaries = Vec::new();
    let mut next_out_frame = 0_u64;
    for run in runs {
        let plan_start = plan.len();
        let output_frames = run.source.len() / factor;
        ensure!(
            output_frames > 0,
            "run {} has no complete target window",
            run.run_id
        );
        let trimmed = u32::try_from(run.source.len() % factor).unwrap();
        let mut previous_source: Option<(i64, u64)> = None;
        for run_frame in 0..output_frames {
            let selected_index = (run_frame + 1) * factor - 1;
            let selected = &run.source[selected_index];
            let qpc_start = frame_qpc(
                run.anchor,
                run_frame as u64,
                run.frequency,
                spec.spec.target.fps_num,
                spec.spec.target.fps_den,
            );
            let qpc_end = frame_qpc(
                run.anchor,
                run_frame as u64 + 1,
                run.frequency,
                spec.spec.target.fps_num,
                spec.spec.target.fps_den,
            );
            ensure!(
                run.source[run_frame * factor].timing.output_qpc_start == qpc_start
                    && selected.timing.output_qpc_end == qpc_end,
                "target window does not align with its exact source CFR slots"
            );
            let identity = (selected.timing.source_qpc, selected.timing.source_seq);
            let duplicate = previous_source == Some(identity);
            if duplicate {
                ensure!(
                    selected.timing.duplicate,
                    "normalized duplicate is not marked in source timing"
                );
            }
            previous_source = Some(identity);
            let is_last = run_frame + 1 == output_frames;
            plan.push(FramePlanRow {
                out_frame: next_out_frame,
                run_id: run.run_id.clone(),
                run_frame: run_frame as u64,
                qpc_start,
                qpc_end,
                source_frame: selected_index as u64,
                source_segment: selected.segment,
                source_segment_frame: selected.timing.frame,
                source_qpc: selected.timing.source_qpc,
                source_seq: selected.timing.source_seq,
                present_seq: selected.timing.present_seq,
                duplicate,
                source_superseded: selected.timing.source_superseded,
                trim: is_last && trimmed > 0,
                trimmed_tail_source_frames: if is_last { trimmed } else { 0 },
            });
            next_out_frame = next_out_frame
                .checked_add(1)
                .context("normalized output frame index overflow")?;
        }
        let covered_end = plan.last().unwrap().qpc_end;
        let covered_events = run.events.partition_point(|event| event.qpc < covered_end);
        summaries.push(PreparedRun {
            run_id: run.run_id.clone(),
            qpc_anchor: run.anchor,
            qpc_frequency: run.frequency,
            plan_start,
            plan_len: output_frames,
            source_frames: run.source.len(),
            trimmed_source_frames: trimmed,
            input_events: run.events.len(),
            covered_events,
            trimmed_events: run.events.len() - covered_events,
            source_videos: run.source_videos.clone(),
            source_video_frames: run.source_video_frames.clone(),
        });
    }
    validate_plan(&plan, &summaries)?;
    Ok((plan, summaries))
}

fn validate_plan(plan: &[FramePlanRow], runs: &[PreparedRun]) -> anyhow::Result<()> {
    ensure!(!plan.is_empty(), "frame-plan is empty");
    for (index, row) in plan.iter().enumerate() {
        ensure!(row.out_frame == index as u64, "out_frame is not contiguous");
        ensure!(row.qpc_end > row.qpc_start, "frame-plan window is empty");
        if let Some(previous) = index.checked_sub(1).and_then(|i| plan.get(i))
            && previous.run_id == row.run_id
        {
            ensure!(
                row.run_frame == previous.run_frame + 1,
                "run_frame is not monotonic"
            );
            ensure!(
                row.qpc_start == previous.qpc_end,
                "frame-plan windows overlap or leave a hole"
            );
            ensure!(
                row.source_frame > previous.source_frame,
                "representative source CFR slot is not monotonic"
            );
            ensure!(
                row.duplicate
                    == ((row.source_qpc, row.source_seq)
                        == (previous.source_qpc, previous.source_seq)),
                "normalized duplicate accounting does not match source identity"
            );
        }
    }
    for run in runs {
        let rows = &plan[run.plan_start..run.plan_start + run.plan_len];
        ensure!(
            rows[0].run_frame == 0,
            "run frame-plan does not restart at zero"
        );
        ensure!(
            rows.iter().all(|row| row.run_id == run.run_id),
            "run frame-plan range crosses a pause/resume boundary"
        );
        ensure!(
            rows.iter().filter(|row| row.trim).count() <= 1
                && rows.iter().find(|row| row.trim).is_none_or(|row| {
                    row.run_frame + 1 == rows.len() as u64
                        && row.trimmed_tail_source_frames == run.trimmed_source_frames
                }),
            "trim marker is not the final row of its run"
        );
    }
    Ok(())
}

fn project_labels(
    request: &NormalizationRequest,
    spec: &LoadedSpec,
    mapping: &MappingSnapshot,
    loaded_runs: &[LoadedRun],
    summaries: &[PreparedRun],
    plan: &[FramePlanRow],
    plan_sha256: &str,
) -> anyhow::Result<Vec<NormalizedLabelRow>> {
    let projection_profile = projection_profile(request, spec);
    let mut labels = Vec::with_capacity(plan.len());
    for (run, summary) in loaded_runs.iter().zip(summaries) {
        let plan_rows = &plan[summary.plan_start..summary.plan_start + summary.plan_len];
        let timing = plan_rows
            .iter()
            .map(|row| FrameTiming {
                frame: row.run_frame,
                session_frame: row.out_frame,
                output_qpc_start: row.qpc_start,
                output_qpc_end: row.qpc_end,
                source_qpc: row.source_qpc,
                source_seq: row.source_seq,
                present_seq: row.present_seq,
                duplicate: row.duplicate,
                source_superseded: row.source_superseded,
                queue_drop: 0,
            })
            .collect::<Vec<_>>();
        let covered_events = &run.events[..summary.covered_events];
        ensure!(
            summary.covered_events + summary.trimmed_events == summary.input_events,
            "Raw Input event partition is not conservative"
        );
        let projected = FrameProjector::default().project_segment(
            &timing,
            covered_events,
            &run.run_id,
            &projection_profile,
            mapping.mapping(),
        )?;
        ensure!(
            projected.len() == plan_rows.len(),
            "projected row count mismatch"
        );
        for (row, projected) in plan_rows.iter().zip(projected) {
            ensure!(
                projected.output_qpc_start == row.qpc_start
                    && projected.output_qpc_end == row.qpc_end
                    && projected.source_qpc == row.source_qpc
                    && projected.source_seq == row.source_seq
                    && projected.duplicate == row.duplicate,
                "projection does not consume the frame-plan timing identity"
            );
            let (raw_dx, raw_dy) = pointer_raw_sum(&projected.events)?;
            ensure!(
                projected.pointer_dx == clamp_pointer(raw_dx)
                    && projected.pointer_dy == clamp_pointer(raw_dy),
                "FrameProjector pointer output is not a single clamp of the raw i64 sum"
            );
            labels.push(NormalizedLabelRow {
                out_frame: row.out_frame,
                run_id: row.run_id.clone(),
                run_frame: row.run_frame,
                video_t_us: projected.video_t_us,
                qpc_start: row.qpc_start,
                qpc_end: row.qpc_end,
                source_frame: row.source_frame,
                source_qpc: row.source_qpc,
                source_seq: row.source_seq,
                present_seq: row.present_seq,
                duplicate: row.duplicate,
                keys: projected.keys,
                buttons: projected.buttons,
                actions: projected.actions,
                pointer_dx: projected.pointer_dx,
                pointer_dy: projected.pointer_dy,
                pointer_raw_dx: raw_dx,
                pointer_raw_dy: raw_dy,
                pointer_bins: projected.pointer_bins,
                events: projected.events.iter().map(normalized_event).collect(),
                src: "rec_normalized_v1".to_owned(),
                normalization_spec_sha256: spec.sha256.clone(),
                frame_plan_sha256: plan_sha256.to_owned(),
            });
        }
    }
    Ok(labels)
}

fn projection_profile(request: &NormalizationRequest, spec: &LoadedSpec) -> CaptureProfile {
    // FrameProjector currently receives CaptureProfile only to derive video_t_us.
    // This rate view is deliberately not a capture profile and is never validated or
    // admitted to a catalog; 20/12 fps remain offline, non-trainable targets.
    CaptureProfile {
        profile_id: format!("normalization-rate-view:{}", spec.spec.spec_id),
        non_trainable: true,
        capture_backend: "native_wgc_window_v1".to_owned(),
        encoder_backend: "mf_nvidia_hardware_v1".to_owned(),
        width: request.source_width,
        height: request.source_height,
        fps_num: spec.spec.target.fps_num,
        fps_den: spec.spec.target.fps_den,
        bitrate_bps: 1,
        max_bitrate_bps: 1,
        gop_frames: spec.spec.gop_frames(),
        segment_seconds: spec.spec.target.chunk_seconds,
        test_segments: 1,
        accepted_encoder_relaxations: None,
        audio: None,
    }
}

fn pointer_raw_sum(events: &[FrameEvent]) -> anyhow::Result<(i64, i64)> {
    let mut dx = 0_i64;
    let mut dy = 0_i64;
    for event in events {
        if event.event_type == "mouse_move" {
            dx = dx
                .checked_add(i64::from(event.dx.context("mouse_move has no dx")?))
                .context("normalized raw pointer dx overflowed i64")?;
            dy = dy
                .checked_add(i64::from(event.dy.context("mouse_move has no dy")?))
                .context("normalized raw pointer dy overflowed i64")?;
        }
    }
    Ok((dx, dy))
}

fn clamp_pointer(value: i64) -> i32 {
    value.clamp(i64::from(-POINTER_CLAMP), i64::from(POINTER_CLAMP)) as i32
}

fn normalized_event(event: &FrameEvent) -> NormalizedEvent {
    NormalizedEvent {
        event_type: event.event_type.clone(),
        vk: event.vk,
        scan_code: event.scan_code,
        flags: event.flags,
        dx: event.dx,
        dy: event.dy,
        delta: event.delta,
        horizontal: event.horizontal,
    }
}

fn jsonl_bytes(rows: &[impl Serialize]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open JSONL input {}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|ext| ext == "zst") {
        Box::new(BufReader::new(zstd::Decoder::new(file)?))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut rows = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        ensure!(
            !line.trim().is_empty(),
            "blank JSONL row at line {}",
            index + 1
        );
        rows.push(
            serde_json::from_reader(Cursor::new(line.as_bytes())).with_context(|| {
                format!("invalid JSONL row {} in {}", index + 1, path.display())
            })?,
        );
    }
    Ok(rows)
}

fn source_evidence(
    role: &str,
    run_id: Option<&str>,
    segment: Option<u32>,
    declared: &Path,
    resolved: &Path,
) -> anyhow::Result<SourceEvidence> {
    Ok(SourceEvidence {
        role: role.to_owned(),
        run_id: run_id.map(str::to_owned),
        segment,
        path: declared.to_string_lossy().replace('\\', "/"),
        sha256: sha256_file(resolved)?,
        bytes: fs::metadata(resolved)
            .with_context(|| format!("failed to stat source {}", resolved.display()))?
            .len(),
    })
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{label} is not a safe path component"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    const MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.normalizer-fixture.pc",
      "version":"1",
      "game":{"id":"normalizer-fixture","name":"Normalizer Fixture","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"normalizer-fixture-recording-v1",
      "bindings":[
        {"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"},
        {"device":"mouse","control":"delta_x","semantic":"camera.yaw"},
        {"device":"mouse","control":"delta_y","semantic":"camera.pitch"},
        {"device":"mouse","button":1,"semantic":"combat.primary_attack"}
      ]
    }"#;

    fn spec(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("specs")
            .join(name)
    }

    fn timing(frames: usize, anchor: i64, session_start: u64) -> Vec<SourceFrameTiming> {
        (0..frames)
            .map(|frame| SourceFrameTiming {
                frame: frame as u64,
                session_frame: session_start + frame as u64,
                output_qpc_start: anchor + frame as i64,
                output_qpc_end: anchor + frame as i64 + 1,
                source_qpc: anchor + frame as i64,
                source_seq: session_start + frame as u64 + 1,
                present_seq: None,
                duplicate: false,
                source_superseded: 0,
                queue_drop: 0,
            })
            .collect()
    }

    fn event(seq: u64, qpc: i64, event_type: &str) -> RawInputEvent {
        RawInputEvent {
            seq,
            run_id: "r0000".into(),
            qpc,
            event_type: event_type.into(),
            device: "fixture".into(),
            vk: None,
            scan_code: None,
            flags: None,
            repeat: None,
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
            control: false,
        }
    }

    fn key(mut event: RawInputEvent, down: bool) -> RawInputEvent {
        event.event_type = if down { "key_down" } else { "key_up" }.into();
        event.vk = Some(87);
        event.scan_code = Some(17);
        event.flags = Some(0);
        event
    }

    fn mouse_move(mut event: RawInputEvent, dx: i32, dy: i32) -> RawInputEvent {
        event.event_type = "mouse_move".into();
        event.dx = Some(dx);
        event.dy = Some(dy);
        event.absolute = Some(false);
        event
    }

    fn mouse_button(mut event: RawInputEvent, down: bool) -> RawInputEvent {
        event.event_type = if down { "mouse_down" } else { "mouse_up" }.into();
        event.vk = Some(1);
        event
    }

    fn write_jsonl(path: &Path, rows: &[impl Serialize]) {
        let mut file = File::create(path).unwrap();
        for row in rows {
            serde_json::to_writer(&mut file, row).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    fn single_run_fixture(
        temp: &TempDir,
        frames: Vec<SourceFrameTiming>,
        events: Vec<RawInputEvent>,
    ) -> PathBuf {
        let root = temp.path();
        fs::write(root.join("mapping.json"), MAPPING).unwrap();
        write_jsonl(&root.join("timing.jsonl"), &frames);
        write_jsonl(&root.join("events.jsonl"), &events);
        fs::write(root.join("video.mp4"), b"media-backend-fixture").unwrap();
        let request = serde_json::json!({
            "schema_version": REQUEST_SCHEMA,
            "session_id": "fixture-session",
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
                    "video_path": "video.mp4"
                }]
            }]
        });
        let path = root.join("request.json");
        fs::write(&path, serde_json::to_vec_pretty(&request).unwrap()).unwrap();
        path
    }

    #[test]
    fn supported_matrix_selects_the_last_source_slot_and_trims_only_the_tail() {
        for (spec_name, factor) in [
            ("fps60-to-training30-v1.json", 2_usize),
            ("fps60-to-training20-v1.json", 3),
            ("fps60-to-training12-v1.json", 5),
        ] {
            let temp = TempDir::new().unwrap();
            let request = single_run_fixture(&temp, timing(factor * 4 + 1, 1000, 0), Vec::new());
            let prepared = prepare(&request, spec(spec_name)).unwrap();
            assert_eq!(prepared.plan.len(), 4);
            assert_eq!(prepared.labels.len(), 4);
            assert_eq!(prepared.runs[0].trimmed_source_frames, 1);
            for (index, row) in prepared.plan.iter().enumerate() {
                assert_eq!(row.run_frame, index as u64);
                assert_eq!(row.qpc_start, 1000 + (index * factor) as i64);
                assert_eq!(row.qpc_end, 1000 + ((index + 1) * factor) as i64);
                assert_eq!(row.source_frame, ((index + 1) * factor - 1) as u64);
                assert_eq!(row.trim, index == 3);
            }
        }
    }

    #[test]
    fn golden_fixture_locks_boundaries_transient_actions_and_single_pointer_clamp() {
        let temp = TempDir::new().unwrap();
        let events = vec![
            key(event(1, 1000, "key_down"), true),
            mouse_move(event(2, 1000, "mouse_move"), 150, 0),
            mouse_move(event(3, 1001, "mouse_move"), 150, 0),
            key(event(4, 1002, "key_up"), false),
            mouse_button(event(5, 1004, "mouse_down"), true),
            mouse_button(event(6, 1004, "mouse_up"), false),
        ];
        let request = single_run_fixture(&temp, timing(6, 1000, 0), events);
        let prepared = prepare(&request, spec("fps60-to-training30-v1.json")).unwrap();
        let actual = prepared
            .plan
            .iter()
            .zip(&prepared.labels)
            .map(|(plan, label)| {
                serde_json::json!({
                    "out_frame": plan.out_frame,
                    "qpc_start": plan.qpc_start,
                    "qpc_end": plan.qpc_end,
                    "source_frame": plan.source_frame,
                    "source_seq": plan.source_seq,
                    "keys": label.keys,
                    "buttons": label.buttons,
                    "actions": label.actions,
                    "pointerRawDx": label.pointer_raw_dx,
                    "pointerRawDy": label.pointer_raw_dy,
                    "pointerDx": label.pointer_dx,
                    "pointerDy": label.pointer_dy,
                    "pointerBins": label.pointer_bins,
                    "duplicate": plan.duplicate
                })
            })
            .collect::<Vec<_>>();
        let expected: serde_json::Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/golden-60-to-30-v1.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(serde_json::Value::Array(actual), expected);
        assert_eq!(prepared.labels[0].pointer_raw_dx, 300);
        assert_eq!(prepared.labels[0].pointer_dx, POINTER_CLAMP);
        assert!(prepared.labels[2].buttons.is_empty());
        assert_eq!(prepared.labels[2].actions, vec!["combat.primary_attack"]);
    }

    #[test]
    fn duplicate_identity_is_preserved_and_replay_hashes_are_deterministic() {
        let temp = TempDir::new().unwrap();
        let mut frames = timing(8, 1000, 0);
        frames[2].source_qpc = frames[1].source_qpc;
        frames[2].source_seq = frames[1].source_seq;
        frames[2].duplicate = true;
        frames[3].source_qpc = frames[1].source_qpc;
        frames[3].source_seq = frames[1].source_seq;
        frames[3].duplicate = true;
        let request = single_run_fixture(&temp, frames, Vec::new());
        let first = prepare(&request, spec("fps60-to-training30-v1.json")).unwrap();
        let second = prepare(&request, spec("fps60-to-training30-v1.json")).unwrap();
        assert!(first.plan[1].duplicate);
        assert!(first.labels[1].duplicate);
        assert_eq!(first.plan_bytes, second.plan_bytes);
        assert_eq!(first.plan_sha256, second.plan_sha256);
        assert_eq!(first.label_bytes, second.label_bytes);
        assert_eq!(first.labels_sha256, second.labels_sha256);
    }

    #[test]
    fn selected_present_identity_survives_plan_and_label_projection() {
        let temp = TempDir::new().unwrap();
        let mut frames = timing(4, 1000, 0);
        for (index, frame) in frames.iter_mut().enumerate() {
            frame.present_seq = Some(65_535 + index as u64);
        }
        let request = single_run_fixture(&temp, frames, Vec::new());
        let prepared = prepare(&request, spec("fps60-to-training30-v1.json")).unwrap();
        let expected = [Some(65_536), Some(65_538)];
        assert_eq!(
            prepared
                .plan
                .iter()
                .map(|row| row.present_seq)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            prepared
                .labels
                .iter()
                .map(|row| row.present_seq)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn held_state_crosses_source_segments_but_pause_resume_resets_the_run() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("mapping.json"), MAPPING).unwrap();
        let segment0 = timing(2, 1000, 0);
        let segment1 = timing(2, 1002, 2);
        let run1 = timing(2, 2000, 4);
        write_jsonl(&root.join("r0s0-timing.jsonl"), &segment0);
        write_jsonl(&root.join("r0s1-timing.jsonl"), &segment1);
        write_jsonl(&root.join("r1s0-timing.jsonl"), &run1);
        write_jsonl(
            &root.join("r0s0-events.jsonl"),
            &[key(event(1, 1000, "key_down"), true)],
        );
        write_jsonl(
            &root.join("r0s1-events.jsonl"),
            &[key(event(2, 1003, "key_up"), false)],
        );
        let no_events: Vec<RawInputEvent> = Vec::new();
        write_jsonl(&root.join("r1s0-events.jsonl"), &no_events);
        for name in ["r0s0.mp4", "r0s1.mp4", "r1s0.mp4"] {
            fs::write(root.join(name), b"media-backend-fixture").unwrap();
        }
        let request = serde_json::json!({
            "schema_version": REQUEST_SCHEMA,
            "session_id": "multi-run-fixture",
            "source_fps": {"fps_num": 60, "fps_den": 1},
            "source_width": 1920,
            "source_height": 1080,
            "mapping_path": "mapping.json",
            "runs": [
                {
                    "run_id": "r0000",
                    "qpc_anchor": 1000,
                    "qpc_frequency": 60,
                    "segments": [
                        {"timing_path":"r0s0-timing.jsonl","events_path":"r0s0-events.jsonl","video_path":"r0s0.mp4"},
                        {"timing_path":"r0s1-timing.jsonl","events_path":"r0s1-events.jsonl","video_path":"r0s1.mp4"}
                    ]
                },
                {
                    "run_id": "r0001",
                    "qpc_anchor": 2000,
                    "qpc_frequency": 60,
                    "segments": [
                        {"timing_path":"r1s0-timing.jsonl","events_path":"r1s0-events.jsonl","video_path":"r1s0.mp4"}
                    ]
                }
            ]
        });
        let request_path = root.join("request.json");
        fs::write(&request_path, serde_json::to_vec_pretty(&request).unwrap()).unwrap();
        let prepared = prepare(&request_path, spec("fps60-to-training30-v1.json")).unwrap();
        assert_eq!(prepared.runs.len(), 2);
        assert_eq!(
            (prepared.runs[0].plan_start, prepared.runs[0].plan_len),
            (0, 2)
        );
        assert_eq!(
            (prepared.runs[1].plan_start, prepared.runs[1].plan_len),
            (2, 1)
        );
        assert_eq!(prepared.labels[0].keys, vec![87]);
        assert!(prepared.labels[1].keys.is_empty());
        assert_eq!(prepared.labels[1].actions, vec!["move.forward"]);
        assert!(prepared.labels[2].keys.is_empty());
        assert!(prepared.labels[2].actions.is_empty());
        assert_eq!(prepared.plan[2].run_frame, 0);
        assert_eq!(prepared.plan[2].qpc_start, 2000);
        assert!(!prepared.spec.spec.framepair_policy.emit);
    }

    #[test]
    fn exact_request_rate_rejects_5994_even_when_the_decimal_looks_like_60() {
        let temp = TempDir::new().unwrap();
        let request = single_run_fixture(&temp, timing(6, 1000, 0), Vec::new());
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
        value["source_fps"] = serde_json::json!({"fps_num": 60000, "fps_den": 1001});
        fs::write(&request, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let error = prepare(&request, spec("fps60-to-training30-v1.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exactly match"));
    }

    #[test]
    fn exhaustive_window_property_has_no_overlap_hole_or_event_loss() {
        for target in [30_u32, 20, 12] {
            let loaded =
                LoadedSpec::load(spec(&format!("fps60-to-training{target}-v1.json"))).unwrap();
            let factor = loaded.spec.downsample_factor() as usize;
            for frames in factor..=100 {
                let source = timing(frames, 1000, 0)
                    .into_iter()
                    .map(|timing| SourceSlot { timing, segment: 0 })
                    .collect::<Vec<_>>();
                let events = (0..frames)
                    .map(|i| mouse_move(event(i as u64 + 1, 1000 + i as i64, "mouse_move"), 1, 0))
                    .collect::<Vec<_>>();
                let run = LoadedRun {
                    run_id: "r0000".into(),
                    anchor: 1000,
                    frequency: 60,
                    source,
                    events,
                    source_videos: vec![PathBuf::from("video.mp4")],
                    source_video_frames: vec![frames as u32],
                };
                let (plan, summary) = build_plan(&[run], &loaded).unwrap();
                assert_eq!(plan.len(), frames / factor);
                assert!(
                    plan.windows(2)
                        .all(|pair| pair[0].qpc_end == pair[1].qpc_start)
                );
                assert_eq!(
                    summary[0].covered_events + summary[0].trimmed_events,
                    frames
                );
                assert_eq!(summary[0].trimmed_source_frames as usize, frames % factor);
            }
        }
    }
}
