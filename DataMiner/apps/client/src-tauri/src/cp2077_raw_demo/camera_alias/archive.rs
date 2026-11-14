//! Offline archived evidence only. Reuse bounded same-launch admission, then check every
//! subsequent Present. Raw observations remain in the verified report, never truncated.
use super::*;
use depth_lab::d3d12_camera_local::{LocalCamera, MIN_PRESENTS, admit};
use std::collections::{BTreeMap, BTreeSet};

const MAX_BATCH_OBSERVATIONS: usize = 32_768;

#[derive(Default)]
pub(in crate::cp2077_raw_demo) struct ArchiveCamera {
    present: Option<u64>,
    frame_views: Vec<Observation>,
    frame_projections: Vec<Observation>,
    writer_views: Vec<Observation>,
    writer_projections: Vec<Observation>,
    warmup_views: Vec<Observation>,
    warmup_projections: Vec<Observation>,
    required: BTreeSet<u64>,
    admitted: Option<LocalCamera>,
    // Keep cross-Present copy-submission consistency while retiring observation batches.
    // This compact ledger is bounded by the already checked archive/expanded byte limits.
    receipts: BTreeMap<u64, CopyReceipt>,
    result: ArchiveMatrices,
}

#[derive(Default)]
pub(in crate::cp2077_raw_demo) struct ArchiveMatrices {
    pub views: RenderMatrixSamples,
    pub projections: RenderMatrixSamples,
    pub view_sources: Vec<depth_lab::D3d12MatrixSourceBinding>,
    pub projection_sources: Vec<depth_lab::D3d12MatrixSourceBinding>,
    pub retained_views: usize,
    pub retained_projections: usize,
}

impl ArchiveCamera {
    pub(in crate::cp2077_raw_demo) fn push_draw(&mut self, draw: &Value) -> anyhow::Result<()> {
        // Same holstered writer/witness policy as the live local_camera_sources path.
        let pass = &draw["pass_context"];
        ensure!(
            pass["schema"] == "dm-d3d12-draw-pass/v1"
                && pass["non_trainable"] == true
                && pass["viewport"]["count"] == 1
                && pass["dsv_flags"] == 0,
            "archived camera lacks writable draw-time viewport evidence"
        );
        let bits: [u32; 6] = serde_json::from_value(pass["viewport"]["single_bits"].clone())?;
        ensure!(
            bits.map(f32::from_bits) == [0.0, 0.0, 1920.0, 1080.0, 0.0, 1.0],
            "archived camera viewport is compressed or changed"
        );
        let enabled = draw["pipeline_depth_enable"]
            .as_bool()
            .context("unknown depth-enable state")?;
        let mask = pass["pipeline"]["depth_write_mask"]
            .as_u64()
            .context("unknown depth-write state")?;
        ensure!(
            mask <= 1
                && pass["pipeline"]["dsv_format"] == 20
                && draw["pipeline_depth_write"].as_bool() == Some(enabled && mask == 1),
            "archived camera depth-write evidence disagrees"
        );
        ensure!(
            draw["physical_source"]
                == "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw",
            "archived camera lacks executed copy provenance"
        );
        let writer = enabled && mask == 1;
        let mut views = Vec::new();
        let mut projections = Vec::new();
        for matrix in draw["matrices"]
            .as_array()
            .context("missing camera matrices")?
        {
            let role = match matrix["matrix_kind"].as_str() {
                Some("view") => depth_lab::MatrixRole::View,
                Some("projection") => {
                    ensure!(
                        matrix["configured_vertical_fov_matches"] == true
                            && matrix["projection_a"].as_f64().is_some_and(f64::is_finite)
                            && matrix["projection_b"].as_f64().is_some_and(f64::is_finite),
                        "archived camera projection/FOV evidence disagrees"
                    );
                    depth_lab::MatrixRole::Projection
                }
                _ => continue,
            };
            let row = sample(matrix, draw, role)?;
            if role == depth_lab::MatrixRole::View {
                views.push(row);
            } else {
                projections.push(row);
            }
        }
        ensure!(
            !views.is_empty() && !projections.is_empty(),
            "archived draw lacks its own view/projection pair"
        );
        if !writer {
            let vg = d3d12_camera_copy_series::resolve(views.clone())?;
            let pg = d3d12_camera_copy_series::resolve(projections.clone())?;
            ensure!(
                vg.len() == pg.len(),
                "non-writing camera pair coverage differs"
            );
            for (v, p) in vg.iter().zip(&pg) {
                d3d12_camera_copy_series::validate_pair(v, p)?;
            }
        }
        for row in views.into_iter().chain(projections) {
            self.push_row(row, writer)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn push(&mut self, row: Observation) -> anyhow::Result<()> {
        self.push_row(row, true)
    }

    fn push_row(&mut self, row: Observation, writer: bool) -> anyhow::Result<()> {
        let present = row.sample.present;
        if let Some(previous) = self.present {
            ensure!(present >= previous, "archived camera Present regressed");
            if present != previous {
                self.flush()?;
            }
        }
        self.present = Some(present);
        if writer {
            match row.sample.source.role {
                depth_lab::MatrixRole::View => self.writer_views.push(row.clone()),
                depth_lab::MatrixRole::Projection => self.writer_projections.push(row.clone()),
            }
        }
        let rows = match row.sample.source.role {
            depth_lab::MatrixRole::View => &mut self.frame_views,
            depth_lab::MatrixRole::Projection => &mut self.frame_projections,
        };
        ensure!(
            rows.len() < MAX_BATCH_OBSERVATIONS,
            "archived camera Present exceeds sample bound"
        );
        rows.push(row);
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        let present = self.present.context("missing archived camera Present")?;
        let views = std::mem::take(&mut self.frame_views);
        let projections = std::mem::take(&mut self.frame_projections);
        for rows in [&views, &projections] {
            d3d12_camera_copy_series::validate_physical_observations(rows)?;
            ensure!(
                rows.iter()
                    .all(|row| row.sample.matrix == rows[0].sample.matrix),
                "archived camera matrices disagree across selected draws"
            );
            for row in rows {
                let receipt = row
                    .sample
                    .receipt
                    .as_ref()
                    .context("missing camera receipt")?;
                if let Some(previous) = self.receipts.get(&receipt.copy_submission) {
                    ensure!(
                        previous.same_physical_copy(receipt),
                        "archived camera copy submission changed across Presents"
                    );
                } else {
                    self.receipts
                        .insert(receipt.copy_submission, receipt.clone());
                }
            }
        }
        let view = views[0].sample.matrix;
        let projection = projections[0].sample.matrix;
        // These are counts of distinct retained physical observations, as in SourceFamily.
        let count = |rows: &[Observation]| {
            rows.iter()
                .map(|r| (&r.sample.source, &r.draw))
                .collect::<BTreeSet<_>>()
                .len()
        };
        let view_count = count(&views);
        let projection_count = count(&projections);
        let views = std::mem::take(&mut self.writer_views);
        let projections = std::mem::take(&mut self.writer_projections);
        ensure!(
            !views.is_empty() && !projections.is_empty(),
            "archived camera has no proven depth-writing draw at the exact Present"
        );
        if let Some(camera) = &mut self.admitted {
            camera.observe_present(views, projections)?;
        } else {
            ensure!(
                self.warmup_views.len() + views.len() <= MAX_BATCH_OBSERVATIONS
                    && self.warmup_projections.len() + projections.len() <= MAX_BATCH_OBSERVATIONS,
                "archived camera warmup exceeds sample bound"
            );
            self.required.insert(present);
            self.warmup_views.extend(views);
            self.warmup_projections.extend(projections);
            if self.required.len() == MIN_PRESENTS {
                let camera = admit(
                    std::mem::take(&mut self.warmup_views),
                    std::mem::take(&mut self.warmup_projections),
                    &self.required,
                )?;
                self.result.view_sources = camera.summary().view_sources.clone();
                self.result.projection_sources = camera.summary().projection_sources.clone();
                self.admitted = Some(camera);
            }
        }
        self.result.views.insert(present, view);
        self.result.projections.insert(present, projection);
        self.result.retained_views += view_count;
        self.result.retained_projections += projection_count;
        Ok(())
    }

    pub(in crate::cp2077_raw_demo) fn finish(
        mut self,
        matched_presents: usize,
    ) -> anyhow::Result<ArchiveMatrices> {
        self.flush()?;
        ensure!(
            self.admitted.is_some(),
            "archived camera lacks 120 exact source Presents"
        );
        ensure!(
            self.result.views.len() >= MIN_RENDER_CAMERA_PRESENTS
                && self.result.views.len() as f64 / matched_presents as f64
                    >= MIN_RENDER_SOURCE_COVERAGE
                && self.result.views.keys().eq(self.result.projections.keys()),
            "archived camera source coverage is insufficient"
        );
        Ok(self.result)
    }
}

#[cfg(test)]
mod tests;
