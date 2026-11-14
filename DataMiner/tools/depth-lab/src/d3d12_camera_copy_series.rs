//! Pure same-launch selected-CBV copy-series validation for local fixed-view diagnostics.
//! Distinct draws remain distinct physical copies. This does not relax the exact-copy alias
//! reducer or author a cross-launch/production camera binding.
use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::D3d12MatrixSourceBinding;
use crate::d3d12_camera_alias::{CopyReceipt, Sample};

pub const POLICY: &str = "same_launch_selected_cbv_copy_series/v1";
const MAX_OBSERVATIONS: usize = 32_768;
const MAX_SOURCES: usize = 64;
const MIN_OVERLAPPING_PRESENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DrawIdentity {
    pub present_qpc: i64,
    pub queue_tag: u64,
    pub command_list_tag: u64,
    pub command_list_generation: u64,
    pub selected_depth_resource_tag: u64,
    pub selected_depth_resource_epoch: u64,
    pub selected_depth_descriptor_generation: u64,
    pub draw_ordinal: u32,
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub sample: Sample,
    pub draw: DrawIdentity,
}

type ObservationKey = (u64, D3d12MatrixSourceBinding, DrawIdentity);

#[derive(Debug)]
pub struct SourceFamily {
    pub sources: Vec<D3d12MatrixSourceBinding>,
    /// One numerical matrix per Present, only after every physical observation agrees.
    pub samples: BTreeMap<u64, Sample>,
    /// All distinct source/draw/copy evidence, including repeated writes of identical bytes.
    pub observations: BTreeMap<ObservationKey, Observation>,
    first_draws: BTreeMap<u64, DrawIdentity>,
}

impl SourceFamily {
    /// Validate a fresh physical observation against this already admitted same-launch family.
    /// This never treats a distinct CopyBufferRegion as an alias of an earlier copy.
    pub(crate) fn validate_continuation(&self, next: &Observation) -> anyhow::Result<()> {
        ensure!(
            self.sources.contains(&next.sample.source),
            "camera source changed after local admission"
        );
        let first = self
            .observations
            .values()
            .next()
            .context("empty admitted camera family")?;
        ensure!(
            physical_range(first)? == physical_range(next)?,
            "camera physical range changed after local admission"
        );
        ensure!(
            first.draw.selected_depth_descriptor_generation
                == next.draw.selected_depth_descriptor_generation,
            "selected depth descriptor generation changed after local admission"
        );
        ensure!(
            receipt(first)?.schema == receipt(next)?.schema,
            "camera copy receipt version changed"
        );
        Ok(())
    }

    fn first_at(&self, present: u64) -> anyhow::Result<&Observation> {
        let sample = self
            .samples
            .get(&present)
            .context("missing Present sample")?;
        let draw = self
            .first_draws
            .get(&present)
            .context("missing Present draw")?;
        self.observations
            .get(&(present, sample.source.clone(), draw.clone()))
            .context("missing physical observation")
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PhysicalRange {
    queue: u64,
    destination: u64,
    destination_generation: u64,
    offset: u64,
    bytes: u64,
    upload_source: u64,
    upload_generation: u64,
    depth_resource: u64,
    depth_epoch: u64,
}

fn receipt(observation: &Observation) -> anyhow::Result<&CopyReceipt> {
    let sample = &observation.sample;
    let draw = &observation.draw;
    let r = sample
        .receipt
        .as_ref()
        .context("copy series lacks a physical receipt")?;
    sample
        .source
        .validate_for_role(sample.source.role)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        sample.present > 0
            && draw.present_qpc > 0
            && draw.queue_tag > 0
            && draw.command_list_tag > 0
            && draw.command_list_generation > 0
            && draw.selected_depth_resource_tag > 0
            && draw.selected_depth_resource_epoch > 0
            && draw.selected_depth_descriptor_generation > 0
            && draw.draw_ordinal > 0,
        "copy series lacks exact selected-draw identity"
    );
    ensure!(
        r.valid_versioned_identity()
            && r.copy_command.is_none_or(|command| {
                command.command_list_tag == draw.command_list_tag
                    && command.command_list_generation == draw.command_list_generation
            })
            && r.copy_submission > 0
            && r.queue_tag == draw.queue_tag
            && r.destination_source_tag > 0
            && r.source_source_tag > 0
            && r.descriptor_generation > 0
            && r.destination_offset == sample.source.cbv_resource_offset
            && r.copied_bytes <= u64::from(sample.source.declared_cbv_bytes)
            && u64::from(sample.source.matrix_offset) + 64 <= r.copied_bytes
            && r.source_offset.checked_add(r.copied_bytes).is_some()
            && r.destination_offset.checked_add(r.copied_bytes).is_some()
            && r.bytes_sha256.len() == 71
            && r.bytes_sha256.starts_with("sha256:")
            && r.bytes_sha256[7..].bytes().all(|b| b.is_ascii_hexdigit()),
        "copy series receipt is incomplete or outside its declared range"
    );
    ensure!(
        sample.matrix.iter().flatten().all(|v| v.is_finite()),
        "non-finite copy-series matrix"
    );
    Ok(r)
}

fn physical_range(observation: &Observation) -> anyhow::Result<PhysicalRange> {
    let r = receipt(observation)?;
    Ok(PhysicalRange {
        queue: r.queue_tag,
        destination: r.destination_source_tag,
        destination_generation: r.destination_content_generation,
        offset: r.destination_offset,
        bytes: r.copied_bytes,
        upload_source: r.source_source_tag,
        upload_generation: r.source_content_generation,
        depth_resource: observation.draw.selected_depth_resource_tag,
        depth_epoch: observation.draw.selected_depth_resource_epoch,
    })
}

pub(crate) fn same_frame(a: &Observation, b: &Observation) -> anyhow::Result<()> {
    ensure!(
        a.sample.matrix == b.sample.matrix,
        "copy-series matrices disagree at one Present"
    );
    ensure!(
        receipt(a)?.bytes_sha256 == receipt(b)?.bytes_sha256,
        "copy-series full CBV payloads disagree at one Present"
    );
    ensure!(
        a.draw.present_qpc == b.draw.present_qpc
            && a.draw.selected_depth_descriptor_generation
                == b.draw.selected_depth_descriptor_generation,
        "copy-series selected Present identity disagrees"
    );
    Ok(())
}

fn compatible(a: &D3d12MatrixSourceBinding, b: &D3d12MatrixSourceBinding) -> bool {
    let mut normalized = b.clone();
    normalized.descriptor_table.root_parameter_index = a.descriptor_table.root_parameter_index;
    *a == normalized
}

/// Verify receipt versions, bounds and identity across all draws, including non-writing witnesses.
/// Distinct physical copies may carry distinct payloads; the very same copy may never do so.
pub fn validate_physical_observations(observations: &[Observation]) -> anyhow::Result<()> {
    ensure!(
        !observations.is_empty() && observations.len() <= MAX_OBSERVATIONS,
        "copy-series observation count is empty or exceeds its bound"
    );
    let receipt_schema = receipt(&observations[0])?.schema.clone();
    let mut submissions = BTreeMap::new();
    let mut draws = BTreeMap::new();
    for observation in observations {
        let r = receipt(observation)?;
        ensure!(
            r.schema == receipt_schema,
            "copy series mixes receipt versions"
        );
        if let Some(previous) = submissions.insert(r.copy_submission, r.clone()) {
            ensure!(
                previous.same_physical_copy(r),
                "one copy submission has contradictory receipts"
            );
        }
        let draw_key = (
            observation.sample.present,
            observation.draw.clone(),
            physical_range(observation)?,
        );
        if let Some(previous) = draws.insert(draw_key, r.clone()) {
            ensure!(
                previous.same_physical_copy(r),
                "one selected draw has contradictory copy receipts across bindings"
            );
        }
    }
    Ok(())
}

/// Group one unchanged destination/upload range. Distinct root parameters need eight common
/// Presents with identical complete payloads and matrices. Physical copies remain distinct.
/// Call validate_physical_observations before excluding proven non-writing witnesses.
pub fn resolve(observations: Vec<Observation>) -> anyhow::Result<Vec<SourceFamily>> {
    validate_physical_observations(&observations)?;
    let mut by_source = BTreeMap::<D3d12MatrixSourceBinding, SourceFamily>::new();
    for observation in observations {
        let source = observation.sample.source.clone();
        let present = observation.sample.present;
        let family = by_source
            .entry(source.clone())
            .or_insert_with(|| SourceFamily {
                sources: vec![source.clone()],
                samples: BTreeMap::new(),
                observations: BTreeMap::new(),
                first_draws: BTreeMap::new(),
            });
        if let Some(first) = family.observations.values().next() {
            ensure!(
                physical_range(first)? == physical_range(&observation)?,
                "one semantic source changed physical copy-series range"
            );
        }
        if family.samples.contains_key(&present) {
            same_frame(family.first_at(present)?, &observation)?;
        }
        let key = (present, source, observation.draw.clone());
        if let Some(previous) = family.observations.get(&key) {
            ensure!(
                previous.sample.receipt == observation.sample.receipt,
                "one selected draw has contradictory copy receipts"
            );
        } else {
            family
                .samples
                .entry(present)
                .or_insert_with(|| observation.sample.clone());
            family
                .first_draws
                .entry(present)
                .or_insert_with(|| observation.draw.clone());
            family.observations.insert(key, observation);
        }
        ensure!(
            by_source.len() <= MAX_SOURCES,
            "too many copy-series sources"
        );
    }
    let mut families = by_source.into_values().collect::<Vec<_>>();
    loop {
        let mut merge = None;
        'pairs: for left in 0..families.len() {
            for right in left + 1..families.len() {
                let a = &families[left];
                let b = &families[right];
                if !compatible(&a.sources[0], &b.sources[0]) {
                    continue;
                }
                if physical_range(a.observations.values().next().context("empty family")?)?
                    != physical_range(b.observations.values().next().context("empty family")?)?
                {
                    continue;
                }
                let mut overlap = 0;
                for present in a.samples.keys() {
                    if b.samples.contains_key(present) {
                        same_frame(a.first_at(*present)?, b.first_at(*present)?)?;
                        overlap += 1;
                    }
                }
                if overlap >= MIN_OVERLAPPING_PRESENTS {
                    merge = Some((left, right));
                    break 'pairs;
                }
            }
        }
        let Some((left, right)) = merge else {
            break;
        };
        let other = families.remove(right);
        families[left].sources.extend(other.sources);
        families[left].sources.sort();
        for (p, sample) in other.samples {
            families[left].samples.entry(p).or_insert(sample);
        }
        for (p, draw) in other.first_draws {
            families[left].first_draws.entry(p).or_insert(draw);
        }
        families[left].observations.extend(other.observations);
    }
    Ok(families)
}

pub fn validate_pair(view: &SourceFamily, projection: &SourceFamily) -> anyhow::Result<()> {
    ensure!(
        view.sources.len() == projection.sources.len()
            && view.observations.len() == projection.observations.len()
            && view.samples.keys().eq(projection.samples.keys()),
        "copy-series camera pair coverage differs"
    );
    for (v, p) in view.sources.iter().zip(&projection.sources) {
        ensure!(
            v.role == crate::MatrixRole::View
                && p.role == crate::MatrixRole::Projection
                && v.descriptor_table == p.descriptor_table
                && v.cbv_resource_offset == p.cbv_resource_offset
                && v.declared_cbv_bytes == p.declared_cbv_bytes
                && v.draw_relation == p.draw_relation
                && v.matrix_offset != p.matrix_offset,
            "copy-series view/projection bindings differ"
        );
        for ((present, source, draw), observation) in &view.observations {
            if source != v {
                continue;
            }
            let other = projection
                .observations
                .get(&(*present, p.clone(), draw.clone()))
                .context("copy-series pair lacks matching exact draw")?;
            ensure!(
                receipt(observation)? == receipt(other)?,
                "copy-series pair has different physical copies"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_camera_alias::CopyCommandIdentity;
    use crate::{
        D3d12DescriptorTableBinding, D3d12DrawRelation, MatrixEncoding, MatrixLayout, MatrixRole,
    };

    fn observation(root: u32, present: u64, draw: u32) -> Observation {
        Observation {
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
                    matrix_offset: 0,
                    encoding: MatrixEncoding::F32LittleEndian,
                    layout: MatrixLayout::RowMajor,
                    transpose: true,
                    role: MatrixRole::View,
                    draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
                },
                matrix: [[present as f64; 4]; 4],
                receipt: Some(CopyReceipt {
                    schema: "dm-d3d12-executed-cbv-copy/v1".into(),
                    copy_submission: present * 100 + u64::from(root) * 3 + u64::from(draw),
                    queue_tag: 1,
                    destination_source_tag: 10,
                    destination_content_generation: 0,
                    destination_offset: 0,
                    source_source_tag: 20,
                    source_content_generation: 1,
                    source_offset: u64::from(draw) * 256,
                    descriptor_generation: u64::from(root) + u64::from(draw),
                    copied_bytes: 256,
                    bytes_sha256: format!("sha256:{present:064x}"),
                    copy_command: None,
                }),
            },
            draw: DrawIdentity {
                present_qpc: present as i64 * 1000,
                queue_tag: 1,
                command_list_tag: u64::from(root) + 30,
                command_list_generation: present,
                selected_depth_resource_tag: 7,
                selected_depth_resource_epoch: 1,
                selected_depth_descriptor_generation: 1,
                draw_ordinal: draw,
            },
        }
    }

    fn overlapping() -> Vec<Observation> {
        (1..=140)
            .map(|p| observation(0, p, 1))
            .chain((121..=260).map(|p| observation(18, p, 1)))
            .collect()
    }

    fn v2_aliases() -> Vec<Observation> {
        (1..=8)
            .flat_map(|present| {
                let mut a = observation(0, present, 1);
                let r = a.sample.receipt.as_mut().unwrap();
                r.schema = "dm-d3d12-executed-cbv-copy/v2".into();
                r.copy_command = Some(CopyCommandIdentity {
                    command_list_tag: a.draw.command_list_tag,
                    command_list_generation: a.draw.command_list_generation,
                    copy_ordinal: 1,
                });
                let mut b = a.clone();
                b.sample.source.descriptor_table.root_parameter_index = 18;
                b.sample.receipt.as_mut().unwrap().descriptor_generation += 100;
                [a, b]
            })
            .collect()
    }

    #[test]
    fn v2_same_copy_across_descriptor_generations_retains_every_binding() {
        let input = v2_aliases();
        let family = resolve(input.clone()).unwrap().remove(0);
        assert_eq!(family.sources.len(), 2);
        assert_eq!(family.samples.len(), 8);
        assert_eq!(family.observations.len(), 16);
        assert_eq!(
            family
                .observations
                .values()
                .filter(|o| { o.sample.receipt.as_ref().unwrap().descriptor_generation == 101 })
                .count(),
            8
        );
        let projections = input
            .into_iter()
            .map(|mut o| {
                o.sample.source.role = MatrixRole::Projection;
                o.sample.source.matrix_offset = 128;
                o
            })
            .collect();
        let mut projection = resolve(projections).unwrap().remove(0);
        validate_pair(&family, &projection).unwrap();
        // View/projection of one binding still require its exact descriptor evidence.
        projection
            .observations
            .values_mut()
            .last()
            .unwrap()
            .sample
            .receipt
            .as_mut()
            .unwrap()
            .descriptor_generation += 1;
        assert!(validate_pair(&family, &projection).is_err());
    }

    #[test]
    fn v2_aliases_reject_real_copy_payload_matrix_and_binding_conflicts() {
        for change in 0..10 {
            let mut input = v2_aliases();
            let mut bad = input.last().unwrap().clone();
            let r = bad.sample.receipt.as_mut().unwrap();
            match change {
                0 => r.copy_submission += 1,
                1 => r.copy_command.as_mut().unwrap().copy_ordinal += 1,
                2 => r.source_offset += 256,
                3 => r.bytes_sha256 = format!("sha256:{}", "c".repeat(64)),
                4 => bad.sample.matrix[2][2] += 0.001,
                5 => r.descriptor_generation += 1,
                6 => r.destination_content_generation += 1,
                7 => r.source_content_generation += 1,
                8 => r.copy_command.as_mut().unwrap().command_list_tag += 1,
                _ => r.copy_command.as_mut().unwrap().command_list_generation += 1,
            }
            input.push(bad);
            assert!(resolve(input).is_err(), "change {change}");
        }
    }

    #[test]
    fn v2_requires_command_and_binding_identity_without_upgrading_v1() {
        for change in 0..7 {
            let mut input = v2_aliases();
            let r = input.last_mut().unwrap().sample.receipt.as_mut().unwrap();
            match change {
                0 => r.copy_command = None,
                1 => r.copy_command.as_mut().unwrap().copy_ordinal = 0,
                2 => r.descriptor_generation = 0,
                3 => r.schema = "dm-d3d12-executed-cbv-copy/v1".into(),
                4 => r.schema = "dm-d3d12-executed-cbv-copy/v3".into(),
                5 => {
                    r.schema = "dm-d3d12-executed-cbv-copy/v1".into();
                    r.copy_command = None;
                }
                _ => {
                    // Historical same-draw receipts with different bindings stay rejected.
                    for o in &mut input {
                        let r = o.sample.receipt.as_mut().unwrap();
                        r.schema = "dm-d3d12-executed-cbv-copy/v1".into();
                        r.copy_command = None;
                    }
                }
            }
            assert!(resolve(input).is_err(), "change {change}");
        }
    }

    #[test]
    fn v2_aliases_still_require_eight_common_presents() {
        let mut input = v2_aliases();
        input.pop();
        assert_eq!(resolve(input).unwrap().len(), 2);
    }

    #[test]
    fn a_later_draw_cannot_mix_historical_and_new_receipt_versions() {
        let mut input = v2_aliases();
        let historical = observation(0, 9, 1);
        input.push(historical);
        assert!(
            resolve(input)
                .unwrap_err()
                .to_string()
                .contains("mixes receipt versions")
        );
    }

    #[test]
    fn distinct_draw_copies_are_retained_without_claiming_one_copy() {
        let mut input = overlapping();
        input.push(observation(0, 130, 2));
        let family = resolve(input).unwrap().remove(0);
        assert_eq!(family.sources.len(), 2);
        assert_eq!(family.samples.len(), 260);
        assert_eq!(family.observations.len(), 281);
        assert_eq!(
            family
                .observations
                .values()
                .filter(|o| o.sample.present == 130)
                .count(),
            3
        );
    }

    #[test]
    fn same_draw_cannot_claim_two_copies_even_with_identical_bytes() {
        let a = observation(0, 1, 1);
        let mut b = a.clone();
        b.sample.receipt.as_mut().unwrap().copy_submission += 1;
        assert!(
            resolve(vec![a, b])
                .unwrap_err()
                .to_string()
                .contains("selected draw")
        );
    }

    #[test]
    fn changing_root_parameter_does_not_hide_a_same_draw_copy_conflict() {
        let a = observation(0, 1, 1);
        let mut b = observation(18, 1, 1);
        b.draw = a.draw.clone();
        assert!(
            resolve(vec![a, b])
                .unwrap_err()
                .to_string()
                .contains("selected draw")
        );
    }

    #[test]
    fn missing_or_malformed_draw_and_receipt_evidence_is_rejected() {
        for change in 0..8 {
            let mut o = observation(0, 1, 1);
            match change {
                0 => o.sample.receipt = None,
                1 => o.draw.command_list_tag = 0,
                2 => o.draw.command_list_generation = 0,
                3 => o.draw.draw_ordinal = 0,
                4 => o.draw.queue_tag = 2,
                5 => o.sample.receipt.as_mut().unwrap().copied_bytes = 32,
                6 => o.sample.receipt.as_mut().unwrap().bytes_sha256 = "not-a-hash".into(),
                _ => o.sample.matrix[1][2] = f64::NAN,
            }
            assert!(resolve(vec![o]).is_err(), "change {change}");
        }
    }

    #[test]
    fn later_matrix_or_payload_conflict_poison_the_whole_family() {
        for change in 0..3 {
            let mut input = overlapping();
            let mut bad = observation(0, 130, 2);
            match change {
                0 => bad.sample.matrix[2][1] += 0.001,
                1 => {
                    bad.sample.receipt.as_mut().unwrap().bytes_sha256 =
                        format!("sha256:{}", "c".repeat(64))
                }
                _ => bad.draw.present_qpc += 1,
            }
            input.push(bad);
            assert!(resolve(input).is_err());
        }
    }

    #[test]
    fn equal_bytes_in_other_physical_ranges_do_not_merge() {
        for change in 0..5 {
            let mut input = overlapping();
            for o in &mut input[140..] {
                let r = o.sample.receipt.as_mut().unwrap();
                match change {
                    0 => r.destination_source_tag += 1,
                    1 => r.destination_content_generation += 1,
                    2 => r.source_source_tag += 1,
                    3 => r.source_content_generation += 1,
                    _ => {
                        r.queue_tag += 1;
                        o.draw.queue_tag += 1;
                    }
                }
            }
            assert_eq!(resolve(input).unwrap().len(), 2);
        }
    }

    #[test]
    fn one_source_cannot_switch_its_generation_bound_range() {
        let a = observation(0, 1, 1);
        let mut b = observation(0, 2, 1);
        b.sample
            .receipt
            .as_mut()
            .unwrap()
            .destination_content_generation += 1;
        assert!(resolve(vec![a, b]).is_err());
    }

    #[test]
    fn copy_submission_reuse_cannot_hide_changed_receipts() {
        let a = observation(0, 1, 1);
        let mut b = observation(0, 1, 2);
        b.sample.receipt.as_mut().unwrap().copy_submission =
            a.sample.receipt.as_ref().unwrap().copy_submission;
        assert!(
            resolve(vec![a, b])
                .unwrap_err()
                .to_string()
                .contains("submission")
        );
    }

    #[test]
    fn fewer_than_eight_overlap_presents_remain_ambiguous() {
        let input = (1..=140)
            .map(|p| observation(0, p, 1))
            .chain((134..=260).map(|p| observation(18, p, 1)))
            .collect();
        assert_eq!(resolve(input).unwrap().len(), 2);
    }

    #[test]
    fn every_retained_view_draw_requires_its_exact_projection_copy() {
        let input = overlapping();
        let view = resolve(input.clone()).unwrap().remove(0);
        let mut projections = input;
        for o in &mut projections {
            o.sample.source.role = MatrixRole::Projection;
            o.sample.source.matrix_offset = 128;
        }
        let mut projection = resolve(projections).unwrap().remove(0);
        validate_pair(&view, &projection).unwrap();
        projection
            .observations
            .values_mut()
            .last()
            .unwrap()
            .sample
            .receipt
            .as_mut()
            .unwrap()
            .copy_submission += 1;
        assert!(validate_pair(&view, &projection).is_err());
    }

    #[test]
    fn equal_pair_counts_do_not_hide_a_missing_projection_draw() {
        let input = overlapping();
        let view = resolve(input.clone()).unwrap().remove(0);
        let mut projections = input;
        for o in &mut projections {
            o.sample.source.role = MatrixRole::Projection;
            o.sample.source.matrix_offset = 128;
        }
        projections.last_mut().unwrap().draw.draw_ordinal += 1;
        let projection = resolve(projections).unwrap().remove(0);
        assert!(
            validate_pair(&view, &projection)
                .unwrap_err()
                .to_string()
                .contains("matching exact draw")
        );
    }
}
