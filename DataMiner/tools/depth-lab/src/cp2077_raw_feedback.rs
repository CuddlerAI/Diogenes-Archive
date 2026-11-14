//! Explicit local diagnostic bootstrap for the shared selected-Present protocol.
use crate::SelectedPresentFeedbackBootstrap;
use serde::{Deserialize, Serialize};

pub const REQUEST_SCHEMA: &str = "dm-cp2077-raw-selected-request/v2";
pub const READY_SCHEMA: &str = "dm-cp2077-raw-selected-ready/v2";
pub const SUMMARY_SCHEMA: &str = "dm-cp2077-raw-selected-summary/v2";
pub const DEPTH_SELECTION: &str = "even_cfr_30fps/v1";
pub const TARGET_DEPTH_HZ: u64 = 15;
pub const PAIRING_SCHEMA: &str = "dm-cp2077-local-raw-rgb-depth-pairing/v3";
pub const READY_FILE: &str = "raw-selected-ready.json";
pub const SUMMARY_FILE: &str = "raw-selected-summary.json";
pub const RGB_STOP_FILE: &str = "raw-selected-rgb-stop-request.json";
pub const GENERATION: u64 = 1;

#[derive(Clone, Debug, Deserialize)]
pub struct EvenCfrRgbFrame {
    pub session_frame: u64,
    pub present_seq: Option<u64>,
    pub source_qpc: i64,
    pub duplicate: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EvenCfrDepthFrame {
    pub ordinal: u64,
    pub present_seq: u64,
    pub present_qpc: i64,
}

#[derive(Debug, Serialize)]
pub struct EvenCfrBinding {
    pub session_frame: u64,
    pub present_seq: u64,
    pub source_qpc: i64,
    pub depth_ordinal: u64,
    pub depth_present_qpc: i64,
}

/// A complete map of required CFR slots to original depth identities. This is
/// independent of the existing 900-original minimum, pixel and camera gates.
#[derive(Debug, Serialize)]
pub struct EvenCfrMapping {
    pub depth_selection: &'static str,
    pub rgb_frames: usize,
    pub required_depth_rgb_frames: usize,
    pub distinct_depth_frames: usize,
    pub bindings: Vec<EvenCfrBinding>,
}

pub fn map_even_cfr_depth(
    rgb: &[EvenCfrRgbFrame],
    depth: &[EvenCfrDepthFrame],
    decoded_watermarks: &std::collections::BTreeMap<u64, u16>,
    max_compositor_lag_ticks: i64,
) -> Result<EvenCfrMapping, String> {
    use std::collections::{BTreeMap, BTreeSet};
    if rgb.is_empty()
        || rgb.len() > 7200
        || depth.is_empty()
        || depth.len() > 1024
        || max_compositor_lag_ticks <= 0
    {
        return Err("even-CFR inputs are empty or outside bounded budgets".into());
    }
    let mut by_present = BTreeMap::new();
    for (i, row) in depth.iter().enumerate() {
        if row.ordinal != i as u64
            || row.present_seq == 0
            || row.present_qpc <= 0
            || (i > 0
                && (row.present_seq <= depth[i - 1].present_seq
                    || row.present_qpc <= depth[i - 1].present_qpc))
            || by_present.insert(row.present_seq, row).is_some()
        {
            return Err("even-CFR raw depth index has conflicting or unordered identities".into());
        }
    }
    let mut used = BTreeSet::new();
    let mut bindings = Vec::with_capacity(rgb.len().div_ceil(2));
    for (i, row) in rgb.iter().enumerate() {
        let present = row
            .present_seq
            .ok_or("even-CFR RGB has no full Present identity")?;
        if row.session_frame != i as u64
            || present == 0
            || row.source_qpc <= 0
            || (i == 0 && row.duplicate)
            || (i > 0
                && (Some(present) < rgb[i - 1].present_seq
                    || row.source_qpc < rgb[i - 1].source_qpc
                    || (row.duplicate
                        && (Some(present) != rgb[i - 1].present_seq
                            || row.source_qpc != rgb[i - 1].source_qpc))))
        {
            return Err(format!(
                "even-CFR RGB identity/timeline conflict at frame {i}"
            ));
        }
        if !i.is_multiple_of(2) {
            continue;
        }
        if decoded_watermarks.get(&row.session_frame).copied() != Some(present as u16) {
            return Err(format!(
                "even-CFR required watermark missing/conflicting at frame {i}"
            ));
        }
        let d = by_present.get(&present).ok_or_else(|| {
            format!("even-CFR exact depth missing at frame {i}, Present {present}")
        })?;
        let lag = row
            .source_qpc
            .checked_sub(d.present_qpc)
            .ok_or("even-CFR QPC subtraction overflow")?;
        if !(0..=max_compositor_lag_ticks).contains(&lag) {
            return Err(format!("even-CFR causal QPC pairing failed at frame {i}"));
        }
        used.insert(present);
        bindings.push(EvenCfrBinding {
            session_frame: row.session_frame,
            present_seq: present,
            source_qpc: row.source_qpc,
            depth_ordinal: d.ordinal,
            depth_present_qpc: d.present_qpc,
        });
    }
    if used.len() != depth.len() {
        return Err("even-CFR raw depth contains an unrequested Present".into());
    }
    Ok(EvenCfrMapping {
        depth_selection: DEPTH_SELECTION,
        rgb_frames: rgb.len(),
        required_depth_rgb_frames: bindings.len(),
        distinct_depth_frames: used.len(),
        bindings,
    })
}

pub fn check_selected_frame_cap(distinct: u64, limit: u64) -> Result<(), String> {
    if limit == 0 || limit > 1024 || distinct > limit {
        return Err("raw selected original-depth frame cap exceeded".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawSelectedRequest {
    pub schema: String,
    pub feedback_schema: u16,
    pub depth_selection: String,
    pub holstered: bool,
    pub non_trainable: bool,
    pub bootstrap: SelectedPresentFeedbackBootstrap,
}

impl RawSelectedRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.bootstrap.validate().map_err(|e| e.to_string())?;
        let b = &self.bootstrap;
        if self.schema != REQUEST_SCHEMA
            || self.feedback_schema != crate::selected_present_feedback::FEEDBACK_SCHEMA_V4
            || self.depth_selection != DEPTH_SELECTION
            || !self.holstered
            || !self.non_trainable
            || b.width != 1920
            || b.height != 1080
            || b.bytes_per_pixel != 4
            || b.resource_epoch != 1
            || b.ring_capacity != 96
            || b.max_feedback_lag_presents != 88
            || b.guard_slots != 8
        {
            return Err("raw selected-Present requires the holstered native-1080p local diagnostic contract".into());
        }
        Ok(())
    }
}

pub fn stream(
    launch: &str,
    resource_epoch: u64,
) -> Result<crate::selected_present_feedback::StreamEpoch, String> {
    if launch.len() != 32
        || !launch
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid raw selected-Present launch identity".into());
    }
    Ok(crate::selected_present_feedback::StreamEpoch {
        identity: crate::selected_present_feedback::WireIdentity::new(
            &format!("raw-rgb-{launch}"),
            "r0000",
        )
        .map_err(|e| e.to_string())?,
        generation: GENERATION,
        resource_epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn fixture(
        presents: &[u64],
    ) -> (
        Vec<EvenCfrRgbFrame>,
        Vec<EvenCfrDepthFrame>,
        BTreeMap<u64, u16>,
    ) {
        let rgb: Vec<_> = presents
            .iter()
            .enumerate()
            .map(|(i, &present)| EvenCfrRgbFrame {
                session_frame: i as u64,
                present_seq: Some(present),
                source_qpc: present as i64 * 10 + 5,
                duplicate: i > 0 && presents[i - 1] == present,
            })
            .collect();
        let chosen: BTreeSet<_> = presents.iter().step_by(2).copied().collect();
        let depth = chosen
            .iter()
            .enumerate()
            .map(|(i, &present)| EvenCfrDepthFrame {
                ordinal: i as u64,
                present_seq: present,
                present_qpc: present as i64 * 10,
            })
            .collect();
        let watermarks = presents
            .iter()
            .enumerate()
            .map(|(i, &p)| (i as u64, p as u16))
            .collect();
        (rgb, depth, watermarks)
    }

    #[test]
    fn even_cfr_maps_inherited_odd_and_repeated_even_to_original_depth() {
        let (rgb, depth, marks) = fixture(&[100, 101, 101, 101, 101, 102, 103, 104]);
        let result = map_even_cfr_depth(&rgb, &depth, &marks, 100).unwrap();
        assert_eq!(
            (
                result.rgb_frames,
                result.required_depth_rgb_frames,
                result.distinct_depth_frames
            ),
            (8, 4, 3)
        );
        assert_eq!(
            result.bindings[1].depth_ordinal,
            result.bindings[2].depth_ordinal
        );
        assert_eq!(result.bindings[1].source_qpc, rgb[2].source_qpc);
        assert_eq!(result.bindings.last().unwrap().present_seq, 103);
    }

    #[test]
    fn even_cfr_requires_every_slot_not_just_all_remaining_depth_rows() {
        let (rgb, mut depth, marks) = fixture(&[100, 101, 102, 103, 104]);
        depth.remove(1);
        for (i, d) in depth.iter_mut().enumerate() {
            d.ordinal = i as u64;
        }
        let error = map_even_cfr_depth(&rgb, &depth, &marks, 100).unwrap_err();
        assert!(error.contains("exact depth missing at frame 2"), "{error}");
    }

    #[test]
    fn even_cfr_rejects_missing_watermark_wrong_full_identity_and_future_depth() {
        let (rgb, depth, marks) = fixture(&[100, 101, 102]);
        let mut missing = marks.clone();
        missing.remove(&2);
        assert!(map_even_cfr_depth(&rgb, &depth, &missing, 100).is_err());
        let mut wrapped = depth.clone();
        for row in &mut wrapped {
            row.present_seq += 65536;
        }
        assert!(map_even_cfr_depth(&rgb, &wrapped, &marks, 100).is_err());
        let mut future = depth.clone();
        future[0].present_qpc = rgb[0].source_qpc + 1;
        assert!(map_even_cfr_depth(&rgb, &future, &marks, 100).is_err());
        let mut stale = rgb.clone();
        stale[2].source_qpc += 1000;
        assert!(map_even_cfr_depth(&stale, &depth, &marks, 100).is_err());
    }

    #[test]
    fn even_cfr_rejects_rgb_holes_false_duplicates_and_extra_depth() {
        let (rgb, depth, marks) = fixture(&[100, 101, 102]);
        let mut hole = rgb.clone();
        hole[1].session_frame = 2;
        assert!(map_even_cfr_depth(&hole, &depth, &marks, 100).is_err());
        let mut dup = rgb.clone();
        dup[1].duplicate = true;
        assert!(map_even_cfr_depth(&dup, &depth, &marks, 100).is_err());
        let mut extra = depth.clone();
        extra.push(EvenCfrDepthFrame {
            ordinal: 2,
            present_seq: 103,
            present_qpc: 1030,
        });
        assert!(map_even_cfr_depth(&rgb, &extra, &marks, 100).is_err());
    }

    #[test]
    fn even_cfr_64_second_frame_budget_has_960_required_slots() {
        let (rgb, depth, marks) = fixture(&(100..2020).collect::<Vec<_>>());
        let mapped = map_even_cfr_depth(&rgb, &depth, &marks, 100).unwrap();
        assert_eq!(
            (
                mapped.required_depth_rgb_frames,
                mapped.distinct_depth_frames
            ),
            (960, 960)
        );
        for n in [1890, 1950] {
            let (rgb, depth, marks) = fixture(&(100..100 + n).collect::<Vec<_>>());
            assert_eq!(
                map_even_cfr_depth(&rgb, &depth, &marks, 100)
                    .unwrap()
                    .bindings
                    .len(),
                n as usize / 2
            );
        }
    }

    #[test]
    #[ignore = "requires an external private simulation; never edits source sessions"]
    fn replay_private_even_cfr_mapping() {
        #[derive(Deserialize)]
        struct Input {
            simulation_only: bool,
            rgb: Vec<EvenCfrRgbFrame>,
            depth: Vec<EvenCfrDepthFrame>,
            watermarks: BTreeMap<u64, u16>,
            max_lag_ticks: i64,
        }
        let path =
            std::env::var_os("CP2077_EVEN_CFR_REPLAY_JSON").expect("set private simulation path");
        let input: Input = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(input.simulation_only);
        let mapped = map_even_cfr_depth(
            &input.rgb,
            &input.depth,
            &input.watermarks,
            input.max_lag_ticks,
        )
        .unwrap();
        assert_eq!(mapped.bindings.len(), input.rgb.len().div_ceil(2));
        assert_eq!(mapped.distinct_depth_frames, input.depth.len());
        eprintln!(
            "private mapping: {} RGB slots, {} required slots, {} originals",
            input.rgb.len(),
            mapped.bindings.len(),
            mapped.distinct_depth_frames
        );
    }
    #[test]
    fn raw_feedback_does_not_expand_the_original_frame_budget_or_guess_identity() {
        assert!(check_selected_frame_cap(1024, 1024).is_ok());
        assert!(check_selected_frame_cap(1025, 1024).is_err());
        assert!(check_selected_frame_cap(1, 4096).is_err());
        let launch = "0123456789abcdef0123456789abcdef";
        assert_eq!(stream(launch, 1).unwrap().generation, 1);
        assert!(stream("old-launch", 1).is_err());
        assert!(stream(&launch.to_uppercase(), 1).is_err());
    }
}
