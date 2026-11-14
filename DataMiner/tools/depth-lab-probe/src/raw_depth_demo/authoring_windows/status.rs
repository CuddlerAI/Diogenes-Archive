//! Bounded transition evidence. No game rendering, depth authorization, or clock changes.
use super::*;

const MAX_SAMPLES: usize = 2048;
const MAX_GUIDES: usize = 0;

#[derive(Clone, Serialize)]
struct Sample {
    qpc: i64,
    present_seq: u64,
    pending_ordinal: Option<u32>,
    committed_ordinal: Option<u32>,
    interval_active: bool,
    eligible_clear_calls: u64,
    active_proven_count: usize,
    active_proven_candidates: Vec<String>,
    candidates_truncated: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Guidance {
    schema: String,
    process_launch_id: String,
    completed_ordinal: u32,
    next_fov: Option<u32>,
}

#[derive(Default, Serialize)]
pub(super) struct Status {
    samples: Vec<Sample>,
    // Preserve the v1 trace shape; GPU operator prompts have been withdrawn.
    guide_submissions: [(); 0],
    guidance: Option<Guidance>,
}

pub(super) fn observe(
    runtime: &Runtime,
    control: &mut Control,
    snapshot: &crate::d3d12_backend::D3d12ObservationSnapshot,
    present: u64,
) -> Result<(), String> {
    if !runtime.arm_authorized.load(Ordering::Acquire) {
        return Ok(());
    }
    let now = current_qpc();
    let pending_ordinal = control.pending.as_ref().map(|p| p.request.ordinal);
    let committed_ordinal = control.phases.last().map(|p| p.ordinal);
    let interval_active = runtime.authoring_interval.observing();
    let active_proven_count = snapshot
        .depth_candidates
        .iter()
        .filter(|c| c.pending_clear_calls > 0 && semantic_candidate_matches(c, runtime.fingerprint))
        .count();
    if control.status.samples.last().is_some_and(|last| {
        now - last.qpc < runtime.qpc_frequency / 4
            && last.pending_ordinal == pending_ordinal
            && last.committed_ordinal == committed_ordinal
            && last.interval_active == interval_active
    }) {
        return Ok(());
    }
    if control.status.samples.len() >= MAX_SAMPLES {
        return Err("authoring transition evidence capacity exceeded".into());
    }
    control.status.samples.push(Sample {
        qpc: now,
        present_seq: present,
        pending_ordinal,
        committed_ordinal,
        interval_active,
        eligible_clear_calls: snapshot.eligible_pre_clear_snapshots,
        active_proven_count,
        active_proven_candidates: snapshot
            .depth_candidates
            .iter()
            .filter(|c| {
                c.pending_clear_calls > 0 && semantic_candidate_matches(c, runtime.fingerprint)
            })
            .take(4)
            .map(|c| c.candidate_id.clone())
            .collect(),
        candidates_truncated: active_proven_count > 4,
    });
    Ok(())
}

fn validate_guidance(
    g: &Guidance,
    launch: &str,
    control: &Control,
    now: i64,
    active: bool,
) -> Result<(), String> {
    if g.schema != "dm-cp2077-authoring-guidance/v1"
        || g.process_launch_id != launch
        || g.next_fov.is_some_and(|f| !matches!(f, 75 | 90))
        || control.pending.is_some()
        || active
        || !control.authorized.is_empty()
        || control
            .phases
            .last()
            .is_none_or(|p| p.ordinal != g.completed_ordinal || now < p.window_deadline_qpc)
        || control
            .status
            .guidance
            .as_ref()
            .is_some_and(|old| old.completed_ordinal >= g.completed_ordinal)
    {
        return Err(
            "authoring guidance has wrong launch/order or precedes completed sampling".into(),
        );
    }
    Ok(())
}

pub(super) fn poll_guidance(runtime: &Runtime) -> Result<(), String> {
    let ordinal = {
        let control = runtime
            .authoring_control
            .lock()
            .map_err(|_| "authoring control poisoned")?;
        let Some(phase) = control.phases.last() else {
            return Ok(());
        };
        if control
            .status
            .guidance
            .as_ref()
            .is_some_and(|g| g.completed_ordinal == phase.ordinal)
        {
            return Ok(());
        }
        phase.ordinal
    };
    let path = runtime
        .output_dir
        .join(format!("camera-authoring-guide-{ordinal:02}.json"));
    let metadata = match path.metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CONTROL_REQUEST_BYTES {
        return Err("unbounded authoring guidance file".into());
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| {
            f.take(MAX_CONTROL_REQUEST_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CONTROL_REQUEST_BYTES {
        return Err("authoring guidance grew past bound".into());
    }
    let g: Guidance = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let mut control = runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?;
    validate_guidance(
        &g,
        &runtime.launch_id,
        &control,
        current_qpc(),
        runtime.authoring_interval.observing(),
    )?;
    control.status.guidance = Some(g);
    Ok(())
}

pub(super) fn write_timeline(runtime: &Runtime) -> Result<(), String> {
    let control = runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?;
    let document = serde_json::json!({
        "schema": "dm-cp2077-authoring-timeline/v1", "process_launch_id": runtime.launch_id,
        "qpc_frequency": runtime.qpc_frequency, "non_trainable": true,
        "guide_evidence": "disabled; no GPU operator prompts are submitted",
        "sample_limit": MAX_SAMPLES, "guide_limit": MAX_GUIDES, "trace": control.status,
        "phases": control.phases,
    });
    drop(control);
    let path = runtime.output_dir.join("camera-authoring-timeline.json");
    let temporary = runtime
        .output_dir
        .join(".camera-authoring-timeline.prepared");
    if path.exists() {
        return Err("refusing existing authoring timeline".into());
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, &document).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn completed() -> Control {
        let mut c = Control::default();
        c.phases.push(FramePhase {
            source_identity: DsvIdentity {
                handle: crate::d3d12_abi::DescriptorHandle(1),
                resource: crate::d3d12_abi::ResourceId(1),
                descriptor_generation: 1,
                resource_epoch: 1,
            },
            ordinal: 1,
            phase: "fov75".into(),
            runtime_candidate_id: "r1".into(),
            source_prepared_present: 2,
            window_open_observed_qpc: 10,
            window_deadline_qpc: 20,
        });
        c
    }
    #[test]
    fn guidance_cannot_precede_drained_window_or_replay_a_completed_phase() {
        let mut c = completed();
        let g = Guidance {
            schema: "dm-cp2077-authoring-guidance/v1".into(),
            process_launch_id: "a".repeat(32),
            completed_ordinal: 1,
            next_fov: Some(90),
        };
        assert!(validate_guidance(&g, &g.process_launch_id, &c, 19, false).is_err());
        assert!(validate_guidance(&g, &g.process_launch_id, &c, 21, true).is_err());
        assert!(validate_guidance(&g, &"b".repeat(32), &c, 21, false).is_err());
        assert!(validate_guidance(&g, &g.process_launch_id, &c, 21, false).is_ok());
        c.status.guidance = Some(g.clone());
        assert!(validate_guidance(&g, &g.process_launch_id, &c, 21, false).is_err());
        c.pending = Some(Pending {
            request: Request {
                schema: REQUEST_SCHEMA.into(),
                source_policy: SOURCE_POLICY.into(),
                process_launch_id: "a".repeat(32),
                ordinal: 2,
                phase: "fov90".into(),
                duration_ms: 4000,
            },
            hash: "x".into(),
            received_qpc: 22,
            barrier_sent: false,
            selection: None,
            prepared: None,
        });
        c.status.guidance = None;
        assert!(validate_guidance(&g, &g.process_launch_id, &c, 22, false).is_err());
    }
}
