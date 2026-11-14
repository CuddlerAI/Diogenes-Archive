//! Opt-in holstered raw diagnostics. Production/cross-launch gates remain separate.
use super::*;
use depth_lab::d3d12_camera_alias::Sample;
use depth_lab::d3d12_camera_copy_series::{DrawIdentity, Observation};

pub type LocalSamples = (Vec<Observation>, Vec<Observation>);

impl RenderCameraTracker {
    pub(crate) fn local_camera_samples(
        &self,
        present: u64,
        width: u32,
        height: u32,
    ) -> Result<LocalSamples, String> {
        self.snapshot.local_camera_samples(present, width, height)
    }
}

impl D3d12RenderCameraSnapshot {
    pub(crate) fn local_camera_samples(
        &self,
        present: u64,
        width: u32,
        height: u32,
    ) -> Result<LocalSamples, String> {
        if !self.selected_depth_armed
            || !self.blockers.is_empty()
            || self
                .local_archive
                .as_ref()
                .is_some_and(|s| s.enqueue_failures != 0)
            || [
                self.matrix_candidate_overflow,
                self.pending_draw_overflow,
                self.draw_scan_budget_exhausted,
                self.present_observation_overflow,
                self.tracker_contention_drops,
                self.hot_state_drops,
                self.upload_unmap_snapshot_budget_exhausted,
                self.exact_cbv_copy_span_budget_exhausted,
                self.exact_cbv_copy_request_budget_exhausted,
                self.exact_cbv_copy_shadow_budget_exhausted,
            ]
            .iter()
            .any(|&count| count != 0)
        {
            return Err("local camera lost bounded selected-draw evidence".into());
        }
        self.local_camera_sources(present, width, height)
    }

    /// Source-policy layer only. Runtime callers must go through local_camera_samples and
    /// its complete loss gates. Separate visibility allows explicit layer-only raw replay.
    pub(super) fn local_camera_sources(
        &self,
        present: u64,
        width: u32,
        height: u32,
    ) -> Result<LocalSamples, String> {
        let mut view = Vec::new();
        let mut projection = Vec::new();
        let mut writer_view = Vec::new();
        let mut writer_projection = Vec::new();
        for draw in self.observations.iter().filter(|d| {
            d.present_seq == present
                && d.selected_present_identity_passed
                && d.copy_before_clear_submitted
        }) {
            let context = draw
                .pass_context
                .as_ref()
                .ok_or("local holstered camera requires draw-time viewport evidence")?;
            if context.schema != "dm-d3d12-draw-pass/v1" || !context.non_trainable {
                return Err("local camera draw-pass provenance has an unsupported contract".into());
            }
            let bits = context
                .viewport
                .filter(|v| v.count == 1)
                .and_then(|v| v.single_bits)
                .ok_or("local holstered camera viewport is unknown")?;
            let values = bits.map(f32::from_bits);
            if values != [0.0, 0.0, width as f32, height as f32, 0.0, 1.0]
                || context.dsv_flags != Some(0)
            {
                return Err("local holstered camera requires an uncompressed 0..1 depth viewport and writable DSV".into());
            }
            if draw.physical_source
                != "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
            {
                return Err("local camera draw lacks exact executed CBV provenance".into());
            }
            let pipeline = context
                .pipeline
                .as_ref()
                .ok_or("local camera requires known draw-time depth-write state")?;
            let enabled = draw
                .pipeline_depth_enable
                .ok_or("local camera requires known draw-time depth-enable state")?;
            if !matches!(pipeline.depth_write_mask, 0 | 1)
                || draw.pipeline_depth_write != Some(enabled && pipeline.depth_write_mask == 1)
                || pipeline.dsv_format != 20
            {
                return Err("local camera depth-write evidence disagrees".into());
            }
            let contributes = enabled && pipeline.depth_write_mask == 1;
            let identity = DrawIdentity {
                present_qpc: draw.present_qpc,
                queue_tag: draw.queue_tag,
                command_list_tag: draw.command_list_tag,
                command_list_generation: draw.command_list_generation,
                selected_depth_resource_tag: draw.selected_depth_resource_tag,
                selected_depth_resource_epoch: draw.selected_depth_resource_epoch,
                selected_depth_descriptor_generation: draw.selected_depth_descriptor_generation,
                draw_ordinal: draw.draw_ordinal,
            };
            let (view_start, projection_start) = (view.len(), projection.len());
            for m in &draw.matrices {
                let role = match m.matrix_kind.as_str() {
                    "view" => MatrixRole::View,
                    "projection" => MatrixRole::Projection,
                    _ => continue,
                };
                let source = m
                    .stable_source
                    .as_ref()
                    .ok_or("local camera matrix lacks a stable binding")?;
                let receipt = m
                    .copy_receipt
                    .as_ref()
                    .ok_or("local camera matrix lacks a copy receipt")?;
                if m.physical_source != "copy_buffer_region_root_signature_declared_cbv"
                    || source.role != role
                    || m.source_tag != receipt.destination_source_tag
                    || receipt.queue_tag != draw.queue_tag
                    || m.root_parameter_index != source.descriptor_table.root_parameter_index
                    || m.cbv_resource_offset != source.cbv_resource_offset
                    || m.matrix_offset != source.matrix_offset
                    || m.transpose != source.transpose
                    || (role == MatrixRole::Projection
                        && m.configured_vertical_fov_matches != Some(true))
                {
                    return Err("local camera physical/binding/FOV identity disagrees".into());
                }
                let observation = Observation {
                    draw: identity.clone(),
                    sample: Sample {
                        present,
                        source: source.clone(),
                        matrix: m.matrix.map(|row| row.map(f64::from)),
                        receipt: Some(receipt.clone()),
                    },
                };
                if role == MatrixRole::View {
                    view.push(observation);
                } else {
                    projection.push(observation);
                }
            }
            if view.len() == view_start || projection.len() == projection_start {
                return Err("an admitted draw lacks its own physical view/projection pair".into());
            }
            if contributes {
                writer_view.extend_from_slice(&view[view_start..]);
                writer_projection.extend_from_slice(&projection[projection_start..]);
            } else {
                // A known non-writer cannot supply depth-camera provenance. Still validate
                // its own copies/bindings and retain it as a matrix-conflict witness.
                let vg = depth_lab::d3d12_camera_copy_series::resolve(view[view_start..].to_vec())
                    .map_err(|e| e.to_string())?;
                let pg = depth_lab::d3d12_camera_copy_series::resolve(
                    projection[projection_start..].to_vec(),
                )
                .map_err(|e| e.to_string())?;
                if vg.len() != pg.len() {
                    return Err("non-writing draw camera pair coverage differs".into());
                }
                for (v, p) in vg.iter().zip(&pg) {
                    depth_lab::d3d12_camera_copy_series::validate_pair(v, p)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        if view.is_empty() || projection.is_empty() {
            return Err("local camera is absent at the exact copied Present".into());
        }
        for rows in [&view, &projection] {
            depth_lab::d3d12_camera_copy_series::validate_physical_observations(rows)
                .map_err(|e| e.to_string())?;
            if rows
                .iter()
                .any(|r| r.sample.matrix != rows[0].sample.matrix)
            {
                return Err("local camera matrices disagree across selected draws".into());
            }
        }
        if writer_view.is_empty() || writer_projection.is_empty() {
            return Err(
                "local camera has no proven depth-writing draw at the exact copied Present".into(),
            );
        }
        Ok((writer_view, writer_projection))
    }
}
