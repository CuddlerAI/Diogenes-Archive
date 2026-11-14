//! Diagnostic command-recording boundaries, never an admission input.
//! Draw N has order key [N, 0, 0]. A boundary after N observed draw calls has
//! [N, 1, boundary_ordinal]. This preserves clears/copies between adjacent draws
//! without another atomic operation on every draw. Cross-list order uses the
//! SAME queue/Execute/list indices as the camera report; cross-queue is unknown.

use super::*;

const MAX_RECORDED: usize = 16_384;
const MAX_SUBMITTED: usize = 32_768;
pub const MAX_CLEAR_RECTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoundaryKind {
    Clear {
        flags: u32,
        depth_value_bits: u32,
        stencil: u8,
        rect_count: u32,
        rects: Vec<[i32; 4]>,
        rects_complete: bool,
    },
    Transition {
        before: u32,
        after: u32,
        subresource: u32,
        flags: u32,
    },
    CopyCandidate {
        interval_present_seq: Option<u64>,
        requested: bool,
        already_captured: bool,
        result: &'static str,
        capture_id: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Boundary {
    pub command_list_tag: u64,
    pub command_list_generation: u64,
    pub resource_tag: u64,
    pub resource_epoch: u64,
    pub descriptor_generation: u64,
    pub order_key: Option<[u64; 3]>,
    pub event: BoundaryKind,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubmittedBoundary {
    pub queue_tag: u64,
    pub execute_batch: u64,
    pub list_index_in_batch: u32,
    pub execution_of_recording: u64,
    #[serde(flatten)]
    pub boundary: Boundary,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DepthOrderSnapshot {
    pub schema: &'static str,
    pub enabled: bool,
    pub non_trainable: bool,
    pub recorded_capacity: usize,
    pub submitted_capacity: usize,
    pub dropped_recorded: u64,
    pub dropped_submitted: u64,
    pub unknown_list: u64,
    pub unknown_order: u64,
    pub discarded_unsubmitted: u64,
    pub unsubmitted_boundaries: Vec<Boundary>,
    pub boundaries: Vec<SubmittedBoundary>,
}

impl Default for DepthOrderSnapshot {
    fn default() -> Self {
        Self {
            schema: "dm-d3d12-depth-order/v1",
            enabled: false,
            non_trainable: true,
            recorded_capacity: MAX_RECORDED,
            submitted_capacity: MAX_SUBMITTED,
            dropped_recorded: 0,
            dropped_submitted: 0,
            unknown_list: 0,
            unknown_order: 0,
            discarded_unsubmitted: 0,
            boundaries: Vec::new(),
            unsubmitted_boundaries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct ListBoundaries {
    ordinal: u64,
    pub(super) executions: u64,
    pub(super) events: Vec<Boundary>,
}

impl RenderCameraTracker {
    pub fn depth_order_diagnostics_enabled(&self) -> bool {
        self.snapshot.depth_order.enabled
    }

    pub fn enable_depth_order_diagnostics(&mut self) {
        self.snapshot.depth_order = DepthOrderSnapshot {
            enabled: true,
            ..Default::default()
        };
        self.depth_boundary_count = 0;
        for list in self.lists.values_mut() {
            list.depth_boundaries = ListBoundaries::default();
        }
    }

    pub fn record_depth_boundary(
        &mut self,
        raw: usize,
        identity: DsvIdentity,
        draws_before: Option<u32>,
        event: BoundaryKind,
    ) {
        let report = &mut self.snapshot.depth_order;
        if !report.enabled
            || !self.selected_depth.is_some_and(|selected| {
                selected.resource == identity.resource
                    && selected.resource_epoch == identity.resource_epoch
            })
        {
            return;
        }
        let Some(list) = self.lists.get_mut(&raw) else {
            report.unknown_list = report.unknown_list.saturating_add(1);
            return;
        };
        list.depth_boundaries.ordinal = list.depth_boundaries.ordinal.saturating_add(1);
        if self.depth_boundary_count >= MAX_RECORDED {
            report.dropped_recorded = report.dropped_recorded.saturating_add(1);
            return;
        }
        let order_key = draws_before.map(|n| [u64::from(n), 1, list.depth_boundaries.ordinal]);
        if order_key.is_none() {
            report.unknown_order = report.unknown_order.saturating_add(1);
        }
        list.depth_boundaries.events.push(Boundary {
            command_list_tag: list.list_tag,
            command_list_generation: list.list_generation,
            resource_tag: identity.resource.0,
            resource_epoch: identity.resource_epoch,
            descriptor_generation: identity.descriptor_generation,
            order_key,
            event,
        });
        self.depth_boundary_count += 1;
    }

    pub(super) fn submit_depth_boundaries(&mut self, queue: QueueId, batch: u64, lists: &[usize]) {
        if !self.snapshot.depth_order.enabled {
            return;
        }
        for (index, raw) in lists.iter().enumerate() {
            let Some(list) = self.lists.get_mut(raw) else {
                continue;
            };
            list.depth_boundaries.executions += 1;
            for boundary in &list.depth_boundaries.events {
                let report = &mut self.snapshot.depth_order;
                if report.boundaries.len() >= MAX_SUBMITTED {
                    report.dropped_submitted = report.dropped_submitted.saturating_add(1);
                    continue;
                }
                report.boundaries.push(SubmittedBoundary {
                    queue_tag: queue.0,
                    execute_batch: batch,
                    list_index_in_batch: index.try_into().unwrap_or(u32::MAX),
                    execution_of_recording: list.depth_boundaries.executions,
                    boundary: boundary.clone(),
                });
            }
        }
    }

    pub(super) fn discard_depth_boundaries(&mut self, raw: usize) {
        if let Some(list) = self.lists.get_mut(&raw) {
            if list.depth_boundaries.executions == 0 {
                self.snapshot.depth_order.discarded_unsubmitted = self
                    .snapshot
                    .depth_order
                    .discarded_unsubmitted
                    .saturating_add(list.depth_boundaries.events.len() as u64);
            }
            self.depth_boundary_count -= list.depth_boundaries.events.len();
            list.depth_boundaries = ListBoundaries::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected_depth() -> DsvIdentity {
        use crate::d3d12_abi::{DescriptorHandle, ResourceId};
        DsvIdentity {
            handle: DescriptorHandle(4),
            descriptor_generation: 2,
            resource: ResourceId(7),
            resource_epoch: 3,
        }
    }
    fn tracker() -> RenderCameraTracker {
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.arm_selected_depth(selected_depth());
        t.enable_depth_order_diagnostics();
        t.register_list(1, 11);
        t.register_list(2, 22);
        t
    }
    fn clear(t: &mut RenderCameraTracker, raw: usize, n: Option<u32>, flags: u32) {
        t.record_depth_boundary(
            raw,
            selected_depth(),
            n,
            BoundaryKind::Clear {
                flags,
                depth_value_bits: 0,
                stencil: 0,
                rect_count: 0,
                rects: vec![],
                rects_complete: true,
            },
        );
    }
    #[test]
    fn clear_between_draws_and_copy_preserves_stencil_and_local_order() {
        let mut t = tracker();
        clear(&mut t, 1, Some(3), 2);
        clear(&mut t, 1, Some(3), 1);
        t.record_depth_boundary(
            1,
            selected_depth(),
            Some(4),
            BoundaryKind::CopyCandidate {
                interval_present_seq: Some(17),
                requested: true,
                already_captured: false,
                result: "recorded",
                capture_id: Some(2),
            },
        );
        t.execute(QueueId(1), &[1]);
        let b = &t.snapshot.depth_order.boundaries;
        assert!([3, 0, 0] < b[0].boundary.order_key.unwrap());
        assert!(b[0].boundary.order_key < b[1].boundary.order_key);
        assert!(b[1].boundary.order_key.unwrap() < [4, 0, 0]);
        assert!([4, 0, 0] < b[2].boundary.order_key.unwrap());
        assert!(matches!(
            b[0].boundary.event,
            BoundaryKind::Clear { flags: 2, .. }
        ));
        assert!(t.snapshot.observations.is_empty()); // diagnostics never author camera evidence
    }
    #[test]
    fn execute_order_reset_and_reexecution_do_not_relabel_old_boundaries() {
        let mut t = tracker();
        clear(&mut t, 1, Some(1), 1);
        clear(&mut t, 2, None, 1);
        t.execute(QueueId(7), &[2, 1]);
        t.execute(QueueId(7), &[1]);
        t.reset_list(1);
        clear(&mut t, 1, Some(0), 1);
        t.execute(QueueId(7), &[1]);
        let b = &t.snapshot.depth_order.boundaries;
        assert_eq!(b[0].boundary.command_list_tag, 22);
        assert_eq!(b[1].list_index_in_batch, 1);
        assert_eq!(b[2].execution_of_recording, 2);
        assert_eq!(b[2].boundary.command_list_generation, 1);
        assert_eq!(b[3].boundary.command_list_generation, 2);
        assert_eq!(b[3].execution_of_recording, 1);
        assert_eq!(t.snapshot.depth_order.unknown_order, 1);
        t.release_list(1);
        t.release_list(2);
        assert_eq!(t.depth_boundary_count, 0);
    }
    #[test]
    fn partial_clear_and_submission_overflow_cannot_look_complete() {
        let mut t = tracker();
        t.record_depth_boundary(
            1,
            selected_depth(),
            Some(0),
            BoundaryKind::Clear {
                flags: 1,
                depth_value_bits: 0.5_f32.to_bits(),
                stencil: 3,
                rect_count: 9,
                rects: vec![[0, 0, 100, 100]; MAX_CLEAR_RECTS],
                rects_complete: false,
            },
        );
        t.execute(QueueId(1), &[1]);
        let event = t.snapshot.depth_order.boundaries[0].clone();
        t.snapshot
            .depth_order
            .boundaries
            .resize(MAX_SUBMITTED, event);
        t.execute(QueueId(1), &[1]);
        assert_eq!(t.snapshot.depth_order.dropped_submitted, 1);
        let json = serde_json::to_value(&t.snapshot.depth_order.boundaries[0]).unwrap();
        assert_eq!(json["event"]["rects_complete"], false);
        assert_eq!(json["event"]["rect_count"], 9);
        t.reset_list(1);
        clear(&mut t, 1, Some(0), 1);
        t.release_list(1);
        assert_eq!(t.snapshot.depth_order.discarded_unsubmitted, 1);
    }

    #[test]
    fn limits_disabled_and_unknown_are_explicit() {
        let mut t = tracker();
        clear(&mut t, 99, Some(1), 1);
        t.depth_boundary_count = MAX_RECORDED;
        clear(&mut t, 1, Some(1), 1);
        assert_eq!(t.snapshot.depth_order.unknown_list, 1);
        assert_eq!(t.snapshot.depth_order.dropped_recorded, 1);
        t.snapshot.depth_order.enabled = false;
        clear(&mut t, 1, Some(1), 1);
        assert_eq!(t.snapshot.depth_order.dropped_recorded, 1);
    }
}
