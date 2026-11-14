//! Local timed-walk discovery only. Production and cross-launch authoring retain exact bindings.
use super::*;
use depth_lab::d3d12_camera_alias::{CopyReceipt, Sample};
use depth_lab::d3d12_camera_copy_series::{self, DrawIdentity, Observation, SourceFamily};

mod archive;
pub(super) use archive::ArchiveCamera;

pub(super) fn sample(
    matrix: &Value,
    draw: &Value,
    role: depth_lab::MatrixRole,
) -> anyhow::Result<Observation> {
    let present = draw["present_seq"]
        .as_u64()
        .context("missing camera Present")?;
    let identity: DrawIdentity = serde_json::from_value(draw.clone())?;
    let source = serde_json::from_str(&render_matrix_source_key(matrix, role)?)?;
    let receipt: Option<CopyReceipt> =
        serde_json::from_value(matrix.get("copy_receipt").cloned().unwrap_or(Value::Null))?;
    if let Some(receipt) = &receipt {
        ensure!(
            matrix["physical_source"] == "copy_buffer_region_root_signature_declared_cbv"
                && matrix["source_tag"].as_u64() == Some(receipt.destination_source_tag)
                && identity.queue_tag == receipt.queue_tag,
            "camera copy receipt contradicts draw/source identity"
        );
    }
    Ok(Observation {
        sample: Sample {
            present,
            source,
            receipt,
            matrix: serde_json::from_value(matrix["matrix"].clone())?,
        },
        draw: identity,
    })
}

pub(super) fn unique(samples: Vec<Observation>, presents: usize) -> anyhow::Result<SourceFamily> {
    let mut admitted = d3d12_camera_copy_series::resolve(samples)?
        .into_iter()
        .filter(|group| group.samples.len() >= MIN_RENDER_CAMERA_PRESENTS)
        .collect::<Vec<_>>();
    ensure!(
        admitted.len() == 1,
        "local camera copy-series family is missing or ambiguous"
    );
    let group = admitted.remove(0);
    ensure!(
        group.samples.len() as f64 / presents as f64 >= MIN_RENDER_SOURCE_COVERAGE,
        "local camera copy-series family has insufficient exact Present coverage"
    );
    Ok(group)
}

pub(super) fn validate_local_observation(
    observation: &Value,
    verified_archive: bool,
) -> anyhow::Result<()> {
    // Recompute local physical admission; never reinterpret a producer's production/authoring
    // flag. A fully restored, hash-checked archive needs fresh local physical reduction;
    // the producer's bounded tail cannot supply whole-recording source counts.
    for name in [
        "matrix_candidate_overflow",
        "pending_draw_overflow",
        "draw_scan_budget_exhausted",
        "present_observation_overflow",
        "tracker_contention_drops",
        "hot_state_drops",
        "upload_unmap_snapshot_budget_exhausted",
        "exact_cbv_copy_span_budget_exhausted",
        "exact_cbv_copy_request_budget_exhausted",
        "exact_cbv_copy_shadow_budget_exhausted",
    ] {
        ensure!(
            observation[name].as_u64() == Some(0),
            "local camera observed missing/nonzero {name}"
        );
    }
    let blockers = observation["authoring_blockers"]
        .as_array()
        .context("missing authoring blockers")?;
    for blocker in blockers {
        let text = blocker.as_str().context("invalid camera blocker")?;
        let archived_tail_only = verified_archive
            && (text == "local archived camera evidence does not author a cross-launch binding"
                || ["view", "projection"].iter().any(|role| {
                    text == format!(
                        "D3D12 render-camera authoring found 0 stable {role} sources; expected one"
                    )
                }));
        ensure!(archived_tail_only || ["view", "projection"].iter().any(|role| {
            (2..=64).any(|count| text == format!("D3D12 render-camera authoring found {count} stable {role} sources; expected one"))
        }), "unresolved camera authoring blocker: {text}");
    }
    Ok(())
}

pub(super) fn matrices(group: &SourceFamily) -> RenderMatrixSamples {
    group
        .samples
        .iter()
        .map(|(present, sample)| (*present, sample.matrix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation() -> Value {
        json!({"matrix_candidate_overflow":0,"pending_draw_overflow":0,
            "draw_scan_budget_exhausted":0,"present_observation_overflow":0,
            "tracker_contention_drops":0,"hot_state_drops":0,
            "upload_unmap_snapshot_budget_exhausted":0,"exact_cbv_copy_span_budget_exhausted":0,
            "exact_cbv_copy_request_budget_exhausted":0,"exact_cbv_copy_shadow_budget_exhausted":0,
            "authoring_blockers":["D3D12 render-camera authoring found 2 stable view sources; expected one",
                "D3D12 render-camera authoring found 2 stable projection sources; expected one"]})
    }

    #[test]
    fn restored_archive_does_not_use_tail_only_source_counts() {
        let mut report = observation();
        report["authoring_blockers"] = json!([
            "D3D12 render-camera authoring found 0 stable view sources; expected one",
            "D3D12 render-camera authoring found 0 stable projection sources; expected one",
            "local archived camera evidence does not author a cross-launch binding"
        ]);
        assert!(validate_local_observation(&report, false).is_err());
        validate_local_observation(&report, true).unwrap();
        report["hot_state_drops"] = json!(1);
        assert!(validate_local_observation(&report, true).is_err());
    }

    #[test]
    fn only_source_counts_can_be_reconsidered_for_local_physical_reduction() {
        validate_local_observation(&observation(), false).unwrap();
        let mut report = observation();
        report["authoring_blockers"]
            .as_array_mut()
            .unwrap()
            .push(json!("unknown physical error"));
        assert!(validate_local_observation(&report, false).is_err());
        assert!(validate_local_observation(&report, true).is_err());
    }

    #[test]
    fn missing_or_lossy_counters_cannot_hide_behind_alias_counts() {
        let mut report = observation();
        report["tracker_contention_drops"] = json!(1);
        assert!(validate_local_observation(&report, false).is_err());
        let mut report = observation();
        report.as_object_mut().unwrap().remove("hot_state_drops");
        assert!(validate_local_observation(&report, true).is_err());
    }
}
