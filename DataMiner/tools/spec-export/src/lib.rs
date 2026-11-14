//! dm-1 RAW 会话 → 数据侧动作世界模型 spec-v1 的离线导出投影。
//!
//! 定位见 docs/17-action-world-model-spec-alignment.md：RAW 产物保持超集不动，
//! 所有 spec 兼容差异（VK↔键名、frames 字段投影、meta/manifest 组装、QPC→ns
//! 换算）都收敛在这个可重跑的转换器里。视频在 v0 不处理（分段 vs 单文件是
//! 对齐会阻塞项）。

pub mod qpc;
pub mod vk;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::qpc::qpc_to_mono_ns;
use crate::vk::{key_code_name, mouse_button_name};

/// 导出物回写的 spec 版本锚点；上游 schema.py 演进时同步更新。
pub const SPEC_SCHEMA_VERSION: &str = "v1-20260724";
/// 输入契约。
pub const SOURCE_CONTRACT: &str = "dm-1";

const ACTIONS_FILE: &str = "actions.jsonl";
const FRAMES_FILE: &str = "frames.jsonl";
const HEARTBEAT_FILE: &str = "heartbeat.jsonl";
const META_FILE: &str = "meta.json";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug)]
pub struct ConversionReport {
    pub session_id: String,
    pub frames: u64,
    pub actions: u64,
    pub heartbeats: u64,
    pub warnings: Vec<String>,
    pub output: PathBuf,
}

// ---------------------------------------------------------------------------
// dm-1 输入模型（只声明本转换器消费的字段，其余字段一律忽略：超集原则）
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct RawSession {
    schema_version: String,
    session_id: String,
    #[serde(default)]
    annotator_id: Option<String>,
    #[serde(default)]
    consent: Option<RawConsent>,
    game: RawGame,
    capture_policy: RawCapturePolicy,
    #[serde(default)]
    versions: Option<RawVersions>,
}

#[derive(Debug, serde::Deserialize)]
struct RawConsent {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RawGame {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RawCapturePolicy {
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
}

#[derive(Debug, serde::Deserialize)]
struct RawVersions {
    #[serde(default)]
    client: Option<String>,
}

/// `run.json` 是 camelCase（`RecordingOutcome` 序列化产物）。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRun {
    qpc_frequency: i64,
}

#[derive(Debug, serde::Deserialize)]
struct RawEventLine {
    #[serde(default)]
    seq: u64,
    qpc: i64,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    vk: Option<u16>,
    #[serde(default)]
    dx: Option<f64>,
    #[serde(default)]
    dy: Option<f64>,
    #[serde(default)]
    delta: Option<i64>,
    #[serde(default)]
    absolute: Option<bool>,
    #[serde(default)]
    horizontal: Option<bool>,
    #[serde(default)]
    control: bool,
}

#[derive(Debug, serde::Deserialize)]
struct RawFrameLine {
    session_frame: u64,
    output_qpc_start: i64,
    source_qpc: i64,
    #[serde(default)]
    duplicate: bool,
}

#[derive(Debug, serde::Deserialize)]
struct RawHeartbeatLine {
    qpc: i64,
    seq: u64,
    state: String,
}

#[derive(Debug, serde::Deserialize)]
struct RawMapping {
    #[serde(default)]
    bindings: Vec<RawBinding>,
}

#[derive(Debug, serde::Deserialize)]
struct RawBinding {
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    scan_code: Option<u32>,
    #[serde(default)]
    button: Option<u32>,
    #[serde(default)]
    control: Option<String>,
}

// ---------------------------------------------------------------------------
// spec-v1 输出模型（字段顺序即序列化顺序，与 spec demo 样例一致）
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ActionLine {
    t_mono_ns: i64,
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    button: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dx: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dy: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<i64>,
    // 以下为 dm-1 扩展字段（spec 超集，接收端可忽略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    vk: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    control: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    absolute: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    horizontal: Option<bool>,
}

#[derive(Debug, Serialize)]
struct FrameLine {
    frame_idx: u64,
    t_mono_ns: i64,
    // dm-1 扩展字段。
    duplicate: bool,
    source_t_mono_ns: i64,
}

#[derive(Debug, Serialize)]
struct HeartbeatLine {
    t_mono_ns: i64,
    seq: u64,
    collector: String,
}

#[derive(Debug, Serialize)]
struct SpecConsent {
    granted: bool,
    version: String,
}

#[derive(Debug, Serialize)]
struct SpecMeta {
    session_id: String,
    game_id: String,
    game_version: String,
    resolution: [u32; 2],
    target_fps: u32,
    input_device: &'static str,
    player_anon_id: String,
    consent: SpecConsent,
    pii_state: &'static str,
    clock: &'static str,
    started_t_mono_ns: i64,
    ended_t_mono_ns: i64,
    // dm-1 扩展字段。
    // target_fps 是整数近似（60000/1001 → 60），有理数真值只在这里保留；
    // 没有这两个字段,导出物就无法无损回溯采集帧率（docs/50 P-5c）。
    target_fps_num: u32,
    target_fps_den: u32,
    recorded_keys: Vec<String>,
    spec_schema_version: &'static str,
    source_contract: &'static str,
}

#[derive(Debug, Serialize)]
struct SpecManifestParams {
    fps: u32,
    // dm-1 扩展：整数 fps 的有理数真值（docs/50 P-5c）。
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    source_schema_version: String,
}

#[derive(Debug, Serialize)]
struct SpecCounts {
    frames: u64,
    actions: u64,
    heartbeats: u64,
}

#[derive(Debug, Serialize)]
struct SpecManifest {
    session_id: String,
    source: &'static str,
    params: SpecManifestParams,
    counts: SpecCounts,
}

// ---------------------------------------------------------------------------
// 会话发现
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SegmentSource {
    index: u64,
    events_path: PathBuf,
    frames_path: PathBuf,
}

#[derive(Debug)]
struct RunSource {
    run_index: usize,
    qpc_frequency: i64,
    segments: Vec<SegmentSource>,
}

fn discover_runs(session_dir: &Path) -> anyhow::Result<Vec<RunSource>> {
    let runs_dir = session_dir.join("runs");
    ensure!(
        runs_dir.is_dir(),
        "session has no runs directory: {}",
        runs_dir.display()
    );
    let mut run_dirs = Vec::new();
    for entry in
        fs::read_dir(&runs_dir).with_context(|| format!("failed to read {}", runs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() && name.starts_with('r') {
            run_dirs.push((name, path));
        }
    }
    run_dirs.sort_by(|a, b| a.0.cmp(&b.0));
    ensure!(!run_dirs.is_empty(), "session has no run directories");

    let mut runs = Vec::new();
    for (run_index, (name, run_dir)) in run_dirs.into_iter().enumerate() {
        let run: RawRun = read_json(&run_dir.join("run.json"))
            .with_context(|| format!("run {name}: failed to read run.json"))?;
        ensure!(
            run.qpc_frequency > 0,
            "run {name}: invalid qpc_frequency {}",
            run.qpc_frequency
        );
        let segments = discover_segments(&run_dir.join("segments"))
            .with_context(|| format!("run {name}: failed to discover segments"))?;
        runs.push(RunSource {
            run_index,
            qpc_frequency: run.qpc_frequency,
            segments,
        });
    }
    let frequency = runs[0].qpc_frequency;
    ensure!(
        runs.iter().all(|run| run.qpc_frequency == frequency),
        "runs disagree on qpc_frequency; a single session must share one QPC clock"
    );
    Ok(runs)
}

/// 同时支持两种段布局：
/// - 目录式（当前客户端实际产物）：`segments/00000/events.jsonl.zst`；
/// - 平铺式（docs/02、docs/08 的命名）：`segments/00000.events.jsonl.zst`。
fn discover_segments(segments_dir: &Path) -> anyhow::Result<Vec<SegmentSource>> {
    ensure!(
        segments_dir.is_dir(),
        "segments directory missing: {}",
        segments_dir.display()
    );
    let mut segments: BTreeMap<u64, SegmentSource> = BTreeMap::new();
    for entry in fs::read_dir(segments_dir)
        .with_context(|| format!("failed to read {}", segments_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let Ok(index) = name.parse::<u64>() else {
                continue; // 跳过 .tmp / quarantine 等非 ready 段目录。
            };
            let events_path = path.join("events.jsonl.zst");
            let frames_path = path.join("frames.jsonl.zst");
            if !events_path.is_file() && !frames_path.is_file() {
                continue;
            }
            ensure!(
                events_path.is_file() && frames_path.is_file(),
                "segment {index:05} is missing events or frames JSONL"
            );
            insert_segment(&mut segments, index, events_path, frames_path)?;
        } else if let Some(stem) = name.strip_suffix(".events.jsonl.zst") {
            let Ok(index) = stem.parse::<u64>() else {
                continue;
            };
            let frames_path = segments_dir.join(format!("{stem}.frames.jsonl.zst"));
            ensure!(
                frames_path.is_file(),
                "segment {index:05} has events but no frames JSONL"
            );
            insert_segment(&mut segments, index, path, frames_path)?;
        }
    }
    ensure!(
        !segments.is_empty(),
        "no ready segments found in {}",
        segments_dir.display()
    );
    Ok(segments.into_values().collect())
}

fn insert_segment(
    segments: &mut BTreeMap<u64, SegmentSource>,
    index: u64,
    events_path: PathBuf,
    frames_path: PathBuf,
) -> anyhow::Result<()> {
    ensure!(
        !segments.contains_key(&index),
        "segment {index:05} appears in both directory and flat layout"
    );
    segments.insert(
        index,
        SegmentSource {
            index,
            events_path,
            frames_path,
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 转换
// ---------------------------------------------------------------------------

pub fn convert_session(session_dir: &Path, output_dir: &Path) -> anyhow::Result<ConversionReport> {
    ensure!(
        session_dir.is_dir(),
        "session directory does not exist: {}",
        session_dir.display()
    );
    let session: RawSession = read_json(&session_dir.join("session.json"))?;
    let runs = discover_runs(session_dir)?;
    let qpc_frequency = runs[0].qpc_frequency;
    let mut warnings = Vec::new();

    prepare_output_dir(output_dir)?;

    let actions = write_actions(&runs, qpc_frequency, output_dir)?;
    let (frames, started_t_mono_ns, ended_t_mono_ns) =
        write_frames(&runs, qpc_frequency, output_dir)?;
    let heartbeats = write_heartbeats(session_dir, qpc_frequency, output_dir, &mut warnings)?;

    let recorded_keys = recorded_keys(session_dir, &mut warnings);
    let meta = build_meta(
        &session,
        started_t_mono_ns,
        ended_t_mono_ns,
        recorded_keys,
        &mut warnings,
    )?;
    write_json_pretty(&output_dir.join(META_FILE), &meta)?;

    let manifest = SpecManifest {
        session_id: session.session_id.clone(),
        source: "client",
        params: SpecManifestParams {
            fps: meta.target_fps,
            fps_num: session.capture_policy.fps_num,
            fps_den: session.capture_policy.fps_den,
            width: session.capture_policy.width,
            height: session.capture_policy.height,
            client_version: session
                .versions
                .as_ref()
                .and_then(|versions| versions.client.clone()),
            source_schema_version: session.schema_version.clone(),
        },
        counts: SpecCounts {
            frames,
            actions,
            heartbeats,
        },
    };
    write_json_pretty(&output_dir.join(MANIFEST_FILE), &manifest)?;

    smoke_check(output_dir).context("factory smoke check failed")?;

    Ok(ConversionReport {
        session_id: session.session_id,
        frames,
        actions,
        heartbeats,
        warnings,
        output: output_dir.to_path_buf(),
    })
}

fn prepare_output_dir(output_dir: &Path) -> anyhow::Result<()> {
    if output_dir.exists() {
        let mut entries = fs::read_dir(output_dir)
            .with_context(|| format!("failed to read {}", output_dir.display()))?;
        ensure!(
            entries.next().is_none(),
            "output directory is not empty: {}",
            output_dir.display()
        );
    } else {
        fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create {}", output_dir.display()))?;
    }
    Ok(())
}

fn write_actions(runs: &[RunSource], qpc_frequency: i64, output_dir: &Path) -> anyhow::Result<u64> {
    // (qpc, run_index, seq) 全局排序：seq 只在 run 内单调，跨 run 用 run 序补位。
    let mut events: Vec<(i64, usize, u64, RawEventLine)> = Vec::new();
    for run in runs {
        for segment in &run.segments {
            let lines: Vec<RawEventLine> =
                read_zstd_jsonl(&segment.events_path).with_context(|| {
                    format!("failed to read events of segment {:05}", segment.index)
                })?;
            for event in lines {
                events.push((event.qpc, run.run_index, event.seq, event));
            }
        }
    }
    events.sort_by_key(|(qpc, run_index, seq, _)| (*qpc, *run_index, *seq));

    let mut writer = jsonl_writer(&output_dir.join(ACTIONS_FILE))?;
    let count = events.len() as u64;
    for (_, _, _, event) in events {
        let line = translate_event(&event, qpc_frequency)
            .with_context(|| format!("failed to translate event at qpc={}", event.qpc))?;
        write_jsonl_line(&mut writer, &line)?;
    }
    writer.flush()?;
    Ok(count)
}

fn translate_event(event: &RawEventLine, qpc_frequency: i64) -> anyhow::Result<ActionLine> {
    let t_mono_ns = qpc_to_mono_ns(event.qpc, qpc_frequency)?;
    let control = event.control.then_some(true);
    let line = match event.event_type.as_str() {
        kind @ ("key_down" | "key_up") => {
            let vk = event.vk.context("key event without vk")?;
            ActionLine {
                t_mono_ns,
                event_type: if kind == "key_down" {
                    "key_down"
                } else {
                    "key_up"
                },
                code: Some(key_code_name(vk)),
                button: None,
                action: None,
                dx: None,
                dy: None,
                delta: None,
                vk: Some(vk),
                control,
                absolute: None,
                horizontal: None,
            }
        }
        kind @ ("mouse_down" | "mouse_up") => {
            let vk = event.vk.context("mouse button event without vk")?;
            ActionLine {
                t_mono_ns,
                event_type: "mouse_button",
                code: None,
                button: Some(mouse_button_name(vk)),
                action: Some(if kind == "mouse_down" { "down" } else { "up" }),
                dx: None,
                dy: None,
                delta: None,
                vk: Some(vk),
                control,
                absolute: None,
                horizontal: None,
            }
        }
        "mouse_move" => ActionLine {
            t_mono_ns,
            event_type: "mouse_move",
            code: None,
            button: None,
            action: None,
            dx: Some(event.dx.context("mouse_move without dx")?),
            dy: Some(event.dy.context("mouse_move without dy")?),
            delta: None,
            vk: None,
            control,
            absolute: event.absolute.filter(|absolute| *absolute),
            horizontal: None,
        },
        "wheel" => ActionLine {
            t_mono_ns,
            event_type: "wheel",
            code: None,
            button: None,
            action: None,
            dx: None,
            dy: None,
            delta: Some(event.delta.context("wheel without delta")?),
            vk: None,
            control,
            absolute: None,
            horizontal: event.horizontal.filter(|horizontal| *horizontal),
        },
        other => bail!("unknown dm-1 event type {other}; refusing to project silently"),
    };
    Ok(line)
}

fn write_frames(
    runs: &[RunSource],
    qpc_frequency: i64,
    output_dir: &Path,
) -> anyhow::Result<(u64, i64, i64)> {
    let mut writer = jsonl_writer(&output_dir.join(FRAMES_FILE))?;
    let mut count = 0_u64;
    let mut started = None;
    let mut ended = None;
    for run in runs {
        for segment in &run.segments {
            let rows: Vec<RawFrameLine> =
                read_zstd_jsonl(&segment.frames_path).with_context(|| {
                    format!("failed to read frames of segment {:05}", segment.index)
                })?;
            for row in rows {
                let t_mono_ns = qpc_to_mono_ns(row.output_qpc_start, qpc_frequency)?;
                let source_t_mono_ns = qpc_to_mono_ns(row.source_qpc, qpc_frequency)?;
                let line = FrameLine {
                    frame_idx: row.session_frame,
                    t_mono_ns,
                    duplicate: row.duplicate,
                    source_t_mono_ns,
                };
                write_jsonl_line(&mut writer, &line)?;
                started.get_or_insert(t_mono_ns);
                ended = Some(t_mono_ns);
                count += 1;
            }
        }
    }
    writer.flush()?;
    let started = started.context("session has no frames; nothing to export")?;
    let ended = ended.expect("ended is set whenever started is");
    Ok((count, started, ended))
}

const KNOWN_HEARTBEAT_STATES: [&str; 4] = ["recording", "paused", "degraded", "stopping"];

fn write_heartbeats(
    session_dir: &Path,
    qpc_frequency: i64,
    output_dir: &Path,
    warnings: &mut Vec<String>,
) -> anyhow::Result<u64> {
    let source = session_dir.join(HEARTBEAT_FILE);
    if !source.is_file() {
        warnings.push(
            "session has no heartbeat.jsonl (pre-heartbeat recording); skipping heartbeat output"
                .to_owned(),
        );
        return Ok(0);
    }
    let lines: Vec<RawHeartbeatLine> = read_jsonl(&source)?;
    let mut writer = jsonl_writer(&output_dir.join(HEARTBEAT_FILE))?;
    let count = lines.len() as u64;
    for line in lines {
        if !KNOWN_HEARTBEAT_STATES.contains(&line.state.as_str()) {
            warnings.push(format!(
                "heartbeat seq={} carries unknown state {:?}; passing through verbatim",
                line.seq, line.state
            ));
        }
        let out = HeartbeatLine {
            t_mono_ns: qpc_to_mono_ns(line.qpc, qpc_frequency)?,
            seq: line.seq,
            collector: line.state,
        };
        write_jsonl_line(&mut writer, &out)?;
    }
    writer.flush()?;
    Ok(count)
}

/// recorded_keys 口径（v0）：mapping.json 契约键声明的物理描述符投影，
/// `keyboard:scan<code>` / `mouse:button<n>` / `mouse:<control>`。
/// mapping 不可得时留空数组并给出警告（README 有说明）。
fn recorded_keys(session_dir: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let path = session_dir.join("mapping.json");
    let mapping: RawMapping = match read_json(&path) {
        Ok(mapping) => mapping,
        Err(error) => {
            warnings.push(format!(
                "mapping.json unavailable ({error:#}); recorded_keys left empty"
            ));
            return Vec::new();
        }
    };
    let mut keys = Vec::new();
    for binding in &mapping.bindings {
        let device = binding.device.as_deref().unwrap_or("unknown");
        if let Some(scan_code) = binding.scan_code {
            keys.push(format!("{device}:scan{scan_code}"));
        } else if let Some(button) = binding.button {
            keys.push(format!("{device}:button{button}"));
        } else if let Some(control) = &binding.control {
            keys.push(format!("{device}:{control}"));
        }
    }
    if keys.is_empty() {
        warnings.push("mapping.json has no recognizable bindings; recorded_keys left empty".into());
    }
    keys
}

fn build_meta(
    session: &RawSession,
    started_t_mono_ns: i64,
    ended_t_mono_ns: i64,
    recorded_keys: Vec<String>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<SpecMeta> {
    let game_id = session
        .game
        .slug
        .clone()
        .or_else(|| session.game.id.clone())
        .context("session.json game has neither slug nor id")?;
    let game_version = session.game.version.clone().unwrap_or_else(|| {
        warnings.push("session.json game.version missing; writing \"unknown\"".into());
        "unknown".to_owned()
    });
    let player_anon_id = session.annotator_id.clone().unwrap_or_else(|| {
        warnings.push("session.json annotator_id missing; writing \"unknown\"".into());
        "unknown".to_owned()
    });
    let consent_version = session
        .consent
        .as_ref()
        .and_then(|consent| consent.id.clone())
        .unwrap_or_else(|| {
            warnings.push("session.json consent.id missing; writing \"unknown\"".into());
            "unknown".to_owned()
        });
    let policy = &session.capture_policy;
    ensure!(
        policy.fps_den > 0,
        "capture_policy fps_den must be positive"
    );
    let target_fps = (policy.fps_num + policy.fps_den / 2) / policy.fps_den;
    Ok(SpecMeta {
        session_id: session.session_id.clone(),
        game_id,
        game_version,
        resolution: [policy.width, policy.height],
        target_fps,
        input_device: "mouse+keyboard",
        player_anon_id,
        consent: SpecConsent {
            granted: true,
            version: consent_version,
        },
        pii_state: "raw",
        clock: "monotonic_ns",
        started_t_mono_ns,
        ended_t_mono_ns,
        target_fps_num: policy.fps_num,
        target_fps_den: policy.fps_den,
        recorded_keys,
        spec_schema_version: SPEC_SCHEMA_VERSION,
        source_contract: SOURCE_CONTRACT,
    })
}

// ---------------------------------------------------------------------------
// 出厂 smoke 预检（行数对账 + 单调性；不复刻数据侧 Level-1 探针）
// ---------------------------------------------------------------------------

pub fn smoke_check(output_dir: &Path) -> anyhow::Result<()> {
    let manifest: serde_json::Value = read_json(&output_dir.join(MANIFEST_FILE))?;
    let counts = &manifest["counts"];
    let expect = |key: &str| -> anyhow::Result<u64> {
        counts[key]
            .as_u64()
            .with_context(|| format!("manifest counts.{key} missing"))
    };
    let expected_frames = expect("frames")?;
    let expected_actions = expect("actions")?;
    let expected_heartbeats = expect("heartbeats")?;

    let actions: Vec<serde_json::Value> = read_jsonl(&output_dir.join(ACTIONS_FILE))?;
    ensure!(
        actions.len() as u64 == expected_actions,
        "actions.jsonl has {} lines but manifest counts {} actions",
        actions.len(),
        expected_actions
    );
    check_monotonic_t(&actions, ACTIONS_FILE)?;

    let frames: Vec<serde_json::Value> = read_jsonl(&output_dir.join(FRAMES_FILE))?;
    ensure!(
        frames.len() as u64 == expected_frames,
        "frames.jsonl has {} lines but manifest counts {} frames",
        frames.len(),
        expected_frames
    );
    check_monotonic_t(&frames, FRAMES_FILE)?;
    let mut previous_idx: Option<u64> = None;
    for (line_no, frame) in frames.iter().enumerate() {
        let frame_idx = frame["frame_idx"]
            .as_u64()
            .with_context(|| format!("frames.jsonl line {}: frame_idx missing", line_no + 1))?;
        if let Some(previous) = previous_idx {
            ensure!(
                frame_idx > previous,
                "frames.jsonl line {}: frame_idx {frame_idx} does not increase past {previous}",
                line_no + 1
            );
        }
        previous_idx = Some(frame_idx);
    }

    let heartbeat_path = output_dir.join(HEARTBEAT_FILE);
    if expected_heartbeats == 0 {
        ensure!(
            !heartbeat_path.exists(),
            "manifest counts 0 heartbeats but heartbeat.jsonl exists"
        );
    } else {
        let heartbeats: Vec<serde_json::Value> = read_jsonl(&heartbeat_path)?;
        ensure!(
            heartbeats.len() as u64 == expected_heartbeats,
            "heartbeat.jsonl has {} lines but manifest counts {} heartbeats",
            heartbeats.len(),
            expected_heartbeats
        );
        check_monotonic_t(&heartbeats, HEARTBEAT_FILE)?;
    }
    Ok(())
}

fn check_monotonic_t(lines: &[serde_json::Value], file: &str) -> anyhow::Result<()> {
    let mut previous: Option<i64> = None;
    for (line_no, line) in lines.iter().enumerate() {
        let t = line["t_mono_ns"]
            .as_i64()
            .with_context(|| format!("{file} line {}: t_mono_ns missing", line_no + 1))?;
        if let Some(previous) = previous {
            ensure!(
                t >= previous,
                "{file} line {}: t_mono_ns {t} moves backwards past {previous}",
                line_no + 1
            );
        }
        previous = Some(t);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// IO helpers
// ---------------------------------------------------------------------------

fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    parse_jsonl(BufReader::new(file), path)
}

fn read_zstd_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let bytes = zstd::decode_all(BufReader::new(file))
        .with_context(|| format!("failed to decompress {}", path.display()))?;
    parse_jsonl(bytes.as_slice(), path)
}

fn parse_jsonl<T: DeserializeOwned>(reader: impl BufRead, path: &Path) -> anyhow::Result<Vec<T>> {
    let mut rows = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(&line).with_context(|| {
                format!("failed to parse {} line {}", path.display(), line_no + 1)
            })?,
        );
    }
    Ok(rows)
}

fn jsonl_writer(path: &Path) -> anyhow::Result<BufWriter<File>> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(BufWriter::new(file))
}

fn write_jsonl_line<T: Serialize>(writer: &mut impl Write, line: &T) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, line)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let mut body = serde_json::to_vec_pretty(value)?;
    body.push(b'\n');
    fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_key_event_matches_spec_shape() {
        let event = RawEventLine {
            seq: 1,
            qpc: 10_603_333,
            event_type: "key_down".into(),
            vk: Some(87),
            dx: None,
            dy: None,
            delta: None,
            absolute: None,
            horizontal: None,
            control: false,
        };
        let line = translate_event(&event, 10_000_000).unwrap();
        let json = serde_json::to_value(&line).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "t_mono_ns": 1_060_333_300_i64,
                "type": "key_down",
                "code": "W",
                "vk": 87
            })
        );
    }

    #[test]
    fn translate_mouse_button_matches_spec_shape() {
        let event = RawEventLine {
            seq: 2,
            qpc: 10_000_000,
            event_type: "mouse_up".into(),
            vk: Some(1),
            dx: None,
            dy: None,
            delta: None,
            absolute: None,
            horizontal: None,
            control: false,
        };
        let json = serde_json::to_value(translate_event(&event, 10_000_000).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "t_mono_ns": 1_000_000_000_i64,
                "type": "mouse_button",
                "button": "left",
                "action": "up",
                "vk": 1
            })
        );
    }

    #[test]
    fn control_events_keep_the_control_marker() {
        let event = RawEventLine {
            seq: 3,
            qpc: 20_000_000,
            event_type: "key_up".into(),
            vk: Some(0x77), // F8
            dx: None,
            dy: None,
            delta: None,
            absolute: None,
            horizontal: None,
            control: true,
        };
        let json = serde_json::to_value(translate_event(&event, 10_000_000).unwrap()).unwrap();
        assert_eq!(json["control"], serde_json::json!(true));
        assert_eq!(json["code"], serde_json::json!("F8"));
    }

    #[test]
    fn unknown_event_type_is_rejected() {
        let event = RawEventLine {
            seq: 4,
            qpc: 1,
            event_type: "telepathy".into(),
            vk: None,
            dx: None,
            dy: None,
            delta: None,
            absolute: None,
            horizontal: None,
            control: false,
        };
        assert!(translate_event(&event, 10_000_000).is_err());
    }

    #[test]
    fn mouse_move_and_wheel_pass_fields_through() {
        let mouse_move = RawEventLine {
            seq: 5,
            qpc: 10_583_333,
            event_type: "mouse_move".into(),
            vk: None,
            dx: Some(12.0),
            dy: Some(-3.0),
            delta: None,
            absolute: None,
            horizontal: None,
            control: false,
        };
        let json = serde_json::to_value(translate_event(&mouse_move, 10_000_000).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "t_mono_ns": 1_058_333_300_i64,
                "type": "mouse_move",
                "dx": 12.0,
                "dy": -3.0
            })
        );

        let wheel = RawEventLine {
            seq: 6,
            qpc: 10_653_333,
            event_type: "wheel".into(),
            vk: None,
            dx: None,
            dy: None,
            delta: Some(-120),
            absolute: None,
            horizontal: None,
            control: false,
        };
        let json = serde_json::to_value(translate_event(&wheel, 10_000_000).unwrap()).unwrap();
        assert_eq!(json["type"], serde_json::json!("wheel"));
        assert_eq!(json["delta"], serde_json::json!(-120));
    }
}
