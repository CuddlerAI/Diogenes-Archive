use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use serde::Serialize;

use crate::input::RawInputEvent;
use crate::mapping::{GameMapping, MouseControl};
use crate::profile::CaptureProfile;

pub const POINTER_CLAMP: i32 = 200;
pub const POINTER_MU: f64 = 8.0;
pub const POINTER_MAX_BIN: i32 = 10;

#[derive(Clone, Debug, Serialize)]
pub struct FrameTiming {
    pub frame: u64,
    pub session_frame: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub queue_drop: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameProjectionRow {
    pub frame: u64,
    pub session_frame: u64,
    pub run_id: String,
    pub video_t_us: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub source_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub queue_drop: u64,
    pub keys: Vec<u16>,
    /// Mouse buttons held at the end of the frame, as VK codes (#59 item 1).
    ///
    /// Same shape and semantics as `keys`, for the other half of the physical
    /// input state. Without it a consumer can only recover button state by
    /// replaying `events`, which is both duplicated work and wrong across an
    /// incremental resume: the projector's held state is re-seeded from a warm
    /// frame that is discarded, so the synthetic `mouse_down` never reaches any
    /// emitted frame. Firing and aiming are the strongest visual causes in an
    /// FPS, and a press landing between framepair samples is invisible without
    /// this field.
    pub buttons: Vec<u16>,
    pub actions: Vec<String>,
    #[serde(rename = "pointerDx")]
    pub pointer_dx: i32,
    #[serde(rename = "pointerDy")]
    pub pointer_dy: i32,
    #[serde(rename = "pointerBins")]
    pub pointer_bins: [i32; 2],
    pub events: Vec<FrameEvent>,
    pub src: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vk: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dx: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dy: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub horizontal: Option<bool>,
}

#[derive(Clone, Debug)]
struct PressedKey {
    vk: u16,
    semantic: Option<String>,
}

#[derive(Clone, Default)]
pub struct FrameProjector {
    pressed_keys: BTreeMap<(u16, bool), PressedKey>,
    pressed_mouse_buttons: BTreeMap<u16, Option<String>>,
    previous_timing: Option<FrameTiming>,
    /// Authoritative control-key set, when the caller can supply one.
    ///
    /// The `control` flag persisted on each event is not hardware evidence: it
    /// is the capture-time verdict of the game profile's `control_vks`. A wrong
    /// profile therefore freezes a wrong verdict into the archive — the COD
    /// corpus shipped `control_vks: [17, 18, ...]` while its mapping bound Ctrl
    /// to `move.prone`, so every prone press was dropped here and the loss
    /// propagated to every derived plane in silence. Callers that know the
    /// contract pass it in and the persisted flag is demoted to evidence;
    /// callers that don't fall back to trusting it, which keeps replaying an
    /// archived session byte-identical.
    control_vks: Option<BTreeSet<u16>>,
    dropped_control_events: BTreeMap<u16, u64>,
}

impl FrameProjector {
    /// Project under an explicit control-key set instead of the persisted flag.
    #[must_use]
    pub fn with_control_vks(control_vks: impl IntoIterator<Item = u16>) -> Self {
        Self {
            control_vks: Some(control_vks.into_iter().collect()),
            ..Self::default()
        }
    }

    /// Events skipped as control input since this projector was created, keyed
    /// by VK. Written into segment `meta.health` so a contract that starts
    /// swallowing a real binding shows up as a count that was always zero —
    /// the signal this whole class of bug was missing.
    #[must_use]
    pub fn dropped_control_events(&self) -> &BTreeMap<u16, u64> {
        &self.dropped_control_events
    }

    fn is_control(&self, event: &RawInputEvent) -> bool {
        match (&self.control_vks, event.vk) {
            (Some(control), Some(vk)) => control.contains(&vk),
            _ => event.control,
        }
    }

    pub fn project_segment(
        &mut self,
        timing: &[FrameTiming],
        events: &[RawInputEvent],
        run_id: &str,
        profile: &CaptureProfile,
        mapping: &GameMapping,
    ) -> anyhow::Result<Vec<FrameProjectionRow>> {
        ensure!(!timing.is_empty(), "cannot project an empty timing segment");
        validate_timing(timing, self.previous_timing.as_ref())?;
        validate_events(events, timing)?;

        let mut rows = Vec::with_capacity(timing.len());
        let mut next_event = 0_usize;
        for frame in timing {
            let mut compact_events = Vec::new();
            // Preserve actions that were active at the start of the frame even
            // when their key/button is released inside this frame window.
            let mut transition_actions = self
                .pressed_keys
                .values()
                .filter_map(|key| key.semantic.clone())
                .chain(self.pressed_mouse_buttons.values().filter_map(Clone::clone))
                .collect::<BTreeSet<_>>();
            let mut pointer_dx = 0_i64;
            let mut pointer_dy = 0_i64;

            while let Some(event) = events.get(next_event) {
                if event.qpc >= frame.output_qpc_end {
                    break;
                }
                ensure!(
                    event.qpc >= frame.output_qpc_start,
                    "Raw Input event does not belong to the next frame window"
                );
                next_event += 1;
                if self.is_control(event) {
                    if let Some(vk) = event.vk {
                        *self.dropped_control_events.entry(vk).or_default() += 1;
                    }
                    continue;
                }
                self.apply_event(
                    event,
                    mapping,
                    &mut compact_events,
                    &mut transition_actions,
                    &mut pointer_dx,
                    &mut pointer_dy,
                )?;
            }

            let mut keys = self
                .pressed_keys
                .values()
                .map(|key| key.vk)
                .collect::<Vec<_>>();
            keys.sort_unstable();
            keys.dedup();

            // End-of-frame held mouse buttons, mirroring `keys` above. The map is
            // already keyed by VK, so it is sorted and unique by construction;
            // collecting it explicitly keeps the emitted shape identical to
            // `keys` rather than depending on that.
            let buttons = self
                .pressed_mouse_buttons
                .keys()
                .copied()
                .collect::<Vec<_>>();

            let mut actions = transition_actions;
            actions.extend(
                self.pressed_keys
                    .values()
                    .filter_map(|key| key.semantic.clone()),
            );
            actions.extend(self.pressed_mouse_buttons.values().filter_map(Clone::clone));
            let pointer_dx = clamp_pointer(pointer_dx);
            let pointer_dy = clamp_pointer(pointer_dy);
            rows.push(FrameProjectionRow {
                frame: frame.frame,
                session_frame: frame.session_frame,
                run_id: run_id.to_owned(),
                video_t_us: video_time_us(frame.frame, profile)?,
                output_qpc_start: frame.output_qpc_start,
                output_qpc_end: frame.output_qpc_end,
                source_qpc: frame.source_qpc,
                source_seq: frame.source_seq,
                present_seq: frame.present_seq,
                duplicate: frame.duplicate,
                source_superseded: frame.source_superseded,
                queue_drop: frame.queue_drop,
                keys,
                buttons,
                actions: actions.into_iter().collect(),
                pointer_dx,
                pointer_dy,
                pointer_bins: [foveated_bin(pointer_dx), foveated_bin(pointer_dy)],
                events: compact_events,
                src: "rec",
            });
        }
        ensure!(
            next_event == events.len(),
            "Raw Input events remain after the final frame window"
        );
        self.previous_timing = timing.last().cloned();
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_event(
        &mut self,
        event: &RawInputEvent,
        mapping: &GameMapping,
        compact: &mut Vec<FrameEvent>,
        transition_actions: &mut BTreeSet<String>,
        pointer_dx: &mut i64,
        pointer_dy: &mut i64,
    ) -> anyhow::Result<()> {
        match event.event_type.as_str() {
            "key_down" | "key_up" => {
                if event.repeat.unwrap_or(false) {
                    return Ok(());
                }
                let vk = event.vk.context("keyboard event has no vk")?;
                let scan_code = event.scan_code.context("keyboard event has no scan_code")?;
                let flags = event.flags.unwrap_or_default();
                let extended = flags & 0x06 != 0;
                let semantic = mapping
                    .keyboard_semantic(scan_code, extended)
                    .map(str::to_owned);
                if event.event_type == "key_down" {
                    if let Some(action) = &semantic {
                        transition_actions.insert(action.clone());
                    }
                    self.pressed_keys
                        .insert((scan_code, extended), PressedKey { vk, semantic });
                } else {
                    self.pressed_keys.remove(&(scan_code, extended));
                }
                compact.push(FrameEvent::key(event, vk, scan_code, flags));
            }
            "mouse_move" => {
                ensure!(
                    !event.absolute.unwrap_or(false),
                    "absolute mouse input cannot be projected"
                );
                let dx = event.dx.context("mouse_move event has no dx")?;
                let dy = event.dy.context("mouse_move event has no dy")?;
                *pointer_dx = pointer_dx.saturating_add(i64::from(dx));
                *pointer_dy = pointer_dy.saturating_add(i64::from(dy));
                if dx != 0
                    && let Some(action) = mapping.mouse_control_semantic(MouseControl::DeltaX)
                {
                    transition_actions.insert(action.to_owned());
                }
                if dy != 0
                    && let Some(action) = mapping.mouse_control_semantic(MouseControl::DeltaY)
                {
                    transition_actions.insert(action.to_owned());
                }
                compact.push(FrameEvent::mouse_move(dx, dy));
            }
            "mouse_down" | "mouse_up" => {
                let button = event.vk.context("mouse button event has no vk")?;
                let semantic = mapping.mouse_button_semantic(button).map(str::to_owned);
                if event.event_type == "mouse_down" {
                    if let Some(action) = &semantic {
                        transition_actions.insert(action.clone());
                    }
                    self.pressed_mouse_buttons.insert(button, semantic);
                } else {
                    self.pressed_mouse_buttons.remove(&button);
                }
                compact.push(FrameEvent::mouse_button(event, button));
            }
            "wheel" => {
                let delta = event.delta.context("wheel event has no delta")?;
                let horizontal = event.horizontal.unwrap_or(false);
                let control = if horizontal {
                    MouseControl::WheelHorizontal
                } else {
                    MouseControl::Wheel
                };
                if let Some(action) = mapping.mouse_control_semantic(control) {
                    transition_actions.insert(action.to_owned());
                }
                compact.push(FrameEvent::wheel(delta, horizontal));
            }
            other => anyhow::bail!("unsupported Raw Input event type {other}"),
        }
        Ok(())
    }
}

impl FrameEvent {
    fn key(event: &RawInputEvent, vk: u16, scan_code: u16, flags: u16) -> Self {
        Self {
            event_type: event.event_type.clone(),
            vk: Some(vk),
            scan_code: Some(scan_code),
            flags: Some(flags),
            dx: None,
            dy: None,
            delta: None,
            horizontal: None,
        }
    }

    fn mouse_move(dx: i32, dy: i32) -> Self {
        Self {
            event_type: "mouse_move".into(),
            vk: None,
            scan_code: None,
            flags: None,
            dx: Some(dx),
            dy: Some(dy),
            delta: None,
            horizontal: None,
        }
    }

    fn mouse_button(event: &RawInputEvent, button: u16) -> Self {
        Self {
            event_type: event.event_type.clone(),
            vk: Some(button),
            scan_code: None,
            flags: None,
            dx: None,
            dy: None,
            delta: None,
            horizontal: None,
        }
    }

    fn wheel(delta: i16, horizontal: bool) -> Self {
        Self {
            event_type: "wheel".into(),
            vk: None,
            scan_code: None,
            flags: None,
            dx: None,
            dy: None,
            delta: Some(delta),
            horizontal: horizontal.then_some(true),
        }
    }
}

#[must_use]
pub fn foveated_bin(delta: i32) -> i32 {
    if delta == 0 {
        return 0;
    }
    let sign = delta.signum();
    let normalized = f64::from(delta.abs()) / f64::from(POINTER_CLAMP);
    let compressed = (1.0 + POINTER_MU * normalized).ln() / (1.0 + POINTER_MU).ln();
    sign * (compressed * f64::from(POINTER_MAX_BIN)).round() as i32
}

fn clamp_pointer(value: i64) -> i32 {
    value.clamp(-i64::from(POINTER_CLAMP), i64::from(POINTER_CLAMP)) as i32
}

fn video_time_us(frame: u64, profile: &CaptureProfile) -> anyhow::Result<u64> {
    let value = u128::from(frame)
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_mul(u128::from(profile.fps_den)))
        .context("frame video timestamp overflow")?
        / u128::from(profile.fps_num);
    u64::try_from(value).context("frame video timestamp exceeds u64")
}

fn validate_timing(
    timing: &[FrameTiming],
    previous_segment: Option<&FrameTiming>,
) -> anyhow::Result<()> {
    for (index, frame) in timing.iter().enumerate() {
        ensure!(
            frame.frame == index as u64,
            "segment frame index is not local"
        );
        ensure!(
            frame.output_qpc_end > frame.output_qpc_start,
            "frame QPC window is empty"
        );
        ensure!(
            frame.queue_drop == 0,
            "frame timing records a source queue drop"
        );
        if let Some(previous) = index
            .checked_sub(1)
            .and_then(|index| timing.get(index))
            .or(previous_segment.filter(|_| index == 0))
        {
            ensure!(
                frame.session_frame == previous.session_frame + 1,
                "session_frame is not monotonic"
            );
            ensure!(
                frame.output_qpc_start == previous.output_qpc_end,
                "frame QPC windows are not contiguous"
            );
            if frame.duplicate {
                ensure!(
                    frame.source_qpc == previous.source_qpc
                        && frame.source_seq == previous.source_seq,
                    "duplicate frame does not reference its previous source"
                );
            } else {
                ensure!(
                    frame.source_qpc > previous.source_qpc
                        && frame.source_seq > previous.source_seq,
                    "unique source frame is not monotonic: previous source QPC/seq {}/{}, current {}/{}",
                    previous.source_qpc,
                    previous.source_seq,
                    frame.source_qpc,
                    frame.source_seq
                );
            }
        } else {
            ensure!(
                !frame.duplicate,
                "the first run frame cannot be a duplicate"
            );
        }
    }
    Ok(())
}

fn validate_events(events: &[RawInputEvent], timing: &[FrameTiming]) -> anyhow::Result<()> {
    let start = timing.first().context("timing is empty")?.output_qpc_start;
    let end = timing.last().context("timing is empty")?.output_qpc_end;
    for (index, event) in events.iter().enumerate() {
        ensure!(
            event.qpc >= start && event.qpc < end,
            "Raw Input event is outside the timing range"
        );
        if let Some(previous) = index.checked_sub(1).and_then(|index| events.get(index)) {
            ensure!(
                (event.qpc, event.seq) > (previous.qpc, previous.seq),
                "Raw Input events are not ordered by QPC and sequence"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::MappingSnapshot;

    fn mapping() -> MappingSnapshot {
        MappingSnapshot::from_slice(
            br#"{
              "schema_version":"dm-mapping-2",
              "mapping_id":"game.test.pc",
              "version":"1",
              "game":{"id":"test","name":"Test","platform":"pc"},
              "taxonomy":{"content":"act","view":"first_person","material":"3d"},
              "recording_profile_id":"test-recording-v1",
              "bindings":[
                {"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"},
                {"device":"mouse","control":"delta_x","semantic":"camera.yaw"},
                {"device":"mouse","control":"delta_y","semantic":"camera.pitch"},
                {"device":"mouse","button":1,"semantic":"combat.primary_attack"}
              ]
            }"#,
        )
        .unwrap()
    }

    fn profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "test".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_bps: 1,
            max_bitrate_bps: 1,
            gop_frames: 240,
            segment_seconds: 1,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    fn event(seq: u64, qpc: i64, event_type: &str) -> RawInputEvent {
        RawInputEvent {
            seq,
            run_id: "r0000".into(),
            qpc,
            event_type: event_type.into(),
            device: "test".into(),
            vk: None,
            scan_code: None,
            flags: None,
            repeat: None,
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
            control: false,
        }
    }

    #[test]
    fn projects_half_open_events_pressed_state_actions_and_pointer() {
        let timing = vec![
            FrameTiming {
                frame: 0,
                session_frame: 10,
                output_qpc_start: 100,
                output_qpc_end: 200,
                source_qpc: 110,
                source_seq: 1,
                present_seq: None,
                duplicate: false,
                source_superseded: 0,
                queue_drop: 0,
            },
            FrameTiming {
                frame: 1,
                session_frame: 11,
                output_qpc_start: 200,
                output_qpc_end: 300,
                source_qpc: 210,
                source_seq: 2,
                present_seq: None,
                duplicate: false,
                source_superseded: 1,
                queue_drop: 0,
            },
        ];
        let mut down = event(0, 120, "key_down");
        down.vk = Some(87);
        down.scan_code = Some(17);
        down.flags = Some(0);
        let mut move_mouse = event(1, 150, "mouse_move");
        move_mouse.dx = Some(120);
        move_mouse.dy = Some(-14);
        move_mouse.absolute = Some(false);
        let mut click = event(2, 200, "mouse_down");
        click.vk = Some(1);
        let mut up = event(3, 240, "key_up");
        up.vk = Some(87);
        up.scan_code = Some(17);
        up.flags = Some(0);

        let mut projector = FrameProjector::default();
        let rows = projector
            .project_segment(
                &timing,
                &[down, move_mouse, click, up],
                "r0000",
                &profile(),
                mapping().mapping(),
            )
            .unwrap();
        assert_eq!(rows[0].keys, vec![87]);
        assert_eq!(
            rows[0].actions,
            vec!["camera.pitch", "camera.yaw", "move.forward"]
        );
        assert_eq!((rows[0].pointer_dx, rows[0].pointer_dy), (120, -14));
        assert_eq!(rows[0].pointer_bins, [8, -2]);
        assert_eq!(rows[0].video_t_us, 0);
        // #59 item 1: the click lands at qpc 200, which is frame 1's half-open
        // start, so frame 0 holds no button while frame 1 does. `buttons` is the
        // end-of-frame held state, exactly like `keys`.
        assert!(rows[0].buttons.is_empty());
        assert_eq!(rows[1].buttons, vec![1]);
        assert!(rows[1].keys.is_empty());
        assert_eq!(
            rows[1].actions,
            vec!["combat.primary_attack", "move.forward"]
        );
        assert_eq!(rows[1].video_t_us, 16_666);
        assert_eq!(rows[1].events[0].event_type, "mouse_down");
    }

    #[test]
    fn pointer_contract_is_symmetric_clamped_and_stable() {
        assert_eq!(foveated_bin(0), 0);
        assert_eq!(foveated_bin(120), 8);
        assert_eq!(foveated_bin(-14), -2);
        assert_eq!(foveated_bin(POINTER_CLAMP), POINTER_MAX_BIN);
        assert_eq!(foveated_bin(-POINTER_CLAMP), -POINTER_MAX_BIN);
    }

    #[test]
    fn duplicate_at_segment_boundary_references_previous_segment_source() {
        let first = [FrameTiming {
            frame: 0,
            session_frame: 10,
            output_qpc_start: 100,
            output_qpc_end: 200,
            source_qpc: 110,
            source_seq: 1,
            present_seq: Some(65_535),
            duplicate: false,
            source_superseded: 0,
            queue_drop: 0,
        }];
        let second = [FrameTiming {
            frame: 0,
            session_frame: 11,
            output_qpc_start: 200,
            output_qpc_end: 300,
            source_qpc: 110,
            source_seq: 1,
            present_seq: Some(65_535),
            duplicate: true,
            source_superseded: 0,
            queue_drop: 0,
        }];
        let mut projector = FrameProjector::default();
        projector
            .project_segment(&first, &[], "r0000", &profile(), mapping().mapping())
            .unwrap();
        let rows = projector
            .project_segment(&second, &[], "r0000", &profile(), mapping().mapping())
            .unwrap();
        assert!(rows[0].duplicate);
        assert_eq!((rows[0].source_qpc, rows[0].source_seq), (110, 1));
        assert_eq!(rows[0].present_seq, Some(65_535));
    }

    #[test]
    fn held_button_survives_a_segment_with_no_events() {
        // #59 item 1. A button pressed in one segment stays held through later
        // frames that carry no events of their own — the state lives in the
        // projector, not in the frames. This is the case a naive "replay each
        // frame's events" reconstruction gets wrong: the later frames have an
        // empty `events` array and would read as no button held.
        let first = vec![FrameTiming {
            frame: 0,
            session_frame: 0,
            output_qpc_start: 100,
            output_qpc_end: 200,
            source_qpc: 110,
            source_seq: 1,
            present_seq: None,
            duplicate: false,
            source_superseded: 0,
            queue_drop: 0,
        }];
        let second = vec![FrameTiming {
            frame: 0,
            session_frame: 1,
            output_qpc_start: 200,
            output_qpc_end: 300,
            source_qpc: 210,
            source_seq: 2,
            present_seq: None,
            duplicate: false,
            source_superseded: 0,
            queue_drop: 0,
        }];
        let mut click = event(0, 150, "mouse_down");
        click.vk = Some(1);

        let mut projector = FrameProjector::default();
        let first_rows = projector
            .project_segment(&first, &[click], "r0000", &profile(), mapping().mapping())
            .unwrap();
        assert_eq!(first_rows[0].buttons, vec![1]);

        // Second segment: no events at all, yet the button is still down.
        let second_rows = projector
            .project_segment(&second, &[], "r0000", &profile(), mapping().mapping())
            .unwrap();
        assert!(second_rows[0].events.is_empty());
        assert_eq!(second_rows[0].buttons, vec![1]);

        // Releasing it clears the state.
        let third = vec![FrameTiming {
            frame: 0,
            session_frame: 2,
            output_qpc_start: 300,
            output_qpc_end: 400,
            source_qpc: 310,
            source_seq: 3,
            present_seq: None,
            duplicate: false,
            source_superseded: 0,
            queue_drop: 0,
        }];
        let mut release = event(1, 350, "mouse_up");
        release.vk = Some(1);
        let third_rows = projector
            .project_segment(&third, &[release], "r0000", &profile(), mapping().mapping())
            .unwrap();
        assert!(third_rows[0].buttons.is_empty());
    }

    #[test]
    fn serialized_projection_matches_the_mixed_case_dm1_contract() {
        let row = FrameProjectionRow {
            frame: 5,
            session_frame: 7205,
            run_id: "r0001".into(),
            video_t_us: 83_333,
            output_qpc_start: 100,
            output_qpc_end: 200,
            source_qpc: 110,
            source_seq: 7,
            present_seq: Some(77),
            duplicate: false,
            source_superseded: 1,
            queue_drop: 0,
            keys: vec![87],
            buttons: vec![1],
            actions: vec!["move.forward".into()],
            pointer_dx: 120,
            pointer_dy: -14,
            pointer_bins: [8, -2],
            events: vec![FrameEvent {
                event_type: "key_down".into(),
                vk: Some(87),
                scan_code: Some(17),
                flags: Some(0),
                dx: None,
                dy: None,
                delta: None,
                horizontal: None,
            }],
            src: "rec",
        };
        let value = serde_json::to_value(row).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "session_frame",
            "run_id",
            "video_t_us",
            "output_qpc_start",
            "output_qpc_end",
            "source_qpc",
            "source_seq",
            "source_superseded",
            "queue_drop",
            "keys",
            "buttons",
            "pointerDx",
            "pointerDy",
            "pointerBins",
        ] {
            assert!(object.contains_key(key), "missing dm-1 field {key}");
        }
        assert!(!object.contains_key("sessionFrame"));
        assert!(!object.contains_key("runId"));
        assert!(!object.contains_key("videoTUs"));
        let compact_event = object["events"][0].as_object().unwrap();
        assert!(compact_event.contains_key("scan_code"));
        assert!(!compact_event.contains_key("scanCode"));
    }
}

#[cfg(test)]
mod control_policy_tests {
    use super::*;

    fn key_event(qpc: i64, seq: u64, vk: u16, scan_code: u16, control: bool) -> RawInputEvent {
        RawInputEvent {
            seq,
            run_id: "r0000".into(),
            qpc,
            device: "kbd:test".into(),
            event_type: "key_down".into(),
            control,
            vk: Some(vk),
            scan_code: Some(scan_code),
            flags: Some(0),
            repeat: Some(false),
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
        }
    }

    #[test]
    fn an_explicit_control_set_overrides_a_wrong_persisted_flag() {
        // Exactly the COD failure: Ctrl arrives stamped control=true because the
        // capture-time profile listed it, but the corrected contract does not.
        let projector = FrameProjector::with_control_vks([119, 121]);
        assert!(!projector.is_control(&key_event(1, 0, 17, 29, true)));
        assert!(projector.is_control(&key_event(2, 1, 119, 66, false)));
    }

    #[test]
    fn without_a_control_set_the_persisted_flag_still_rules() {
        // Replaying an archived session must stay byte-identical for callers
        // that cannot supply the contract.
        let projector = FrameProjector::default();
        assert!(projector.is_control(&key_event(1, 0, 17, 29, true)));
        assert!(!projector.is_control(&key_event(2, 1, 17, 29, false)));
    }

    #[test]
    fn dropped_control_events_are_counted_per_vk() {
        let mut projector = FrameProjector::with_control_vks([17]);
        assert!(projector.dropped_control_events().is_empty());
        for event in [
            key_event(1, 0, 17, 29, false),
            key_event(2, 1, 17, 29, false),
            key_event(3, 2, 87, 17, false),
        ] {
            if projector.is_control(&event)
                && let Some(vk) = event.vk
            {
                *projector.dropped_control_events.entry(vk).or_default() += 1;
            }
        }
        assert_eq!(projector.dropped_control_events().get(&17), Some(&2));
        assert_eq!(projector.dropped_control_events().get(&87), None);
    }
}
