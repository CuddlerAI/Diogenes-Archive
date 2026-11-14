//! Discontinuous, diagnostic-only observations with explicit drained source boundaries.
//! The raw session has one arm. Healthy phase changes retain the ring, counters and capture IDs.
use super::*;
use crate::d3d12_abi::DsvIdentity;
use std::collections::BTreeMap;
mod status;

const REQUEST_SCHEMA: &str = "dm-cp2077-camera-authoring-window/v2";
const SOURCE_POLICY: &str = "revalidate-at-drained-boundary";
const MAX_WINDOWS: u32 = 8;
const MAX_TOTAL_MS: u64 = 64_000;
const PREPARE_TIMEOUT_MS: i64 = 5_000;

#[derive(Default)]
pub(super) struct IntervalGate {
    pub(super) deadline: AtomicI64,
    pub(super) active: AtomicBool,
}
impl IntervalGate {
    pub(super) fn allows_new(&self, now: i64, stopping: bool) -> bool {
        !stopping && now > 0 && now < self.deadline.load(Ordering::Acquire)
    }
    pub(super) fn begin(&self, now: i64, stopping: bool) -> bool {
        let active = self.allows_new(now, stopping);
        self.active.store(active, Ordering::Release);
        active
    }
    pub(super) fn observing(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    source_policy: String,
    process_launch_id: String,
    ordinal: u32,
    phase: String,
    duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(super) struct FramePhase {
    #[serde(skip)]
    source_identity: DsvIdentity,
    ordinal: u32,
    phase: String,
    pub(super) runtime_candidate_id: String,
    source_prepared_present: u64,
    window_open_observed_qpc: i64,
    window_deadline_qpc: i64,
}

#[derive(Clone, Debug, Serialize)]
struct PreparedSource {
    #[serde(skip)]
    source_identity: DsvIdentity,
    previous_candidate_id: Option<String>,
    runtime_candidate_id: String,
    prepared_present: u64,
    prepared_qpc: i64,
    multi_clear_baseline: u64,
    unpaired_clear_baseline: u64,
}

struct Pending {
    request: Request,
    hash: String,
    received_qpc: i64,
    barrier_sent: bool,
    selection: Option<(String, u64, u64)>,
    prepared: Option<PreparedSource>,
}

impl Pending {
    fn check_prepare_deadline(&self, now: i64, frequency: i64) -> Result<(), String> {
        if now.saturating_sub(self.received_qpc)
            > frequency.saturating_mul(PREPARE_TIMEOUT_MS) / 1000
        {
            return Err("authoring phase failed to drain/bind within 5 seconds".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Control {
    pending: Option<Pending>,
    phases: Vec<FramePhase>,
    authorized: BTreeMap<u64, FramePhase>,
    status: status::Status,
}

pub(super) fn candidate_id(source: DsvIdentity) -> String {
    format!(
        "dsv-r{}-g{}-e{}",
        source.resource.0, source.descriptor_generation, source.resource_epoch
    )
}

pub(super) fn authorize_interval(runtime: &Runtime, present: u64) -> Result<(), String> {
    let mut control = runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?;
    let phase = control
        .phases
        .last()
        .cloned()
        .ok_or("authoring interval has no committed phase")?;
    if control.pending.is_some()
        || control.authorized.len() >= runtime.max_frames as usize
        || present <= phase.source_prepared_present
        || control.authorized.contains_key(&present)
    {
        return Err("authoring interval replay, phase overlap or frame budget exceeded".into());
    }
    control.authorized.insert(present, phase);
    Ok(())
}

impl Control {
    pub(super) fn has_authorized_frames(&self) -> bool {
        !self.authorized.is_empty()
    }
    fn claim(&mut self, present: u64, source: DsvIdentity) -> Result<FramePhase, String> {
        let expected = self
            .authorized
            .get(&present)
            .ok_or("authoring frame lacks an authorized Present/phase")?;
        if expected.source_identity != source
            || expected.runtime_candidate_id != candidate_id(source)
        {
            return Err("authoring GPU frame source differs from its authorized phase".into());
        }
        // Wrong or late frames cannot consume an obligation or borrow the latest phase identity.
        Ok(self.authorized.remove(&present).expect("checked above"))
    }
}

pub(super) fn claim_frame(
    runtime: &Runtime,
    present: u64,
    source: Option<DsvIdentity>,
) -> Result<FramePhase, String> {
    runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?
        .claim(
            present,
            source.ok_or("authoring GPU frame has no actual source identity")?,
        )
}

/// Called at the beginning of Present, before pending executed-clear counters are accounted.
/// The old interval must already have closed; all old frames, camera work and writer commands
/// are preserved and drained before a new source can be selected.
pub(super) fn prepare_selection(
    runtime: &Runtime,
    backend: &D3d12ObservationBackend,
    snapshot: &crate::d3d12_backend::D3d12ObservationSnapshot,
    present: u64,
) -> Result<Option<D3d12DepthSelector>, String> {
    let mut control = runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?;
    status::observe(runtime, &mut control, snapshot, present)?;
    if control.pending.is_none() {
        return Ok(None);
    }
    if runtime.authoring_interval.active.load(Ordering::Acquire)
        || !control.authorized.is_empty()
        || snapshot.capture_in_flight != 0
        || !backend.authoring_boundary_is_drained()?
    {
        return Ok(None);
    }
    let pending = control.pending.as_mut().expect("checked above");
    pending.check_prepare_deadline(current_qpc(), runtime.qpc_frequency)?;
    if pending.selection.is_some() || pending.prepared.is_some() {
        return Ok(None);
    }
    if !pending.barrier_sent {
        backend.archive_local_camera_before(present, |rows| {
            match runtime.sender.try_send(WriterCommand::CameraArchive(rows)) {
                Ok(()) => Ok(()),
                Err(
                    TrySendError::Full(WriterCommand::CameraArchive(rows))
                    | TrySendError::Disconnected(WriterCommand::CameraArchive(rows)),
                ) => Err(rows),
                Err(_) => unreachable!(),
            }
        })?;
        runtime
            .sender
            .try_send(WriterCommand::AuthoringBarrier(u64::from(
                pending.request.ordinal,
            )))
            .map_err(|_| "authoring writer barrier unavailable")?;
        pending.barrier_sent = true;
        return Ok(None);
    }
    if runtime.authoring_writer_barrier.load(Ordering::Acquire)
        != u64::from(pending.request.ordinal)
    {
        return Ok(None);
    }
    let matching =
        sampling_semantic_candidates(&snapshot.depth_candidates, runtime.fingerprint, true, true)
            .expect("active authoring returns candidates");
    runtime
        .semantic_match_count
        .store(matching.len() as u64, Ordering::Release);
    let Some(c) = select_boundary_candidate(&matching)? else {
        // Returning from the FOV menu can leave an interval without an executed clear.
        // Keep the window closed and the original preparation deadline; never borrow a
        // historical target or reset the drained barrier while waiting for a live source.
        return Ok(None);
    };
    pending.selection = Some((
        c.candidate_id.clone(),
        c.present_intervals_with_multiple_clears,
        unpaired_clear_count(c),
    ));
    Ok(Some(D3d12DepthSelector {
        candidate_id: c.candidate_id.clone(),
        width: runtime.width,
        height: runtime.height,
        sample_count: runtime.fingerprint.sample_count,
        accepted_formats: vec![
            runtime.fingerprint.resource_format,
            runtime.fingerprint.dsv_format,
        ],
        stable_selection: None,
    }))
}

pub(super) fn select_boundary_candidate<'a>(
    matching: &[&'a D3d12DepthCandidateObservation],
) -> Result<Option<&'a D3d12DepthCandidateObservation>, String> {
    match matching {
        [] => Ok(None),
        [candidate] => Ok(Some(*candidate)),
        _ => Err(format!(
            "authoring phase requires one active proven source: matches={}",
            matching.len()
        )),
    }
}

pub(super) fn source_prepared(
    runtime: &Runtime,
    source: DsvIdentity,
    present: u64,
) -> Result<(), String> {
    let mut control = runtime
        .authoring_control
        .lock()
        .map_err(|_| "authoring control poisoned")?;
    let previous = control
        .phases
        .last()
        .map(|p| p.runtime_candidate_id.clone());
    let pending = control
        .pending
        .as_mut()
        .ok_or("authoring source prepared without a request")?;
    let (selected, multi, unpaired) = pending
        .selection
        .as_ref()
        .ok_or("authoring source has no selection")?;
    let actual = candidate_id(source);
    if pending.prepared.is_some() || actual != *selected {
        return Err("authoring prepared source differs from selection or was replayed".into());
    }
    *runtime
        .runtime_candidate_id
        .lock()
        .map_err(|_| "raw candidate lock poisoned")? = Some(actual.clone());
    runtime
        .armed_double_clear_interval_baseline
        .store(*multi, Ordering::Release);
    runtime
        .armed_unpaired_clear_baseline
        .store(*unpaired, Ordering::Release);
    pending.prepared = Some(PreparedSource {
        source_identity: source,
        previous_candidate_id: previous,
        runtime_candidate_id: actual,
        prepared_present: present,
        prepared_qpc: current_qpc(),
        multi_clear_baseline: *multi,
        unpaired_clear_baseline: *unpaired,
    });
    Ok(())
}

#[derive(Default)]
pub(super) struct Windows {
    completed_requests: u32,
    total_ms: u64,
}

impl Windows {
    fn validate(
        &self,
        request: &Request,
        launch_id: &str,
        now: i64,
        previous_end: i64,
        frequency: i64,
    ) -> Result<i64, String> {
        if request.schema != REQUEST_SCHEMA
            || request.source_policy != SOURCE_POLICY
            || request.process_launch_id != launch_id
            || request.ordinal != self.completed_requests + 1
            || request.ordinal > MAX_WINDOWS
            || !(1_000..=40_000).contains(&request.duration_ms)
            || request.phase.is_empty()
            || request.phase.len() > 48
            || !request
                .phase
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.total_ms.saturating_add(request.duration_ms) > MAX_TOTAL_MS
            || now <= 0
            || frequency <= 0
            || now < previous_end
        {
            return Err(
                "camera authoring window has invalid identity, order, overlap or budget".into(),
            );
        }
        let ticks = frequency
            .checked_mul(request.duration_ms as i64)
            .and_then(|n| n.checked_div(1_000))
            .ok_or("camera authoring window duration overflow")?;
        now.checked_add(ticks)
            .ok_or_else(|| "camera authoring window QPC overflow".into())
    }

    pub(super) fn poll(&mut self, runtime: &Runtime) -> Result<(), String> {
        status::poll_guidance(runtime)?;
        // No filesystem work on Present. Holding this mutex is limited to tiny state copies;
        // publishing the durable receipt happens here, on the control worker.
        let prepared = {
            let control = runtime
                .authoring_control
                .lock()
                .map_err(|_| "authoring control poisoned")?;
            if let Some(p) = &control.pending {
                p.check_prepare_deadline(current_qpc(), runtime.qpc_frequency)?;
                let Some(source) = p.prepared.clone() else {
                    return Ok(());
                };
                Some((p.request.clone(), p.hash.clone(), source))
            } else {
                None
            }
        };
        if let Some((request, hash, source)) = prepared {
            let now = current_qpc();
            let end = self.validate(
                &request,
                &runtime.launch_id,
                now,
                runtime.authoring_interval.deadline.load(Ordering::Acquire),
                runtime.qpc_frequency,
            )?;
            let phase = FramePhase {
                source_identity: source.source_identity,
                ordinal: request.ordinal,
                phase: request.phase.clone(),
                runtime_candidate_id: source.runtime_candidate_id.clone(),
                source_prepared_present: source.prepared_present,
                window_open_observed_qpc: now,
                window_deadline_qpc: end,
            };
            let receipt = serde_json::json!({
                "schema": "dm-cp2077-camera-authoring-window-receipt/v2", "request": request,
                "request_sha256": hash, "qpc_frequency": runtime.qpc_frequency,
                "window_open_observed_qpc": now, "window_deadline_qpc": end,
                "source_binding": source, "previous_work_drained": true,
                "writer_barrier_ordinal": runtime.authoring_writer_barrier.load(Ordering::Acquire),
                "sampling_mode": "discontinuous_authoring_windows/v2", "input_events_recorded": false,
                "non_trainable": true, "recording_ready": false, "production_binding_authorized": false,
            });
            let temporary = runtime.output_dir.join(format!(
                "camera-authoring-receipt-{:02}.prepared",
                request.ordinal
            ));
            let published = runtime.output_dir.join(format!(
                "camera-authoring-receipt-{:02}.json",
                request.ordinal
            ));
            if published.exists() {
                return Err("refusing existing camera authoring receipt".into());
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            file.write_all(&serde_json::to_vec_pretty(&receipt).map_err(|e| e.to_string())?)
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
            drop(file);
            std::fs::rename(temporary, published).map_err(|e| e.to_string())?;
            let mut control = runtime
                .authoring_control
                .lock()
                .map_err(|_| "authoring control poisoned")?;
            control.phases.push(phase);
            control.pending = None;
            runtime.last_cadence_request_qpc.store(0, Ordering::Release);
            runtime
                .authoring_interval
                .deadline
                .store(end, Ordering::Release);
            self.completed_requests += 1;
            self.total_ms += request.duration_ms;
            return Ok(());
        }
        let ordinal = self.completed_requests + 1;
        let path = runtime
            .output_dir
            .join(format!("camera-authoring-window-{ordinal:02}.json"));
        let metadata = match path.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("stat camera authoring request: {e}")),
        };
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CONTROL_REQUEST_BYTES
        {
            return Err("camera authoring request is not a bounded regular file".into());
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
            return Err("camera authoring request grew past its bound".into());
        }
        let request: Request = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let now = current_qpc();
        self.validate(
            &request,
            &runtime.launch_id,
            now,
            runtime.authoring_interval.deadline.load(Ordering::Acquire),
            runtime.qpc_frequency,
        )?;
        runtime
            .authoring_control
            .lock()
            .map_err(|_| "authoring control poisoned")?
            .pending = Some(Pending {
            request,
            hash: format!("sha256:{:x}", Sha256::digest(&bytes)),
            received_qpc: now,
            barrier_sent: false,
            selection: None,
            prepared: None,
        });
        Ok(())
    }
}

pub(super) fn write_timeline(runtime: &Runtime) -> Result<(), String> {
    status::write_timeline(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            schema: REQUEST_SCHEMA.into(),
            source_policy: SOURCE_POLICY.into(),
            process_launch_id: "a".repeat(32),
            ordinal: 1,
            phase: "fov75-motion".into(),
            duration_ms: 24_000,
        }
    }

    #[test]
    fn authorized_tail_keeps_camera_until_next_present_even_after_wall_deadline() {
        let gate = IntervalGate::default();
        gate.deadline.store(36_000, Ordering::Release);
        assert!(gate.begin(35_990, false));
        // The real failure's final Present arrived 12.3584 ms after the deadline.
        assert!(!gate.allows_new(36_012, false));
        assert!(
            gate.observing(),
            "draws of the already-open interval must finish"
        );
        assert!(!gate.begin(36_012, false));
        assert!(!gate.observing());
        gate.deadline.store(90_000, Ordering::Release);
        assert!(
            !gate.observing(),
            "publishing a phase must not open a partial render interval"
        );
        assert!(gate.begin(80_000, false));
        assert!(!gate.allows_new(80_001, true));
        assert!(
            gate.observing(),
            "Stop also drains previously authorized work"
        );
        assert!(!gate.begin(80_002, true));
    }

    #[test]
    fn prepare_deadline_includes_time_spent_waiting_for_source() {
        let pending = Pending {
            request: request(),
            hash: "test-request".into(),
            received_qpc: 100_000,
            barrier_sent: true,
            selection: None,
            prepared: None,
        };
        // Use the live machine's QPC frequency; empty observations cannot buy a
        // fresh five seconds, and even one tick beyond the original limit fails.
        for now in [100_000, 10_100_000, 50_099_999, 50_100_000] {
            assert!(pending.check_prepare_deadline(now, 10_000_000).is_ok());
            assert!(select_boundary_candidate(&[]).unwrap().is_none());
        }
        assert_eq!(
            pending.check_prepare_deadline(50_100_001, 10_000_000),
            Err("authoring phase failed to drain/bind within 5 seconds".into())
        );
    }

    #[test]
    fn frames_keep_their_authorized_source_and_reject_late_replay_or_wrong_resource() {
        use crate::d3d12_abi::{DescriptorHandle, ResourceId};
        let old = DsvIdentity {
            handle: DescriptorHandle(4),
            resource: ResourceId(6),
            descriptor_generation: 1,
            resource_epoch: 1,
        };
        let new = DsvIdentity {
            handle: DescriptorHandle(8),
            resource: ResourceId(29),
            ..old
        };
        let phase = |ordinal, source| FramePhase {
            source_identity: source,
            ordinal,
            phase: format!("fov-{ordinal}"),
            runtime_candidate_id: candidate_id(source),
            source_prepared_present: 10,
            window_open_observed_qpc: 100,
            window_deadline_qpc: 200,
        };
        let mut control = Control::default();
        control.phases.push(phase(2, new));
        control.authorized.insert(11, phase(1, old));
        control.authorized.insert(20, phase(2, new));
        assert!(control.claim(11, new).is_err());
        assert_eq!(
            control.authorized.len(),
            2,
            "wrong source cannot erase a debt"
        );
        assert_eq!(control.claim(11, old).unwrap(), phase(1, old));
        assert!(control.claim(11, old).is_err());
        assert!(control.claim(20, old).is_err());
        assert!(control.claim(19, new).is_err());
        assert_eq!(control.claim(20, new).unwrap(), phase(2, new));
        assert!(!control.has_authorized_frames());
    }

    #[test]
    fn source_transition_requires_explicit_v2_request_policy() {
        let mut req = request();
        let check =
            |r: &Request| Windows::default().validate(r, &r.process_launch_id, 100, 0, 1000);
        assert!(check(&req).is_ok());
        req.schema = "dm-cp2077-camera-authoring-window/v1".into();
        assert!(check(&req).is_err());
        req = request();
        req.source_policy = "allow-any-live-source".into();
        assert!(check(&req).is_err());
        req = request();
        req.ordinal = 9;
        let windows = Windows {
            completed_requests: 8,
            total_ms: 8_000,
        };
        assert!(
            windows
                .validate(&req, &req.process_launch_id, 100, 0, 1000)
                .is_err()
        );
    }

    #[test]
    fn authoring_windows_reject_cross_launch_replay_overlap_and_budget_growth() {
        let mut windows = Windows::default();
        let mut req = request();
        let launch = req.process_launch_id.clone();
        assert_eq!(
            windows.validate(&req, &launch, 100, 0, 1000).unwrap(),
            24_100
        );
        assert!(
            windows
                .validate(&req, &"b".repeat(32), 100, 0, 1000)
                .is_err()
        );
        assert!(windows.validate(&req, &launch, 100, 101, 1000).is_err());
        windows.completed_requests = 1;
        windows.total_ms = 24_000;
        assert!(
            windows
                .validate(&req, &launch, 24_100, 24_100, 1000)
                .is_err()
        );
        req.ordinal = 2;
        req.duration_ms = 40_000;
        assert!(
            windows
                .validate(&req, &launch, 24_100, 24_100, 1000)
                .is_ok()
        );
        windows.total_ms += 1;
        assert!(
            windows
                .validate(&req, &launch, 24_100, 24_100, 1000)
                .is_err()
        );
        req.phase = "../overwrite".into();
        assert!(
            windows
                .validate(&req, &launch, 24_100, 24_100, 1000)
                .is_err()
        );
        req = request();
        assert!(
            Windows::default()
                .validate(&req, &launch, i64::MAX, 0, 1000)
                .is_err()
        );
    }

    #[test]
    fn authoring_config_is_explicit_diagnostic_and_keeps_original_frame_ceiling() {
        let mut config = super::super::tests::config();
        config.camera_authoring_windows = true;
        assert!(validate_config(&config).is_err());
        config.manual_stop = true;
        assert_eq!(
            validate_config(&config).is_ok(),
            cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
        );
        config.max_frames = 1025;
        assert!(validate_config(&config).is_err());
        config.max_frames = 1024;
        config.capture_contract = depth_lab::PR405_LOCAL_CONTRACT.into();
        assert!(validate_config(&config).is_err());
    }
}
