//! Pure D3D12 production depth-source selection.
//!
//! D3D12 does not expose an immediate-context DSV at `Present`. A production
//! source must therefore join three observations without guessing:
//!
//! 1. an exact DSV descriptor that was observed at creation time;
//! 2. the resource referenced when that descriptor is actually bound; and
//! 3. either an exact transition of the same resource out of `DEPTH_WRITE`, or
//!    a build-pinned exact projection draw independently validated from CBV bytes.
//!
//! This module deliberately contains no COM pointers or Windows calls. Runtime
//! pointers are opaque, process-local keys and are never serialized.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::probe_config::DiscoveryDepthFormat;

/// Complete, build-pinned D3D12 resource description used by a production
/// selector. Numeric fields use the corresponding D3D12/DXGI discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12ResourceIdentity {
    pub dimension: i32,
    pub width: u64,
    pub height: u32,
    pub depth_or_array_size: u16,
    pub mip_levels: u16,
    pub format: DiscoveryDepthFormat,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub layout: i32,
    pub flags: i32,
}

/// Exact DSV view description. An implicit/null DSV description is not enough
/// for production because its effective format cannot be independently joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12DsvIdentity {
    pub format: DiscoveryDepthFormat,
    pub dimension: i32,
    pub flags: i32,
}

/// Exact transition at which a readback implementation may copy the selected
/// depth plane without guessing application-owned resource state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12TransitionIdentity {
    pub subresource: u32,
    pub state_before: i32,
    pub state_after: i32,
    pub barrier_flags: i32,
}

/// Build-pinned fallback used only when the game does not expose the selected
/// depth resource's application transition on every runtime/driver. The
/// ordinal is zero-based within one DSV binding interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12FallbackDrawIdentity {
    pub root_signature_sha256: String,
    pub draw_ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3d12DepthSelection {
    pub resource: D3d12ResourceIdentity,
    pub view: D3d12DsvIdentity,
    pub transition: D3d12TransitionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_draw: Option<D3d12FallbackDrawIdentity>,
}

impl D3d12DepthSelection {
    /// Validate API-level invariants only. Game-specific truth still requires
    /// external evidence and the normal profile/rights authoring gates.
    pub fn validate(&self) -> Result<(), String> {
        if self.resource.dimension != 3
            || self.resource.width == 0
            || self.resource.height == 0
            || self.resource.depth_or_array_size != 1
            || self.resource.mip_levels != 1
            || self.resource.sample_count != 1
            || self.resource.sample_quality != 0
        {
            return Err(
                "D3D12 production depth resource must be a non-MSAA single-mip Texture2D".into(),
            );
        }
        if self.resource.flags & 2 == 0 {
            return Err("D3D12 production depth resource must declare ALLOW_DEPTH_STENCIL".into());
        }
        if self.view.dimension == 0 {
            return Err("D3D12 production DSV dimension must be explicit".into());
        }
        if self.view.flags & !3 != 0 {
            return Err("D3D12 production DSV flags contain unsupported bits".into());
        }
        let formats_compatible = matches!(
            (self.resource.format, self.view.format),
            (
                DiscoveryDepthFormat::R24G8Typeless,
                DiscoveryDepthFormat::D24UnormS8Uint
            ) | (
                DiscoveryDepthFormat::R32Typeless,
                DiscoveryDepthFormat::D32Float
            ) | (
                DiscoveryDepthFormat::R32G8X24Typeless,
                DiscoveryDepthFormat::D32FloatS8X24Uint
            )
        );
        if !formats_compatible {
            return Err("D3D12 production resource and DSV formats are incompatible".into());
        }
        if self.transition.subresource != 0
            || self.transition.state_before != 16
            || self.transition.barrier_flags != 0
            || self.transition.state_after == 0
            || self.transition.state_after & 16 != 0
        {
            return Err(
                "D3D12 production transition must be a non-split plane-0 transition out of DEPTH_WRITE"
                    .into(),
            );
        }
        if let Some(fallback) = &self.fallback_draw
            && (!crate::camera_binding::is_sha256_digest(&fallback.root_signature_sha256)
                || fallback.draw_ordinal > 4_095)
        {
            return Err("D3D12 fallback draw identity is invalid or unbounded".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct D3d12SelectionFault {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum D3d12SelectionStatus {
    WaitingForDsvBind,
    WaitingForEvidence { resource_key: u64 },
    Joined { resource_key: u64 },
    Faulted(D3d12SelectionFault),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionEvidence {
    ExactTransition,
    FallbackDraw,
}

/// Per-StartRun selector. A matching DSV bind is only a candidate: the resource
/// becomes selected only after the same resource also proves an accepted exact
/// transition or projection pass. If two candidates prove the active anchor,
/// selection remains fail-closed.
#[derive(Debug, Clone)]
pub struct D3d12ProductionSelector {
    expected: D3d12DepthSelection,
    bound_resources: BTreeSet<u64>,
    selected_resource: Option<u64>,
    selected_evidence: Option<SelectionEvidence>,
    selection_proven: bool,
    fault: Option<D3d12SelectionFault>,
}

impl D3d12ProductionSelector {
    pub fn new(expected: D3d12DepthSelection) -> Result<Self, String> {
        expected.validate()?;
        Ok(Self {
            expected,
            bound_resources: BTreeSet::new(),
            selected_resource: None,
            selected_evidence: None,
            selection_proven: false,
            fault: None,
        })
    }

    /// Observe a DSV that was resolved through the creation-time descriptor
    /// registry and is now actually bound on a command list.
    pub fn observe_bound_dsv(
        &mut self,
        resource_key: u64,
        resource: D3d12ResourceIdentity,
        view: D3d12DsvIdentity,
        view_desc_explicit: bool,
    ) -> bool {
        if self.fault.is_some()
            || !view_desc_explicit
            || resource != self.expected.resource
            || view != self.expected.view
        {
            return false;
        }
        self.bound_resources.insert(resource_key);
        true
    }

    pub fn observe_transition(
        &mut self,
        resource_key: u64,
        resource: D3d12ResourceIdentity,
        transition: D3d12TransitionIdentity,
    ) {
        if self.fault.is_some()
            || !self.bound_resources.contains(&resource_key)
            || resource != self.expected.resource
            || transition != self.expected.transition
        {
            return;
        }
        self.prove_resource(
            resource_key,
            SelectionEvidence::ExactTransition,
            "exact transition",
        );
    }

    /// Prove that a bound candidate owns the build-pinned main depth-source draw.
    /// The caller must establish the exact game-specific draw identity;
    /// this selector still rejects a second resource without fallback.
    pub fn observe_proven_draw(&mut self, resource_key: u64) {
        if self.fault.is_some() || !self.bound_resources.contains(&resource_key) {
            return;
        }
        self.prove_resource(
            resource_key,
            SelectionEvidence::FallbackDraw,
            "exact depth-source draw",
        );
    }

    #[must_use]
    pub fn status(&self) -> D3d12SelectionStatus {
        if let Some(fault) = &self.fault {
            return D3d12SelectionStatus::Faulted(fault.clone());
        }
        match (self.selected_resource, self.selection_proven) {
            (Some(resource_key), true) => D3d12SelectionStatus::Joined { resource_key },
            (Some(resource_key), false) => {
                D3d12SelectionStatus::WaitingForEvidence { resource_key }
            }
            (None, _) => self
                .bound_resources
                .first()
                .copied()
                .map_or(D3d12SelectionStatus::WaitingForDsvBind, |resource_key| {
                    D3d12SelectionStatus::WaitingForEvidence { resource_key }
                }),
        }
    }

    fn latch_fault(&mut self, code: &'static str, message: String) {
        if self.fault.is_none() {
            self.fault = Some(D3d12SelectionFault { code, message });
        }
    }

    fn prove_resource(
        &mut self,
        resource_key: u64,
        evidence_kind: SelectionEvidence,
        evidence: &str,
    ) {
        match (self.selected_resource, self.selected_evidence) {
            (None, _) => {
                self.selected_resource = Some(resource_key);
                self.selected_evidence = Some(evidence_kind);
                self.selection_proven = true;
            }
            (Some(selected), Some(_)) if selected == resource_key => {
                if evidence_kind == SelectionEvidence::ExactTransition {
                    self.selected_evidence = Some(SelectionEvidence::ExactTransition);
                }
                self.selection_proven = true;
            }
            (Some(_), Some(SelectionEvidence::FallbackDraw))
                if evidence_kind == SelectionEvidence::ExactTransition =>
            {
                self.selected_resource = Some(resource_key);
                self.selected_evidence = Some(SelectionEvidence::ExactTransition);
                self.selection_proven = true;
            }
            (Some(_), Some(SelectionEvidence::ExactTransition))
                if evidence_kind == SelectionEvidence::FallbackDraw => {}
            (Some(selected), Some(_)) => self.latch_fault(
                "d3d12_dsv_resource_ambiguous",
                format!(
                    "second matching D3D12 DSV resource proved the {evidence}: selected=0x{selected:016x} second=0x{resource_key:016x}"
                ),
            ),
            (Some(_), None) => self.latch_fault(
                "d3d12_dsv_selector_state_invalid",
                "selected D3D12 resource has no evidence provenance".into(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection() -> D3d12DepthSelection {
        D3d12DepthSelection {
            resource: D3d12ResourceIdentity {
                dimension: 3,
                width: 1920,
                height: 1080,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: DiscoveryDepthFormat::R24G8Typeless,
                sample_count: 1,
                sample_quality: 0,
                layout: 0,
                flags: 2,
            },
            view: D3d12DsvIdentity {
                format: DiscoveryDepthFormat::D24UnormS8Uint,
                dimension: 3,
                flags: 0,
            },
            transition: D3d12TransitionIdentity {
                subresource: 0,
                state_before: 16,
                state_after: 128,
                barrier_flags: 0,
            },
            fallback_draw: Some(D3d12FallbackDrawIdentity {
                root_signature_sha256: format!("sha256:{}", "a".repeat(64)),
                draw_ordinal: 0,
            }),
        }
    }

    #[test]
    fn join_requires_exact_bound_dsv_then_same_resource_transition() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        selector.observe_transition(7, expected.resource, expected.transition);
        assert_eq!(selector.status(), D3d12SelectionStatus::WaitingForDsvBind);

        assert!(selector.observe_bound_dsv(7, expected.resource, expected.view, true));
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::WaitingForEvidence { resource_key: 7 }
        );

        selector.observe_transition(7, expected.resource, expected.transition);
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::Joined { resource_key: 7 }
        );
    }

    #[test]
    fn implicit_or_wrong_view_never_selects_a_resource() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(!selector.observe_bound_dsv(1, expected.resource, expected.view, false));
        let mut wrong_view = expected.view;
        wrong_view.flags = 1;
        assert!(!selector.observe_bound_dsv(1, expected.resource, wrong_view, true));
        assert_eq!(selector.status(), D3d12SelectionStatus::WaitingForDsvBind);
    }

    #[test]
    fn clear_only_candidate_does_not_preempt_the_resource_with_the_exact_transition() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::WaitingForEvidence { resource_key: 1 }
        );
        selector.observe_transition(2, expected.resource, expected.transition);
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::Joined { resource_key: 2 }
        );
    }

    #[test]
    fn two_resources_proving_the_exact_transition_fault_without_fallback() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        selector.observe_transition(1, expected.resource, expected.transition);
        selector.observe_transition(2, expected.resource, expected.transition);
        let D3d12SelectionStatus::Faulted(fault) = selector.status() else {
            panic!("selector must fault on ambiguity");
        };
        assert_eq!(fault.code, "d3d12_dsv_resource_ambiguous");
    }

    #[test]
    fn exact_depth_source_draw_selects_only_a_bound_candidate() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        selector.observe_proven_draw(2);
        assert_eq!(selector.status(), D3d12SelectionStatus::WaitingForDsvBind);
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        selector.observe_proven_draw(2);
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::Joined { resource_key: 2 }
        );
    }

    #[test]
    fn two_resources_proving_the_exact_depth_source_draw_fault_without_fallback() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        selector.observe_proven_draw(1);
        selector.observe_proven_draw(2);
        let D3d12SelectionStatus::Faulted(fault) = selector.status() else {
            panic!("selector must fault on projection-pass ambiguity");
        };
        assert_eq!(fault.code, "d3d12_dsv_resource_ambiguous");
        assert!(fault.message.contains("exact depth-source draw"));
    }

    #[test]
    fn exact_transition_replaces_a_different_fallback_draw_candidate() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        selector.observe_proven_draw(1);
        selector.observe_transition(2, expected.resource, expected.transition);
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::Joined { resource_key: 2 }
        );
    }

    #[test]
    fn fallback_draw_cannot_displace_an_exact_transition_candidate() {
        let expected = selection();
        let mut selector = D3d12ProductionSelector::new(expected.clone()).unwrap();
        assert!(selector.observe_bound_dsv(1, expected.resource, expected.view, true));
        assert!(selector.observe_bound_dsv(2, expected.resource, expected.view, true));
        selector.observe_transition(1, expected.resource, expected.transition);
        selector.observe_proven_draw(2);
        assert_eq!(
            selector.status(),
            D3d12SelectionStatus::Joined { resource_key: 1 }
        );
    }

    #[test]
    fn validation_rejects_a_guessed_resource_state() {
        let mut expected = selection();
        expected.transition.state_before = 0;
        assert!(D3d12ProductionSelector::new(expected).is_err());
    }

    #[test]
    fn validation_rejects_an_impossible_resource_view_pair() {
        let mut expected = selection();
        expected.view.format = DiscoveryDepthFormat::D32Float;
        assert!(D3d12ProductionSelector::new(expected).is_err());
    }

    #[test]
    fn fallback_draw_identity_is_canonical_and_bounded() {
        let mut expected = selection();
        expected
            .fallback_draw
            .as_mut()
            .unwrap()
            .root_signature_sha256 = "not-a-hash".into();
        assert!(D3d12ProductionSelector::new(expected).is_err());

        let mut expected = selection();
        expected.fallback_draw.as_mut().unwrap().draw_ordinal = 4_096;
        assert!(D3d12ProductionSelector::new(expected).is_err());
    }
}
