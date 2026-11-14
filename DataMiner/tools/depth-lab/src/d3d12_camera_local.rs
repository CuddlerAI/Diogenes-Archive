//! Same-launch camera admission for explicitly non-trainable CP2077 diagnostics.
//! Cross-launch CameraBinding authoring is deliberately independent of this verdict.
use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use serde::Serialize;

use crate::d3d12_camera_copy_series::{self as series, Observation, SourceFamily};

pub const POLICY: &str = "cp2077-holstered-same-launch-camera/v2";
pub const MIN_PRESENTS: usize = 120;
const MAX_OBSERVATIONS: usize = 32_768;

#[derive(Debug, Serialize)]
pub struct AdmissionSummary {
    pub schema: &'static str,
    pub non_trainable: bool,
    pub production_camera_binding_emitted: bool,
    pub cross_launch_admission: bool,
    pub view_sources: Vec<crate::D3d12MatrixSourceBinding>,
    pub projection_sources: Vec<crate::D3d12MatrixSourceBinding>,
    pub exact_presents: usize,
    pub required_presents: usize,
    pub retained_view_observations: usize,
    pub retained_projection_observations: usize,
}

pub struct LocalCamera {
    pub view: SourceFamily,
    pub projection: SourceFamily,
    summary: AdmissionSummary,
    last_present: u64,
    fault: Option<String>,
}

impl LocalCamera {
    pub fn summary(&self) -> &AdmissionSummary {
        &self.summary
    }

    /// Every new Present is independently checked against frozen sources AND its own copies.
    /// Failures latch: a later good sample cannot silently restore a failed session.
    pub fn observe_present(
        &mut self,
        views: Vec<Observation>,
        projections: Vec<Observation>,
    ) -> anyhow::Result<u64> {
        ensure!(
            self.fault.is_none(),
            "local camera session already faulted: {:?}",
            self.fault
        );
        let result = self.validate_present(views, projections);
        match result {
            Ok(present) => {
                self.last_present = present;
                Ok(present)
            }
            Err(error) => {
                self.fault = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn validate_present(
        &self,
        views: Vec<Observation>,
        projections: Vec<Observation>,
    ) -> anyhow::Result<u64> {
        let first = views
            .first()
            .context("current Present has no physical view")?;
        let present = first.sample.present;
        ensure!(
            present > self.last_present,
            "local camera Present did not advance"
        );
        for (rows, admitted) in [(&views, &self.view), (&projections, &self.projection)] {
            ensure!(
                !rows.is_empty() && rows.len() <= MAX_OBSERVATIONS,
                "current camera sample count is invalid"
            );
            for row in rows {
                ensure!(
                    row.sample.present == present,
                    "current camera samples cross a Present boundary"
                );
                admitted.validate_continuation(row)?;
                series::same_frame(&rows[0], row)?;
            }
        }
        // Even when root aliases lack eight overlaps in this one-Present window, exact
        // view/projection pairing must hold separately for EVERY observed source/draw.
        let vg = series::resolve(views)?;
        let pg = series::resolve(projections)?;
        ensure!(
            vg.len() == pg.len(),
            "current camera source coverage differs"
        );
        for (v, p) in vg.iter().zip(&pg) {
            series::validate_pair(v, p)?;
        }
        Ok(present)
    }
}

/// Loss counters, the exact selected DSV/draw order and the holstered viewport condition
/// must be verified by the caller before supplying observations. V2 also requires proven
/// depth-writing draws; known non-writers remain matrix witnesses, not source-family members.
/// No numerical A/B filter; complete payload equality remains required for contributing draws.
pub fn admit(
    views: Vec<Observation>,
    projections: Vec<Observation>,
    required: &BTreeSet<u64>,
) -> anyhow::Result<LocalCamera> {
    ensure!(
        required.len() >= MIN_PRESENTS,
        "local camera needs at least 120 exact Presents"
    );
    let mut vg = series::resolve(views)?;
    let mut pg = series::resolve(projections)?;
    ensure!(
        vg.len() == 1 && pg.len() == 1,
        "local camera copy-series family is missing or ambiguous"
    );
    let view = vg.remove(0);
    let projection = pg.remove(0);
    series::validate_pair(&view, &projection)?;
    ensure!(
        view.samples.len() >= MIN_PRESENTS && view.samples.keys().all(|p| required.contains(p)),
        "local camera evidence lacks exact required Present identity"
    );
    // Preserve the existing 90% source coverage threshold using exact integer arithmetic.
    ensure!(
        view.samples.len() as u128 * 10 >= required.len() as u128 * 9,
        "local camera source coverage below 90 percent"
    );
    let last_present = *view
        .samples
        .last_key_value()
        .context("missing admitted Present")?
        .0;
    let summary = AdmissionSummary {
        schema: POLICY,
        non_trainable: true,
        production_camera_binding_emitted: false,
        cross_launch_admission: false,
        view_sources: view.sources.clone(),
        projection_sources: projection.sources.clone(),
        exact_presents: view.samples.len(),
        required_presents: required.len(),
        retained_view_observations: view.observations.len(),
        retained_projection_observations: projection.observations.len(),
    };
    Ok(LocalCamera {
        view,
        projection,
        summary,
        last_present,
        fault: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::d3d12_camera_alias::{CopyCommandIdentity, CopyReceipt, Sample};
    use crate::{
        D3d12DescriptorTableBinding, D3d12DrawRelation, D3d12MatrixSourceBinding, MatrixEncoding,
        MatrixLayout, MatrixRole,
    };

    pub(crate) fn rows(present: u64, role: MatrixRole) -> Vec<Observation> {
        [0, 18]
            .map(|root| Observation {
                draw: series::DrawIdentity {
                    present_qpc: present as i64 * 1000,
                    queue_tag: 1,
                    command_list_tag: 2,
                    command_list_generation: present,
                    selected_depth_resource_tag: 3,
                    selected_depth_resource_epoch: 1,
                    selected_depth_descriptor_generation: 1,
                    draw_ordinal: 1,
                },
                sample: Sample {
                    present,
                    source: D3d12MatrixSourceBinding {
                        descriptor_table: D3d12DescriptorTableBinding {
                            root_signature_sha256: format!("sha256:{}", "a".repeat(64)),
                            root_parameter_index: root,
                            descriptor_range_index: 0,
                            descriptor_offset_in_range: 1,
                            descriptor_count: 4,
                            base_shader_register: 0,
                            register_space: 0,
                        },
                        cbv_resource_offset: 0,
                        declared_cbv_bytes: 1024,
                        matrix_offset: if role == MatrixRole::View { 0 } else { 128 },
                        encoding: MatrixEncoding::F32LittleEndian,
                        layout: MatrixLayout::RowMajor,
                        transpose: true,
                        role,
                        draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
                    },
                    matrix: [[present as f64; 4]; 4],
                    receipt: Some(CopyReceipt {
                        schema: "dm-d3d12-executed-cbv-copy/v2".into(),
                        copy_submission: present,
                        queue_tag: 1,
                        destination_source_tag: 4,
                        destination_content_generation: 1,
                        destination_offset: 0,
                        source_source_tag: 5,
                        source_content_generation: 1,
                        source_offset: present * 1024,
                        descriptor_generation: 1 + root as u64,
                        copied_bytes: 1024,
                        bytes_sha256: format!("sha256:{present:064x}"),
                        copy_command: Some(CopyCommandIdentity {
                            command_list_tag: 2,
                            command_list_generation: present,
                            copy_ordinal: 1,
                        }),
                    }),
                },
            })
            .into()
    }

    fn fixture() -> (Vec<Observation>, Vec<Observation>, BTreeSet<u64>) {
        (
            (1..=120).flat_map(|p| rows(p, MatrixRole::View)).collect(),
            (1..=120)
                .flat_map(|p| rows(p, MatrixRole::Projection))
                .collect(),
            (1..=120).collect(),
        )
    }

    #[test]
    fn holstered_sources_admit_locally_and_every_later_present_is_rechecked() {
        let (v, p, required) = fixture();
        let mut camera = admit(v, p, &required).unwrap();
        assert_eq!(camera.summary().view_sources.len(), 2);
        assert!(!camera.summary().cross_launch_admission);
        assert!(!camera.summary().production_camera_binding_emitted);
        assert_eq!(
            camera
                .observe_present(
                    rows(121, MatrixRole::View),
                    rows(121, MatrixRole::Projection)
                )
                .unwrap(),
            121
        );
        let mut changed = rows(122, MatrixRole::Projection);
        changed[1].sample.matrix[2][2] += 1.0;
        assert!(
            camera
                .observe_present(rows(122, MatrixRole::View), changed)
                .is_err()
        );
        assert!(
            camera
                .observe_present(
                    rows(123, MatrixRole::View),
                    rows(123, MatrixRole::Projection)
                )
                .is_err()
        );
    }

    #[test]
    fn one_binding_may_be_absent_but_a_new_binding_may_not_replace_it() {
        let (v, p, r) = fixture();
        let mut camera = admit(v, p, &r).unwrap();
        let mut v = rows(121, MatrixRole::View);
        let mut p = rows(121, MatrixRole::Projection);
        v.pop();
        p.pop();
        camera.observe_present(v, p).unwrap();
        let mut v = rows(122, MatrixRole::View);
        v[0].sample.source.descriptor_table.root_parameter_index = 7;
        assert!(
            camera
                .observe_present(v, rows(122, MatrixRole::Projection))
                .is_err()
        );
    }

    #[test]
    fn source_family_equality_does_not_hide_weapon_payload_or_copy_conflicts() {
        for mutation in 0..4 {
            let (mut v, mut p, r) = fixture();
            match mutation {
                0 => p[101].sample.matrix[2][2] -= 0.000004,
                1 => {
                    v[101].sample.receipt.as_mut().unwrap().bytes_sha256 =
                        format!("sha256:{}", "f".repeat(64))
                }
                2 => {
                    p.pop();
                }
                _ => v[101].sample.receipt = None,
            }
            assert!(admit(v, p, &r).is_err(), "mutation {mutation}");
        }
    }

    #[test]
    fn minimum_and_coverage_are_not_lowered() {
        let (v, p, mut r) = fixture();
        r.extend(121..=134);
        assert!(admit(v, p, &r).is_err());
        let (mut v, mut p, mut r) = fixture();
        v.truncate(238);
        p.truncate(238);
        r.remove(&120);
        assert!(admit(v, p, &r).is_err());
    }

    #[test]
    fn later_generation_change_and_cross_present_input_fail() {
        for mutate in 0..3 {
            let (v, p, r) = fixture();
            let mut camera = admit(v, p, &r).unwrap();
            let mut v = rows(121, MatrixRole::View);
            let p = rows(121, MatrixRole::Projection);
            match mutate {
                0 => {
                    v[0].sample
                        .receipt
                        .as_mut()
                        .unwrap()
                        .destination_content_generation += 1
                }
                1 => v[0].sample.present = 122,
                _ => v[0].draw.selected_depth_resource_epoch += 1,
            }
            assert!(camera.observe_present(v, p).is_err());
        }
    }
}
