//! Same-launch, discovery-only CBV alias reduction. Never emits a production binding.
//! Equal matrices or resource tags alone are not alias evidence. Every merged source retains its
//! full semantic key and every co-observation must name the same executed, generation-bound copy.
use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::D3d12MatrixSourceBinding;

/// One recorded CopyBufferRegion command. Ordinals are one-based within a list generation.
/// Re-executing the list still receives a new copy_submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyCommandIdentity {
    pub command_list_tag: u64,
    pub command_list_generation: u64,
    pub copy_ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyReceipt {
    pub schema: String,
    pub copy_submission: u64,
    pub queue_tag: u64,
    pub destination_source_tag: u64,
    pub destination_content_generation: u64,
    pub destination_offset: u64,
    pub source_source_tag: u64,
    pub source_content_generation: u64,
    pub source_offset: u64,
    /// Binding evidence, not physical-copy identity in v2. Equality of complete receipts
    /// remains required for repeated observations of the same semantic binding.
    pub descriptor_generation: u64,
    pub copied_bytes: u64,
    pub bytes_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_command: Option<CopyCommandIdentity>,
}

impl CopyReceipt {
    pub fn valid_versioned_identity(&self) -> bool {
        self.copy_submission > 0
            && self.descriptor_generation > 0
            && match (self.schema.as_str(), self.copy_command) {
                ("dm-d3d12-executed-cbv-copy/v1", None) => true,
                ("dm-d3d12-executed-cbv-copy/v2", Some(command)) => {
                    command.command_list_tag > 0
                        && command.command_list_generation > 0
                        && command.copy_ordinal > 0
                }
                _ => false,
            }
    }

    /// Compare exact physical evidence across bindings, never infer a copy from equal bytes.
    /// Old v1 evidence lacks command identity and keeps its original strict equality rule.
    pub fn same_physical_copy(&self, other: &Self) -> bool {
        if !self.valid_versioned_identity() || !other.valid_versioned_identity() {
            return false;
        }
        if self.schema != "dm-d3d12-executed-cbv-copy/v2" {
            return self == other;
        }
        self.schema == other.schema
            && self.copy_command == other.copy_command
            && self.copy_submission == other.copy_submission
            && self.queue_tag == other.queue_tag
            && self.destination_source_tag == other.destination_source_tag
            && self.destination_content_generation == other.destination_content_generation
            && self.destination_offset == other.destination_offset
            && self.source_source_tag == other.source_source_tag
            && self.source_content_generation == other.source_content_generation
            && self.source_offset == other.source_offset
            && self.copied_bytes == other.copied_bytes
            && self.bytes_sha256 == other.bytes_sha256
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub present: u64,
    pub source: D3d12MatrixSourceBinding,
    pub matrix: [[f64; 4]; 4],
    pub receipt: Option<CopyReceipt>,
}

#[derive(Debug)]
pub struct SourceGroup {
    pub sources: Vec<D3d12MatrixSourceBinding>,
    pub samples: BTreeMap<u64, Sample>,
}

fn compatible(left: &D3d12MatrixSourceBinding, right: &D3d12MatrixSourceBinding) -> bool {
    let mut normalized = right.clone();
    normalized.descriptor_table.root_parameter_index = left.descriptor_table.root_parameter_index;
    *left == normalized
}

fn valid_receipt(sample: &Sample) -> anyhow::Result<&CopyReceipt> {
    let receipt = sample
        .receipt
        .as_ref()
        .context("CBV alias lacks an executed-copy receipt")?;
    ensure!(
        receipt.valid_versioned_identity()
            && receipt.copy_submission > 0
            && receipt.queue_tag > 0
            && receipt.destination_source_tag > 0
            && receipt.source_source_tag > 0
            && receipt.descriptor_generation > 0
            && receipt.destination_offset == sample.source.cbv_resource_offset
            && receipt.copied_bytes <= u64::from(sample.source.declared_cbv_bytes)
            && u64::from(sample.source.matrix_offset) + 64 <= receipt.copied_bytes
            && receipt.bytes_sha256.len() == 71
            && receipt.bytes_sha256.starts_with("sha256:")
            && receipt.bytes_sha256[7..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "CBV alias receipt is incomplete or outside its declared range"
    );
    Ok(receipt)
}

/// Require repeated exact-copy co-observations, not numerical similarity. The existing minimum
/// sample count / coverage gate still applies after reduction. Missing old receipts cannot pass.
pub fn resolve(samples: Vec<Sample>) -> anyhow::Result<Vec<SourceGroup>> {
    let mut by_source = BTreeMap::<D3d12MatrixSourceBinding, BTreeMap<u64, Sample>>::new();
    for sample in samples {
        ensure!(
            sample
                .matrix
                .iter()
                .flatten()
                .all(|value| value.is_finite()),
            "non-finite camera matrix"
        );
        let rows = by_source.entry(sample.source.clone()).or_default();
        if let Some(previous) = rows.get(&sample.present) {
            ensure!(
                previous.matrix == sample.matrix && previous.receipt == sample.receipt,
                "one camera source has conflicting observations at one Present"
            );
        } else {
            rows.insert(sample.present, sample);
        }
    }
    ensure!(
        by_source.len() <= 64,
        "too many camera sources for bounded alias reduction"
    );
    let mut groups = by_source
        .into_iter()
        .map(|(source, samples)| SourceGroup {
            sources: vec![source],
            samples,
        })
        .collect::<Vec<_>>();
    loop {
        let mut merge = None;
        'pairs: for left in 0..groups.len() {
            for right in left + 1..groups.len() {
                if !compatible(&groups[left].sources[0], &groups[right].sources[0]) {
                    continue;
                }
                let mut overlap = 0;
                for (present, a) in &groups[left].samples {
                    if let Some(b) = groups[right].samples.get(present) {
                        // Any contradictory co-observation poisons the proposed alias family.
                        ensure!(
                            valid_receipt(a)?.same_physical_copy(valid_receipt(b)?)
                                && a.matrix == b.matrix,
                            "semantic CBV aliases disagree on physical copy or matrix at Present {present}"
                        );
                        overlap += 1;
                    }
                }
                if overlap >= 8 {
                    for sample in groups[left]
                        .samples
                        .values()
                        .chain(groups[right].samples.values())
                    {
                        valid_receipt(sample)?;
                    }
                    merge = Some((left, right));
                    break 'pairs;
                }
            }
        }
        let Some((left, right)) = merge else {
            break;
        };
        let other = groups.remove(right);
        groups[left].sources.extend(other.sources);
        groups[left].sources.sort();
        for (present, sample) in other.samples {
            groups[left].samples.entry(present).or_insert(sample);
        }
    }
    Ok(groups)
}

pub fn validate_pair(view: &SourceGroup, projection: &SourceGroup) -> anyhow::Result<()> {
    ensure!(
        view.sources.len() == projection.sources.len(),
        "view/projection alias families differ"
    );
    for (a, b) in view.sources.iter().zip(&projection.sources) {
        ensure!(
            a.descriptor_table == b.descriptor_table
                && a.cbv_resource_offset == b.cbv_resource_offset
                && a.declared_cbv_bytes == b.declared_cbv_bytes
                && a.draw_relation == b.draw_relation
                && a.matrix_offset != b.matrix_offset,
            "view/projection are not distinct matrices in one CBV family"
        );
    }
    if view.sources.len() > 1 {
        for (present, a) in &view.samples {
            let b = projection
                .samples
                .get(present)
                .context("aliased camera pair lacks a matching projection Present")?;
            ensure!(
                valid_receipt(a)? == valid_receipt(b)?,
                "view/projection copy receipts differ"
            );
        }
        ensure!(
            view.samples.len() == projection.samples.len(),
            "aliased camera pair coverage differs"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        D3d12DescriptorTableBinding, D3d12DrawRelation, MatrixEncoding, MatrixLayout, MatrixRole,
    };

    fn sample(root: u32, present: u64) -> Sample {
        Sample {
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
                copy_submission: present + 1,
                queue_tag: 1,
                destination_source_tag: 10,
                destination_content_generation: 0,
                destination_offset: 0,
                source_source_tag: 20,
                source_content_generation: 1,
                source_offset: 256,
                descriptor_generation: 5,
                copied_bytes: 256,
                bytes_sha256: format!("sha256:{}", "b".repeat(64)),
                copy_command: None,
            }),
        }
    }

    fn overlapping() -> Vec<Sample> {
        (0..140)
            .map(|p| sample(0, p))
            .chain((120..260).map(|p| sample(18, p)))
            .collect()
    }

    #[test]
    fn versioned_receipts_round_trip_without_reinterpreting_historical_evidence() {
        let v1 = sample(0, 1).receipt.unwrap();
        let json = serde_json::to_value(&v1).unwrap();
        assert!(json.get("copy_command").is_none());
        assert_eq!(serde_json::from_value::<CopyReceipt>(json).unwrap(), v1);
        let mut v2 = v1.clone();
        v2.schema = "dm-d3d12-executed-cbv-copy/v2".into();
        v2.copy_command = Some(CopyCommandIdentity {
            command_list_tag: 30,
            command_list_generation: 1,
            copy_ordinal: 1,
        });
        let mut other_binding = v2.clone();
        other_binding.descriptor_generation += 1;
        assert_ne!(v2, other_binding);
        assert!(v2.same_physical_copy(&other_binding));
        assert!(!v1.same_physical_copy(&v2));
        assert!(!v2.same_physical_copy(&v1));
        let json = serde_json::to_value(&v2).unwrap();
        assert_eq!(serde_json::from_value::<CopyReceipt>(json).unwrap(), v2);
    }

    #[test]
    fn v2_alias_reduction_accepts_binding_differences_only_for_one_exact_copy() {
        let mut samples = overlapping();
        for sample in &mut samples {
            let r = sample.receipt.as_mut().unwrap();
            r.schema = "dm-d3d12-executed-cbv-copy/v2".into();
            r.copy_command = Some(CopyCommandIdentity {
                command_list_tag: 30,
                command_list_generation: sample.present + 1,
                copy_ordinal: 1,
            });
            r.descriptor_generation +=
                u64::from(sample.source.descriptor_table.root_parameter_index);
        }
        assert_eq!(resolve(samples.clone()).unwrap().len(), 1);
        for change in 0..4 {
            let mut bad = samples.clone();
            let r = bad[140].receipt.as_mut().unwrap();
            match change {
                0 => r.copy_submission += 1,
                1 => r.copy_command.as_mut().unwrap().copy_ordinal += 1,
                2 => r.bytes_sha256 = format!("sha256:{}", "c".repeat(64)),
                _ => r.descriptor_generation = 0,
            }
            assert!(resolve(bad).is_err(), "change {change}");
        }
        let mut repeated_binding = samples[140].clone();
        repeated_binding
            .receipt
            .as_mut()
            .unwrap()
            .descriptor_generation += 1;
        samples.push(repeated_binding);
        assert!(resolve(samples).is_err());
    }

    #[test]
    fn exact_copy_alias_union_retains_both_semantic_keys_and_every_present() {
        let groups = resolve(overlapping()).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sources.len(), 2);
        assert_eq!(groups[0].samples.len(), 260);
        assert_eq!(
            groups[0].sources[1].descriptor_table.root_parameter_index,
            18
        );
    }

    #[test]
    fn equal_values_and_tags_without_copy_receipts_never_merge() {
        let mut samples = overlapping();
        for sample in &mut samples {
            sample.receipt = None;
        }
        assert!(resolve(samples).is_err());
    }

    #[test]
    fn changed_resource_generation_or_copy_submission_rejects_alias() {
        for change in 0..4 {
            let mut samples = overlapping();
            let receipt = samples[140].receipt.as_mut().unwrap();
            match change {
                0 => receipt.destination_content_generation += 1,
                1 => receipt.source_source_tag += 1,
                2 => receipt.copy_submission += 1,
                _ => receipt.descriptor_generation += 1,
            }
            assert!(resolve(samples).is_err());
        }
    }

    #[test]
    fn duplicate_source_conflict_is_not_overwritten() {
        let a = sample(0, 1);
        let mut b = a.clone();
        b.matrix[0][0] += 0.001;
        assert!(resolve(vec![a, b]).is_err());
    }

    #[test]
    fn disjoint_sources_or_different_declared_bindings_remain_separate() {
        let disjoint = (0..120)
            .map(|p| sample(0, p))
            .chain((120..240).map(|p| sample(18, p)))
            .collect();
        assert_eq!(resolve(disjoint).unwrap().len(), 2);
        let mut samples = overlapping();
        for sample in &mut samples[140..] {
            sample.source.descriptor_table.register_space = 1;
        }
        assert_eq!(resolve(samples).unwrap().len(), 2);
    }

    #[test]
    fn aliased_view_projection_must_have_matching_copy_receipts() {
        let view = resolve(overlapping()).unwrap().remove(0);
        let mut projection_samples = overlapping();
        for sample in &mut projection_samples {
            sample.source.role = MatrixRole::Projection;
            sample.source.matrix_offset = 128;
        }
        let mut projection = resolve(projection_samples).unwrap().remove(0);
        validate_pair(&view, &projection).unwrap();
        projection
            .samples
            .get_mut(&10)
            .unwrap()
            .receipt
            .as_mut()
            .unwrap()
            .copy_submission += 1;
        assert!(validate_pair(&view, &projection).is_err());
    }

    #[test]
    fn one_good_overlap_does_not_conceal_a_later_conflicting_alias() {
        let mut samples = overlapping();
        samples[150].matrix[2][1] += 1.0;
        assert!(resolve(samples).is_err());
    }
}
