//! Authored holstered camera sources used by the normal StartRun path.
//! Stable sources are supplied by a binding, never elected from runtime candidates.
use std::collections::BTreeSet;

use anyhow::{Context, ensure};

use crate::camera_binding::{D3d12CameraBinding, D3d12CameraBindingError};
use crate::d3d12_camera_copy_series::{self as series, Observation};
use crate::d3d12_camera_local::{LocalCamera, MIN_PRESENTS, admit};

pub const POLICY: &str = "cp2077-bound-holstered-camera/v1";
/// The declared view slot stores row-vector VP, not a directly readable V.
/// Recover V = VP * inverse(P) using only its paired, same-CBV projection.
pub const ROW_VIEW_PROJECTION_POLICY: &str = "cp2077-bound-holstered-row-vp/v2";
// Two matrices per alias fit the existing 32-matrix selected-draw budget.
pub const MAX_SOURCE_PAIRS: usize = 16;
const MAX_WARMUP_OBSERVATIONS: usize = 32_768;

/// Check every draw and its exact view/projection pairing before retaining one
/// witness for repeated uses of the SAME copy by the SAME semantic source.
/// Different copies, aliases and descriptor generations remain distinct. This
/// only bounds warmup storage; it must never skip conflicting draw evidence.
fn compact_warmup_present(
    views: Vec<Observation>,
    projections: Vec<Observation>,
) -> anyhow::Result<(Vec<Observation>, Vec<Observation>)> {
    let view = series::resolve(views)?;
    let projection = series::resolve(projections)?;
    ensure!(
        view.len() == projection.len(),
        "bound camera warmup pair coverage differs"
    );
    for (v, p) in view.iter().zip(&projection) {
        series::validate_pair(v, p)?;
    }
    fn compact(families: Vec<series::SourceFamily>) -> Vec<Observation> {
        let mut seen = BTreeSet::new();
        let mut retained = Vec::new();
        for family in families {
            for ((_, source, _), row) in family.observations {
                let copy = row.sample.receipt.as_ref().expect("validated copy receipt");
                // resolve checked all receipt fields and matrices. In v2 the
                // physical-copy comparison excludes descriptor generation, so
                // keep that generation in the key as well as the stable source.
                if seen.insert((source, copy.copy_submission, copy.descriptor_generation)) {
                    retained.push(row);
                }
            }
        }
        retained
    }
    Ok((compact(view), compact(projection)))
}

impl D3d12CameraBinding {
    pub fn matrix_sources(&self) -> impl Iterator<Item = &crate::D3d12MatrixSourceBinding> {
        self.view_sources
            .iter()
            .map(|s| &s.source)
            .chain(self.projection_sources.iter())
    }
    pub(crate) fn validate_holstered_sources(&self) -> Result<(), D3d12CameraBindingError> {
        let invalid = |reason| D3d12CameraBindingError::InvalidMatrixSource {
            kind: "view",
            reason,
        };
        let policy = self
            .holstered_camera
            .as_ref()
            .ok_or_else(|| invalid("v3 requires a camera basis and source policy"))?;
        if !matches!(policy.schema.as_str(), POLICY | ROW_VIEW_PROJECTION_POLICY)
            || self.executable_sha256 != crate::CP2077_BUILD_20383525_EXE_SHA256
            || !matches!(
                self.profile_version.as_str(),
                crate::DepthProfile::D3D12_SCHEMA_VERSION
                    | crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION
            )
            || (self.render_width, self.render_height) != (1920, 1080)
        {
            return Err(invalid(
                "holstered binding policy, build or geometry is unsupported",
            ));
        }
        policy.camera_axis_mapping.validate()?;
        let count = self.view_sources.len();
        if count == 0 || count > MAX_SOURCE_PAIRS || count != self.projection_sources.len() {
            return Err(invalid("v3 requires bounded paired source aliases"));
        }
        let mut distinct = BTreeSet::new();
        for (view, projection) in self.view_sources.iter().zip(&self.projection_sources) {
            view.source.validate_for_role(crate::MatrixRole::View)?;
            projection.validate_for_role(crate::MatrixRole::Projection)?;
            let v = &view.source;
            if !distinct.insert(v.clone())
                || view.translation != crate::camera::TranslationSlot::Row3
                || v.descriptor_table != projection.descriptor_table
                || v.cbv_resource_offset != projection.cbv_resource_offset
                || v.declared_cbv_bytes != projection.declared_cbv_bytes
                || v.matrix_offset == projection.matrix_offset
                || v.matrix_offset.checked_add(64).is_none_or(|n| n > 256)
                || projection
                    .matrix_offset
                    .checked_add(64)
                    .is_none_or(|n| n > 256)
            {
                return Err(invalid(
                    "v3 source aliases must contain complete bounded native view/projection pairs",
                ));
            }
        }
        Ok(())
    }
}

/// Per-run, bounded source warmup. The physical resource/descriptor generations are learned
/// anew, while every stable source must exactly match the externally supplied binding.
/// No failed instance can be re-armed. Callers must first validate all draw/loss/order evidence.
#[derive(Default)]
pub struct BoundCamera {
    views: Vec<Observation>,
    projections: Vec<Observation>,
    required: BTreeSet<u64>,
    admitted: Option<LocalCamera>,
    fault: Option<String>,
}

impl BoundCamera {
    pub fn started(&self) -> bool {
        self.admitted.is_some() || !self.required.is_empty() || self.fault.is_some()
    }
    pub fn observe(
        &mut self,
        binding: &D3d12CameraBinding,
        views: Vec<Observation>,
        projections: Vec<Observation>,
    ) -> anyhow::Result<Option<(crate::camera::Mat4, crate::camera::Mat4, i64)>> {
        ensure!(
            self.fault.is_none(),
            "bound camera already faulted: {:?}",
            self.fault
        );
        let result = self.observe_inner(binding, views, projections);
        if let Err(error) = &result {
            self.fault = Some(error.to_string());
        }
        result
    }

    fn observe_inner(
        &mut self,
        binding: &D3d12CameraBinding,
        views: Vec<Observation>,
        projections: Vec<Observation>,
    ) -> anyhow::Result<Option<(crate::camera::Mat4, crate::camera::Mat4, i64)>> {
        ensure!(
            binding.holstered_camera.is_some(),
            "bound holstered policy is absent"
        );
        binding
            .validate_for_executable(&binding.executable_sha256)
            .map_err(anyhow::Error::msg)?;
        let first = views.first().context("bound camera has no view")?;
        let projection = projections
            .first()
            .context("bound camera has no projection")?;
        let present = first.sample.present;
        let result = (
            first.sample.matrix.map(|r| r.map(|v| v as f32)),
            projection.sample.matrix.map(|r| r.map(|v| v as f32)),
            first.draw.present_qpc,
        );
        for (rows, sources) in [
            (
                &views,
                binding
                    .view_sources
                    .iter()
                    .map(|s| &s.source)
                    .collect::<Vec<_>>(),
            ),
            (
                &projections,
                binding.projection_sources.iter().collect::<Vec<_>>(),
            ),
        ] {
            ensure!(
                rows.len() <= MAX_WARMUP_OBSERVATIONS,
                "bound camera sample budget exceeded"
            );
            ensure!(
                rows.iter()
                    .all(|r| r.sample.present == present && sources.contains(&&r.sample.source)),
                "camera source or Present differs from authored binding"
            );
        }
        if let Some(camera) = &mut self.admitted {
            camera.observe_present(views, projections)?;
            return Ok(Some(result));
        }
        ensure!(
            self.required.last().is_none_or(|&p| present > p),
            "bound camera Present did not advance"
        );
        let incoming_draws = (views.len(), projections.len());
        let (views, projections) = compact_warmup_present(views, projections)?;
        ensure!(
            self.views.len() + views.len() <= MAX_WARMUP_OBSERVATIONS
                && self.projections.len() + projections.len() <= MAX_WARMUP_OBSERVATIONS,
            "bound camera warmup budget exceeded: presents={}, retained_views={}, \
             retained_projections={}, incoming_views={}, incoming_projections={}, \
             incoming_draw_views={}, incoming_draw_projections={}, limit={}",
            self.required.len(),
            self.views.len(),
            self.projections.len(),
            views.len(),
            projections.len(),
            incoming_draws.0,
            incoming_draws.1,
            MAX_WARMUP_OBSERVATIONS
        );
        self.required.insert(present);
        self.views.extend(views);
        self.projections.extend(projections);
        if self.required.len() < MIN_PRESENTS {
            return Ok(None);
        }
        let camera = admit(
            std::mem::take(&mut self.views),
            std::mem::take(&mut self.projections),
            &self.required,
        )?;
        let expected_view: BTreeSet<_> = binding.view_sources.iter().map(|s| &s.source).collect();
        let expected_projection: BTreeSet<_> = binding.projection_sources.iter().collect();
        ensure!(
            camera.view.sources.iter().collect::<BTreeSet<_>>() == expected_view
                && camera.projection.sources.iter().collect::<BTreeSet<_>>() == expected_projection,
            "not all authored source aliases were independently proven during warmup"
        );
        self.admitted = Some(camera);
        self.required.clear();
        Ok(Some(result))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::d3d12_camera_local::tests::rows;
    use crate::{Cp2077HolsteredCamera, D3d12SignedAxis, D3d12ViewSourceBinding, MatrixRole};

    #[test]
    fn actual_cp2077_catalog_pair_requires_the_stable_profile_selector() {
        let profile: crate::DepthProfile = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/depth-profile.json"
        ))
        .unwrap();
        let binding: crate::CameraBindingDocument = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();
        binding
            .validate_for_profile(&profile, &profile.executable_sha256)
            .unwrap();
        assert_eq!(
            profile.schema_version,
            crate::DepthProfile::D3D12_STABLE_SCHEMA_VERSION
        );
        assert_eq!(binding.schema(), crate::CAMERA_BINDING_SCHEMA_V3);
        let mut missing_selector = profile.clone();
        missing_selector.d3d12_selection = None;
        assert!(
            binding
                .validate_for_profile(&missing_selector, &profile.executable_sha256)
                .is_err()
        );
        let mut bad_binding = match binding {
            crate::CameraBindingDocument::V2(b) => b,
            _ => panic!("expected D3D12 binding"),
        };
        bad_binding.profile_version = "dm-depth-profile-4".into();
        assert!(
            bad_binding
                .validate_for_executable(&profile.executable_sha256)
                .is_err()
        );
    }

    pub(crate) fn binding() -> D3d12CameraBinding {
        let mut b: D3d12CameraBinding =
            serde_json::from_str(include_str!("../tests/fixtures/camera-binding-v2.json")).unwrap();
        b.schema = crate::CAMERA_BINDING_SCHEMA_V3.into();
        b.executable_sha256 = crate::CP2077_BUILD_20383525_EXE_SHA256.into();
        let mut camera_axes = b.axis_mapping;
        camera_axes.delivery_y_from = D3d12SignedAxis::NegativeY;
        b.holstered_camera = Some(Cp2077HolsteredCamera {
            schema: POLICY.into(),
            camera_axis_mapping: camera_axes,
        });
        b.view_sources = rows(1, MatrixRole::View)
            .into_iter()
            .map(|r| D3d12ViewSourceBinding {
                source: r.sample.source,
                translation: crate::camera::TranslationSlot::Row3,
            })
            .collect();
        b.projection_sources = rows(1, MatrixRole::Projection)
            .into_iter()
            .map(|r| r.sample.source)
            .collect();
        b
    }

    #[test]
    fn v3_requires_explicit_paired_sources_and_preserves_v2() {
        let b = binding();
        b.validate_for_executable(&b.executable_sha256).unwrap();
        let doc: crate::CameraBindingDocument =
            serde_json::from_value(serde_json::to_value(&b).unwrap()).unwrap();
        assert_eq!(doc.schema(), crate::CAMERA_BINDING_SCHEMA_V3);
        for mutate in [
            |b: &mut D3d12CameraBinding| b.holstered_camera = None,
            |b: &mut D3d12CameraBinding| b.schema = crate::CAMERA_BINDING_SCHEMA_V2.into(),
            |b: &mut D3d12CameraBinding| b.projection_sources.clear(),
            |b: &mut D3d12CameraBinding| b.view_sources[1] = b.view_sources[0].clone(),
            |b: &mut D3d12CameraBinding| b.projection_sources[1].matrix_offset = 256,
            |b: &mut D3d12CameraBinding| b.executable_sha256 = format!("sha256:{}", "a".repeat(64)),
        ] {
            let mut bad = b.clone();
            mutate(&mut bad);
            assert!(bad.validate_for_executable(&bad.executable_sha256).is_err());
        }
    }

    #[test]
    fn authored_aliases_need_warmup_and_physical_changes_latch() {
        let b = binding();
        let mut camera = BoundCamera::default();
        for present in 1..=120 {
            let pose = camera
                .observe(
                    &b,
                    rows(present, MatrixRole::View),
                    rows(present, MatrixRole::Projection),
                )
                .unwrap();
            assert_eq!(pose.is_some(), present == 120);
        }
        let mut v = rows(121, MatrixRole::View);
        v[0].sample
            .receipt
            .as_mut()
            .unwrap()
            .destination_content_generation += 1;
        assert!(
            camera
                .observe(&b, v, rows(121, MatrixRole::Projection))
                .is_err()
        );
        assert!(
            camera
                .observe(
                    &b,
                    rows(122, MatrixRole::View),
                    rows(122, MatrixRole::Projection)
                )
                .is_err()
        );
    }

    #[test]
    fn a_declared_alias_cannot_be_silently_missing() {
        let b = binding();
        let mut camera = BoundCamera::default();
        for present in 1..=120 {
            let mut v = rows(present, MatrixRole::View);
            v.pop();
            let mut p = rows(present, MatrixRole::Projection);
            p.pop();
            let result = camera.observe(&b, v, p);
            assert_eq!(result.is_err(), present == 120);
        }
    }

    fn repeated_draws(present: u64, role: MatrixRole, count: u32) -> Vec<Observation> {
        (1..=count)
            .flat_map(|draw| {
                let mut observations = rows(present, role);
                for row in &mut observations {
                    row.draw.draw_ordinal = draw;
                }
                observations
            })
            .collect()
    }

    #[test]
    fn many_draws_reusing_exact_copies_complete_the_full_warmup() {
        let b = binding();
        let mut camera = BoundCamera::default();
        for present in 1..=121 {
            let result = camera
                .observe(
                    &b,
                    repeated_draws(present, MatrixRole::View, 1500),
                    repeated_draws(present, MatrixRole::Projection, 1500),
                )
                .unwrap_or_else(|e| panic!("Present {present}: {e}"));
            assert_eq!(result.is_some(), present >= MIN_PRESENTS as u64);
        }
        let admitted = camera.admitted.as_ref().unwrap();
        assert_eq!(admitted.summary().exact_presents, MIN_PRESENTS);
        assert_eq!(admitted.summary().retained_view_observations, 240);
        assert_eq!(admitted.summary().retained_projection_observations, 240);
    }

    #[test]
    fn compaction_checks_the_last_draw_and_latches_conflicts() {
        let b = binding();
        for fault in ["matrix", "payload", "missing_pair", "copy_command"] {
            let mut camera = BoundCamera::default();
            let mut views = repeated_draws(1, MatrixRole::View, 1500);
            let mut projections = repeated_draws(1, MatrixRole::Projection, 1500);
            let last = views.last_mut().unwrap();
            match fault {
                "matrix" => last.sample.matrix[0][0] += 1.0,
                "payload" => {
                    last.sample.receipt.as_mut().unwrap().bytes_sha256 =
                        format!("sha256:{}", "b".repeat(64))
                }
                "missing_pair" => {
                    projections.pop();
                }
                "copy_command" => {
                    last.sample
                        .receipt
                        .as_mut()
                        .unwrap()
                        .copy_command
                        .as_mut()
                        .unwrap()
                        .command_list_generation += 1
                }
                _ => unreachable!(),
            }
            assert!(camera.observe(&b, views, projections).is_err(), "{fault}");
            assert!(
                camera
                    .observe(
                        &b,
                        rows(2, MatrixRole::View),
                        rows(2, MatrixRole::Projection)
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn distinct_copies_and_descriptor_generations_remain_distinct() {
        let b = binding();
        let mut camera = BoundCamera::default();
        let mut views = repeated_draws(1, MatrixRole::View, 3);
        let mut projections = repeated_draws(1, MatrixRole::Projection, 3);
        for row in views.iter_mut().chain(&mut projections) {
            let copy = row.sample.receipt.as_mut().unwrap();
            if row.draw.draw_ordinal == 2 {
                copy.copy_submission = 2;
                copy.copy_command.as_mut().unwrap().copy_ordinal = 2;
            } else if row.draw.draw_ordinal == 3 {
                copy.descriptor_generation += 100;
            }
        }
        assert!(camera.observe(&b, views, projections).unwrap().is_none());
        assert_eq!(camera.views.len(), 6);
        assert_eq!(camera.projections.len(), 6);
    }

    #[test]
    fn conflicting_copy_reuse_across_presents_is_not_hidden() {
        let b = binding();
        let mut camera = BoundCamera::default();
        for present in 1..=120 {
            let mut views = repeated_draws(present, MatrixRole::View, 3);
            let mut projections = repeated_draws(present, MatrixRole::Projection, 3);
            if present == 120 {
                for row in views.iter_mut().chain(&mut projections) {
                    // The draw's other fields are internally valid; the old
                    // submission identity must still conflict with Present 1.
                    row.sample.receipt.as_mut().unwrap().copy_submission = 1;
                }
            }
            let result = camera.observe(&b, views, projections);
            if present == 120 {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("contradictory receipts")
                );
            } else {
                assert!(result.unwrap().is_none());
            }
        }
    }
}
