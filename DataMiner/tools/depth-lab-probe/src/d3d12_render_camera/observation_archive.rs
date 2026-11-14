//! A bounded active window; ownership moves to the existing raw writer, never disappears.
use super::*;

// Reuse the bounded active window to coalesce small per-Present archive jobs.
// The same 32-slot writer also receives depth frames; a job per Present can
// exhaust it during a short durable-write stall. No extra queue or thread.
const ARCHIVE_BATCH_PRESENTS: u64 = 8;
const ARCHIVE_BATCH_ROWS: usize = 256;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ArchiveState {
    pub schema: &'static str,
    pub enqueued_draws: u64,
    pub enqueued_batches: u64,
    pub enqueue_failures: u64,
    pub retired_before_present: u64,
    pub frozen: bool,
}

impl RenderCameraTracker {
    // Only the diagnostic arm path (and tests) opens the window; other builds never call it.
    #[cfg(any(test, all(feature = "cp2077-diagnostics", debug_assertions)))]
    pub(crate) fn enable_local_observation_archive(&mut self) {
        self.snapshot.local_archive = Some(ArchiveState {
            schema: "dm-cp2077-camera-active-window/v1",
            ..Default::default()
        });
    }

    pub(crate) fn freeze_local_observation_archive(&mut self) {
        if let Some(state) = self.snapshot.local_archive.as_mut() {
            state.frozen = true;
        }
    }

    pub(crate) fn archive_local_before(
        &mut self,
        present: u64,
        enqueue: impl FnOnce(
            Vec<D3d12RenderDrawObservation>,
        ) -> Result<(), Vec<D3d12RenderDrawObservation>>,
    ) -> Result<(), String> {
        let Some(state) = self.snapshot.local_archive.as_mut() else {
            return Ok(());
        };
        if state.frozen {
            return Ok(());
        }
        if present < state.retired_before_present {
            return Err("local camera archive Present moved backwards".into());
        }
        if self
            .snapshot
            .observations
            .windows(2)
            .any(|w| w[0].present_seq > w[1].present_seq)
        {
            return Err("local camera archive observations are unordered".into());
        }
        let cut = self
            .snapshot
            .observations
            .partition_point(|d| d.present_seq < present);
        if cut == 0 {
            return Ok(());
        }
        if self.snapshot.observations[cut..]
            .iter()
            .any(|d| d.present_seq < present)
        {
            return Err("local camera archive observations are unordered".into());
        }
        if cut < ARCHIVE_BATCH_ROWS
            && present.saturating_sub(self.snapshot.observations[0].present_seq)
                < ARCHIVE_BATCH_PRESENTS
        {
            // Keep every row in the existing 4096-row window. Freeze preserves
            // a partial batch in the final report's tail; no stop-time flush
            // or change to per-batch writer sync/SHA validation is needed.
            return Ok(());
        }
        let current = self.snapshot.observations.split_off(cut);
        let older = std::mem::replace(&mut self.snapshot.observations, current);
        if let Err(mut returned) = enqueue(older) {
            // try_send returns ownership on failure. Keep the exact evidence and fail closed.
            returned.append(&mut self.snapshot.observations);
            self.snapshot.observations = returned;
            state.enqueue_failures = state.enqueue_failures.saturating_add(1);
            return Err("local camera archive writer queue unavailable".into());
        }
        state.enqueued_draws += cut as u64;
        state.enqueued_batches += 1;
        state.retired_before_present = present;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(present: u64, ordinal: u32) -> D3d12RenderDrawObservation {
        D3d12RenderDrawObservation {
            present_seq: present,
            present_qpc: present as i64 * 100,
            draw_ordinal: ordinal,
            ..Default::default()
        }
    }

    // Use the real retirement callback and a bounded FIFO. A stopped consumer
    // models sync_all blocking without sleeping, allocating depth images or
    // depending on disk speed. This is not a replay of OS I/O timings.
    fn stalled_mixed_writer(stall_presents: u64, phase: u64) -> Result<(usize, usize), String> {
        enum Command {
            Depth(u64),
            Camera(Vec<D3d12RenderDrawObservation>),
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.enable_local_observation_archive();
        let mut archived = Vec::new();
        let mut depths = Vec::new();
        let mut queued = 0usize;
        let mut high_water = 0usize;
        let stall_start = 17 + phase;
        for present in 1..=160 {
            t.snapshot
                .observations
                .extend((1..=27).map(|n| draw(present, n)));
            t.archive_local_before(present, |rows| match tx.try_send(Command::Camera(rows)) {
                Ok(()) => {
                    queued += 1;
                    Ok(())
                }
                Err(std::sync::mpsc::TrySendError::Full(Command::Camera(rows))) => Err(rows),
                _ => panic!("consumer remains connected"),
            })?;
            if present % 2 == 0 {
                tx.try_send(Command::Depth(present))
                    .map_err(|_| "raw depth writer queue filled".to_owned())?;
                queued += 1;
            }
            high_water = high_water.max(queued);
            if !(stall_start..stall_start + stall_presents).contains(&present) {
                for command in rx.try_iter() {
                    match command {
                        Command::Depth(present) => depths.push(present),
                        Command::Camera(rows) => archived.extend(rows),
                    }
                    queued -= 1;
                }
            }
        }
        assert_eq!(queued, 0);
        assert_eq!(depths, (2..=160).step_by(2).collect::<Vec<_>>());
        t.freeze_local_observation_archive();
        let tail = t.snapshot.observations.clone();
        t.archive_local_before(161, |_| panic!("frozen tail belongs in final report"))?;
        assert_eq!(t.snapshot.observations, tail);
        archived.extend(tail);
        let expected = (1..=160)
            .flat_map(|p| (1..=27).map(move |n| draw(p, n)))
            .collect::<Vec<_>>();
        assert_eq!(archived, expected, "every draw must survive once, in order");
        Ok((
            high_water,
            t.snapshot.local_archive.unwrap().enqueued_batches as usize,
        ))
    }

    #[test]
    fn camera_archive_batching_survives_short_mixed_writer_stall() {
        // 30 Hz Presents / 15 Hz depth; 48 ticks pause the writer for 1.6 s,
        // covering the observed 1.57 s depth-file write. Exercise every batch
        // phase, including a stall starting next to an archive enqueue.
        for phase in 0..8 {
            let (high_water, batches) = stalled_mixed_writer(48, phase).unwrap();
            assert!(high_water <= 32);
            assert!(batches <= 20);
            eprintln!("phase={phase} queue_high_water={high_water} archive_batches={batches}");
        }
    }

    #[test]
    fn camera_archive_batching_still_fails_when_writer_stall_exceeds_capacity() {
        let error = stalled_mixed_writer(80, 0).unwrap_err();
        assert!(error.contains("writer queue"), "{error}");
    }

    #[test]
    fn authoring_rebind_keeps_history_and_refuses_pending_deferred_or_frozen_camera_work() {
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
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        assert!(
            t.rebind_authoring_source(new).is_err(),
            "ordinary discovery is not a phase transition"
        );
        t.enable_local_observation_archive();
        t.arm_selected_depth(old);
        t.register_list(0x100, 1);
        t.snapshot.observations.push(draw(42, 1));
        t.next_execute_batch = 90;
        t.next_copy_submission = 80;
        t.snapshot.pending_draw_overflow = 3;
        let before = t.snapshot.clone();
        t.rebind_authoring_source(new).unwrap();
        assert_eq!(
            t.snapshot, before,
            "source transition must not erase history or failures"
        );
        assert_eq!(t.selected_depth, Some(new));
        assert_eq!((t.next_execute_batch, t.next_copy_submission), (90, 80));
        t.lists
            .get_mut(&0x100)
            .unwrap()
            .deferred_copy_draws
            .push(DeferredCopyDraw {
                pass_context: None,
                pipeline_depth: None,
                draw_ordinal: 1,
                pass_ordinal: 1,
                draw_ordinal_in_pass: 1,
                root_signature_sha256: None,
                selected_depth: new,
                bindings: vec![],
            });
        assert!(t.rebind_authoring_source(old).is_err());
        assert_eq!(t.lists[&0x100].deferred_copy_draws.len(), 1);
        t.materialize_deferred_copy_draws(QueueId(1), &[0x100]);
        t.execute(QueueId(1), &[0x100]);
        t.present(QueueId(1), 43, 4300);
        t.mark_depth_copy_submitted_in_list(QueueId(1), 44, Some(0x100), Some(0), Some(1));
        assert!(
            t.rebind_authoring_source(old).is_err(),
            "unresolved copy/Present must drain"
        );
        t.present(QueueId(1), 44, 4400);
        t.rebind_authoring_source(old).unwrap();
        t.freeze_local_observation_archive();
        assert!(
            t.rebind_authoring_source(new).is_err(),
            "a sealed session cannot resume"
        );
    }

    #[test]
    fn camera_archive_retirement_is_bounded_and_transactional() {
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.enable_local_observation_archive();
        let mut archived = Vec::new();
        for p in 1..=1500 {
            t.snapshot.observations.extend((1..=27).map(|n| draw(p, n)));
            t.archive_local_before(p, |rows| {
                archived.extend(rows);
                Ok(())
            })
            .unwrap();
            assert!(t.snapshot.observations.len() <= 8 * 27);
            assert_eq!(t.snapshot.observations.last().unwrap().present_seq, p);
        }
        assert_eq!(archived.len() + t.snapshot.observations.len(), 1500 * 27);
        assert_eq!(t.snapshot.present_observation_overflow, 0);
        assert_eq!(
            t.snapshot.local_archive.as_ref().unwrap().enqueued_draws,
            archived.len() as u64
        );
        for (i, d) in archived.iter().enumerate() {
            assert_eq!(
                (d.present_seq, d.draw_ordinal),
                ((i / 27 + 1) as u64, (i % 27 + 1) as u32)
            );
        }
        let before = t.snapshot.clone();
        assert!(t.archive_local_before(1508, Err).is_err());
        assert_eq!(t.snapshot.observations, before.observations);
        assert_eq!(
            t.snapshot.local_archive.as_ref().unwrap().enqueued_draws,
            before.local_archive.unwrap().enqueued_draws
        );
        assert_eq!(
            t.snapshot.local_archive.as_ref().unwrap().enqueue_failures,
            1
        );
        t.freeze_local_observation_archive();
        t.archive_local_before(1600, |_| panic!("frozen records must stay"))
            .unwrap();
        t.present(QueueId(1), 1600, 160000);
        assert_eq!(t.snapshot.observations, before.observations);
        assert_eq!(
            t.snapshot.local_archive.as_ref().unwrap().enqueue_failures,
            1
        );
    }

    #[test]
    fn camera_archive_row_limit_flushes_early_and_preserves_current_present() {
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.enable_local_observation_archive();
        t.snapshot
            .observations
            .extend((1..=256).map(|n| draw(10, n)));
        t.snapshot.observations.push(draw(11, 1));
        let mut archived = Vec::new();
        t.archive_local_before(11, |rows| {
            archived = rows;
            Ok(())
        })
        .unwrap();
        assert_eq!(archived, (1..=256).map(|n| draw(10, n)).collect::<Vec<_>>());
        assert_eq!(t.snapshot.observations, [draw(11, 1)]);
        assert!(t.archive_local_before(10, |_| panic!("backwards")).is_err());
    }

    #[test]
    fn camera_archive_sparse_rows_flush_at_present_limit() {
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.enable_local_observation_archive();
        t.snapshot.observations.push(draw(100, 1));
        for present in 101..108 {
            t.archive_local_before(present, |_| panic!("batch not due"))
                .unwrap();
            assert_eq!(t.snapshot.observations, [draw(100, 1)]);
        }
        let mut archived = Vec::new();
        t.archive_local_before(108, |rows| {
            archived = rows;
            Ok(())
        })
        .unwrap();
        assert_eq!(archived, [draw(100, 1)]);
        assert!(t.snapshot.observations.is_empty());
    }

    #[test]
    fn camera_archive_does_not_apply_to_discovery_or_accept_reordered_rows() {
        let mut t = RenderCameraTracker::new(1920, 1080, 75.0);
        t.snapshot.observations.push(draw(3, 1));
        t.archive_local_before(4, |_| panic!("non-streaming discovery must not retire"))
            .unwrap();
        t.enable_local_observation_archive();
        t.snapshot.observations.push(draw(2, 1));
        assert!(
            t.archive_local_before(4, |_| Ok(()))
                .unwrap_err()
                .contains("unordered")
        );
        assert_eq!(t.snapshot.observations.len(), 2);
    }
}
