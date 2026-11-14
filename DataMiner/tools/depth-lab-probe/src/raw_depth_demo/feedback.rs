//! Local raw bridge; reuses the shared SPSC protocol, ring state and D3D12 GPU owner.
//! Only the serialized Present callback consumes decisions and produces acknowledgements.
use super::*;
use crate::d3d12_candidate::D3d12CandidateRing;
use depth_lab::cp2077_raw_feedback as contract;
use depth_lab::d3d12_camera_copy_series::Observation;
use depth_lab::d3d12_camera_local::LocalCamera;
use depth_lab::selected_present_candidate_ring::{
    CandidateRingConfig, CandidateRingState, SelectPlan,
};
use depth_lab::selected_present_feedback::{
    Ack, Decision, DepthSelectionCadence, Envelope, FEEDBACK_SCHEMA_V4, FaultReason, FeedbackState,
    StreamEpoch,
};
use depth_lab::selected_present_transport::{ProbeTransport, TransportNames};
use std::collections::{BTreeMap, BTreeSet};
mod performance;
use performance::{Performance, Phase};

pub(super) struct Feedback {
    peer: crate::probe::PeerProcess,
    request: contract::RawSelectedRequest,
    stream: StreamEpoch,
    transport: ProbeTransport,
    protocol: FeedbackState,
    ring: CandidateRingState,
    gpu: Option<D3d12CandidateRing>,
    pending_arm: Option<Envelope<Decision>>,
    ack_ordinal: u64,
    camera: Option<LocalCamera>,
    warm_views: Vec<Observation>,
    warm_projections: Vec<Observation>,
    warm_presents: BTreeSet<u64>,
    candidate_ids: BTreeMap<u64, u64>,
    selected_ids: BTreeMap<u64, u64>,
    ready_candidate: Option<u64>,
    ready_published: bool,
    sealed: bool,
    fault: Option<String>,
    admitted_present: u64,
    performance: Performance,
}

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Feedback {
    pub(super) fn new(
        runtime: &Runtime,
        request: contract::RawSelectedRequest,
    ) -> Result<Self, String> {
        if !cfg!(all(feature = "cp2077-diagnostics", debug_assertions)) {
            return Err("raw selected-Present requires an explicit diagnostic build".into());
        }
        request.validate()?;
        if runtime.capture_contract != depth_lab::ISSUE735_LOCAL_CONTRACT
            || !(900..=1024).contains(&runtime.max_frames)
        {
            return Err("raw selected-Present requires issue735_local and a 900..1024 original-depth frame budget".into());
        }
        let b = &request.bootstrap;
        let stream = contract::stream(&runtime.launch_id, b.resource_epoch)?;
        let names = TransportNames::new(
            b.client_process_id,
            std::process::id(),
            stream.generation,
            &b.nonce,
        )
        .map_err(error)?;
        let transport = ProbeTransport::open(&names, &stream).map_err(error)?;
        let ring = CandidateRingState::new(
            CandidateRingConfig {
                capacity: b.ring_capacity,
                max_feedback_lag_presents: b.max_feedback_lag_presents,
                guard_slots: b.guard_slots,
            },
            stream.generation,
            stream.resource_epoch,
        )
        .map_err(error)?;
        let peer = crate::probe::PeerProcess::open(b.client_process_id)?;
        Ok(Self {
            peer,
            protocol: FeedbackState::with_cadence(
                stream.clone(),
                DepthSelectionCadence::EvenCfrFrames,
            ),
            stream,
            request,
            transport,
            ring,
            gpu: None,
            pending_arm: None,
            ack_ordinal: 1,
            camera: None,
            warm_views: Vec::new(),
            warm_projections: Vec::new(),
            warm_presents: BTreeSet::new(),
            candidate_ids: BTreeMap::new(),
            selected_ids: BTreeMap::new(),
            ready_candidate: None,
            ready_published: false,
            sealed: false,
            fault: None,
            admitted_present: 0,
            performance: Performance::default(),
        })
    }

    pub(super) fn preflight(
        &self,
        runtime: &Runtime,
        backend: &D3d12ObservationBackend,
        swapchain: *mut std::ffi::c_void,
    ) -> Result<(), String> {
        crate::d3d12_candidate::preflight_snapshot_allocation(
            &backend.swapchain_device(swapchain)?,
            &self.request.bootstrap,
            runtime.width,
            runtime.height,
            runtime.readback_slots,
        )
    }

    fn ack(&mut self, payload: Ack) -> Result<(), String> {
        self.transport
            .acks
            .try_push(&Envelope {
                schema: FEEDBACK_SCHEMA_V4,
                stream: self.stream.clone(),
                ordinal: self.ack_ordinal,
                payload,
            })
            .map_err(error)?;
        self.ack_ordinal = self
            .ack_ordinal
            .checked_add(1)
            .ok_or("raw selected ACK ordinal overflow")?;
        Ok(())
    }

    pub(super) fn capture_next(&self) -> bool {
        !self.sealed && self.fault.is_none()
    }

    pub(super) fn on_present(
        &mut self,
        runtime: &Runtime,
        backend: &D3d12ObservationBackend,
        swapchain: *mut std::ffi::c_void,
        present: u64,
    ) -> Result<(), String> {
        if self.fault.is_some() {
            return Err("raw selected-Present session is already faulted".into());
        }
        let started = std::time::Instant::now();
        let result = self.advance(runtime, backend, swapchain, present);
        self.performance.advance.record_duration(started.elapsed());
        if let Err(message) = &result {
            self.fault = Some(message.clone());
            let _ = self.transport.publish_fault(FaultReason::TransportCorrupt);
        }
        result
    }

    fn advance(
        &mut self,
        runtime: &Runtime,
        backend: &D3d12ObservationBackend,
        swapchain: *mut std::ffi::c_void,
        present: u64,
    ) -> Result<(), String> {
        if !self.sealed && self.peer.has_exited()? {
            let _ = self.transport.publish_fault(FaultReason::PeerExited);
            return Err("raw selected-Present recorder exited before Seal".into());
        }
        if !backend.capture_is_armed() {
            return Ok(());
        }
        if self.gpu.is_none() {
            self.performance
                .checkpoint(Phase::SourceArmed, current_qpc(), present, || {
                    backend.hook_timing_snapshot()
                });
            let _timer = self.performance.gpu_initialization.start();
            self.gpu = Some(D3d12CandidateRing::new(
                &backend.swapchain_device(swapchain)?,
                &backend.swapchain_queue(swapchain)?,
                &self.request.bootstrap,
                runtime.readback_slots,
                backend.gpu_snapshot_storage_bytes()?,
                depth_lab::ZConvention::Reversed,
            )?);
        }
        let gpu_timer = self.performance.gpu_ready_and_snapshots.start();
        let gpu_ready = self
            .gpu
            .as_mut()
            .ok_or("raw candidate GPU owner missing")?
            .ready()?;
        let snapshots = backend.take_gpu_depth_snapshots()?;
        drop(gpu_timer);
        if self.ring.is_armed() && !self.sealed && snapshots.len() != 1 {
            return Err("raw selected-Present missed its unique depth snapshot".into());
        }
        for snapshot in snapshots {
            if snapshot.present_seq != present
                || !backend.watermark_submitted_for(swapchain, present)
            {
                return Err(
                    "raw selected snapshot lacks its exact current Present/watermark".into(),
                );
            }
            if !gpu_ready || self.sealed {
                backend.release_gpu_depth_snapshot(&snapshot, None)?;
                continue;
            }
            let camera_timer = self.performance.local_camera.start();
            let (views, projections) =
                backend.local_camera_samples(present, runtime.width, runtime.height)?;
            if views
                .iter()
                .any(|v| v.draw.selected_depth_resource_epoch != self.stream.resource_epoch)
            {
                return Err("raw selected camera/depth resource epoch changed".into());
            }
            if let Some(camera) = self.camera.as_mut() {
                camera.observe_present(views, projections).map_err(error)?;
                self.admitted_present = present;
            } else {
                self.warm_presents.insert(present);
                self.warm_views.extend(views);
                self.warm_projections.extend(projections);
                if self.warm_views.len() > 32768 || self.warm_projections.len() > 32768 {
                    return Err("raw camera warmup observation budget exhausted".into());
                }
                if self.warm_presents.len() >= depth_lab::d3d12_camera_local::MIN_PRESENTS {
                    self.camera = Some(
                        depth_lab::d3d12_camera_local::admit(
                            std::mem::take(&mut self.warm_views),
                            std::mem::take(&mut self.warm_projections),
                            &self.warm_presents,
                        )
                        .map_err(error)?,
                    );
                    self.admitted_present = present;
                }
            }
            drop(camera_timer);
            if self.ring.is_armed() {
                let _timer = self.performance.candidate_materialize.start();
                if self.admitted_present != present {
                    return Err("raw candidate lacks exact local camera admission".into());
                }
                let slot = self.ring.materialize(present).map_err(error)?;
                let completion = self
                    .gpu
                    .as_mut()
                    .ok_or("raw candidate GPU owner missing")?
                    .materialize(&snapshot, slot)?;
                backend.release_gpu_depth_snapshot(&snapshot, Some(completion))?;
                if self
                    .candidate_ids
                    .insert(present, snapshot.capture_id)
                    .is_some()
                {
                    return Err("duplicate raw GPU candidate identity".into());
                }
                runtime.requested_intervals.fetch_add(1, Ordering::AcqRel);
                runtime
                    .feedback_capture_start_qpc
                    .compare_exchange(0, current_qpc(), Ordering::AcqRel, Ordering::Acquire)
                    .ok();
            } else {
                // Pre-Arm uses only the already bounded source snapshot scratch, no candidate ring slots/readback.
                backend.release_gpu_depth_snapshot(&snapshot, None)?;
                if self.camera.is_some() && !self.ready_published {
                    self.ready_candidate = Some(present);
                }
            }
        }
        let drain_timer = self.performance.readback_drain_and_submit.start();
        for frame in self
            .gpu
            .as_mut()
            .ok_or("raw candidate GPU owner missing")?
            .drain_ready()?
        {
            let capture_id = self
                .selected_ids
                .remove(&frame.present_seq)
                .ok_or("selected readback lost original capture identity")?;
            super::submit_selected_frame(frame, capture_id, present)?;
        }
        drop(drain_timer);
        if let Some(first) = self.ring.first_materialized_present_seq()
            && let Some(message) = self.pending_arm.take()
        {
            let ack = self.protocol.apply_arm(message, first).map_err(error)?;
            self.ack(ack)?;
        }
        if self.pending_arm.is_some() {
            return Ok(());
        }
        while let Some(message) = self.transport.decisions.try_pop().map_err(error)? {
            let decision = message.payload.clone();
            if matches!(decision, Decision::ArmGeneration) {
                if !self.ready_published || self.camera.is_none() {
                    return Err("raw Arm arrived before local Ready".into());
                }
                self.protocol.stage_arm(&message).map_err(error)?;
                self.ring.arm().map_err(error)?;
                self.performance
                    .checkpoint(Phase::SelectedArm, current_qpc(), present, || {
                        backend.hook_timing_snapshot()
                    });
                self.pending_arm = Some(message);
                break;
            }
            let ack = self.protocol.apply(message).map_err(error)?;
            match decision {
                Decision::Select {
                    present_seq,
                    depth_required,
                    source_qpc,
                    callback_start_qpc,
                    callback_ready_qpc,
                    decision_qpc,
                    ..
                } => {
                    depth_lab::selected_present_feedback::SelectTimestamps {
                        source: source_qpc,
                        callback_start: callback_start_qpc,
                        callback_ready: callback_ready_qpc,
                        decision: decision_qpc,
                        process: current_qpc(),
                    }
                    .classify(runtime.qpc_frequency as u64)
                    .map_err(error)?;
                    if depth_required
                        && let SelectPlan::Submit(plan) = self
                            .ring
                            .select(
                                self.stream.generation,
                                self.stream.resource_epoch,
                                present_seq,
                                present,
                            )
                            .map_err(error)?
                    {
                        let _timer = self.performance.select_readback_submit.start();
                        contract::check_selected_frame_cap(
                            self.ring.stats().distinct_selected,
                            runtime.max_frames,
                        )?;
                        let capture_id = self
                            .candidate_ids
                            .remove(&present_seq)
                            .ok_or("Select has no retained GPU candidate")?;
                        if !self
                            .gpu
                            .as_mut()
                            .ok_or("raw GPU ring missing")?
                            .select(plan.slot, present_seq)?
                        {
                            return Err("raw selected readback pool full".into());
                        }
                        self.selected_ids.insert(present_seq, capture_id);
                        self.ring.commit_submitted(plan).map_err(error)?;
                    }
                }
                Decision::RetireThrough { present_seq } => {
                    self.ring.retire_through(present_seq).map_err(error)?;
                    self.candidate_ids.retain(|seq, _| *seq > present_seq);
                }
                Decision::SealGeneration { .. } => {
                    self.ring.seal().map_err(error)?;
                    self.candidate_ids.clear();
                    self.sealed = true;
                    self.performance.checkpoint(
                        Phase::SelectedSeal,
                        current_qpc(),
                        present,
                        || backend.hook_timing_snapshot(),
                    );
                }
                Decision::ArmGeneration => unreachable!(),
            }
            self.ack(ack)?;
        }
        runtime.feedback_drained.store(
            self.sealed
                && self.selected_ids.is_empty()
                && self.gpu.as_ref().is_some_and(|g| g.in_flight() == 0),
            Ordering::Release,
        );
        Ok(())
    }

    pub(super) fn after_present(
        &mut self,
        runtime: &Runtime,
        present: u64,
        succeeded: bool,
    ) -> Result<(), String> {
        if !succeeded {
            self.fault = Some("raw selected Present failed".into());
            return Err("raw selected Present failed".into());
        }
        if self.fault.is_some() || self.ready_published || self.ready_candidate != Some(present) {
            return Ok(());
        }
        let camera = self
            .camera
            .as_ref()
            .ok_or("local Ready has no camera admission")?;
        let value = serde_json::json!({"schema":contract::READY_SCHEMA,"process_launch_id":runtime.launch_id,
            "profile_sha256":runtime.profile_sha256,"non_trainable":true,"production_certified":false,"recording_ready":false,
            "generation":self.stream.generation,"resource_epoch":self.stream.resource_epoch,"first_present_seq":present,
            "feedback_schema":FEEDBACK_SCHEMA_V4,"depth_selection":contract::DEPTH_SELECTION,
            "watermark_source_width":runtime.width,"watermark_source_height":runtime.height,
            "candidate_materialized":self.ring.stats().materialized,"camera_admission":camera.summary()});
        publish_json(&runtime.output_dir, contract::READY_FILE, &value)?;
        self.ready_published = true;
        Ok(())
    }

    pub(super) fn seal_report(&mut self, runtime: &Runtime, written: u64) -> Result<(), String> {
        if self.fault.is_none() {
            self.fault = runtime
                .fatal
                .lock()
                .map_err(|_| "raw fatal lock poisoned")?
                .clone();
        }
        if self.fault.is_none()
            && let Err(e) = self.ring.begin_stop()
        {
            self.fault = Some(error(e));
        }
        if let Some(g) = self.gpu.as_mut()
            && let Err(e) = g.refresh_final_stats()
        {
            self.fault = Some(e);
        }
        let stats = self.ring.stats();
        let decisions = self.transport.decisions.stats().map_err(error)?;
        let acks = self.transport.acks.stats().map_err(error)?;
        let completed = self.gpu.as_ref().map_or(0, |g| g.completed());
        if self.fault.is_none()
            && (self.protocol.counts().1 != stats.distinct_selected
                || stats.selected_messages != self.protocol.counts().0.div_ceil(2)
                || completed != stats.submitted
                || stats.submitted != runtime.submitted_frames.load(Ordering::Acquire)
                || written != stats.submitted
                || self.ring.unresolved() != 0
                || decisions.depth != 0
                || acks.depth != 0
                || decisions.fatal_reason != 0
                || acks.fatal_reason != 0)
        {
            self.fault =
                Some("raw selected-Present final queue/readback/writer counts disagree".into());
        }
        let allocation=self.gpu.as_ref().map(|g| {let s=g.stats(); serde_json::json!({
            "capacity":s.capacity,"estimated_bytes":s.estimated_bytes,"budget_before":s.budget_before,
            "usage_before":s.usage_before,"headroom_before":s.headroom_before,"headroom_after":s.headroom_after,
            "actual_usage_delta":s.actual_usage_delta,"headroom_at_flush":s.headroom_at_flush})});
        let value = serde_json::json!({"schema":contract::SUMMARY_SCHEMA,"non_trainable":true,"recording_ready":false,
            "feedback_schema":FEEDBACK_SCHEMA_V4,"depth_selection":contract::DEPTH_SELECTION,
            "process_launch_id":runtime.launch_id,"generation":self.stream.generation,"resource_epoch":self.stream.resource_epoch,
            "first_candidate_present_seq":self.ring.first_materialized_present_seq(),"materialized_candidates":stats.materialized,
            "selected_rgb_frames":self.protocol.counts().0,"required_depth_rgb_frames":stats.selected_messages,
            "skipped_depth_rgb_frames":self.protocol.counts().0.saturating_sub(stats.selected_messages),
            "distinct_selected_depth":stats.distinct_selected,
            "aliases":stats.source_identity_aliases,"readback_submitted":stats.submitted,"readback_completed":completed,
            "written_depth_frames":written,
            "unresolved":self.ring.unresolved(),"max_unresolved":stats.max_unresolved,"max_lag_presents":stats.max_feedback_lag_presents,
            "decision_queue_depth":decisions.depth,"ack_queue_depth":acks.depth,"sealed":self.sealed,"fault":self.fault,
            "capture_start_qpc":runtime.feedback_capture_start_qpc.load(Ordering::Acquire),
            "camera_admission":self.camera.as_ref().map(|c|c.summary()),"gpu_allocation":allocation,
            "performance":self.performance.report(runtime.qpc_frequency)});
        publish_json(&runtime.output_dir, contract::SUMMARY_FILE, &value)?;
        self.fault.clone().map_or(Ok(()), Err)
    }
}

fn publish_json(dir: &Path, name: &str, value: &serde_json::Value) -> Result<(), String> {
    let pending = dir.join(format!("{name}.pending"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
        .map_err(error)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(error)?)
        .map_err(error)?;
    file.sync_all().map_err(error)?;
    drop(file);
    let final_path = dir.join(name);
    if final_path.exists() {
        return Err("refusing to replace immutable raw selected report".into());
    }
    std::fs::rename(pending, final_path).map_err(error)
}
