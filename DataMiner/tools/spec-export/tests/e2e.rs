//! 端到端测试：用代码构造最小合成 dm-1 会话（两段、少量事件/帧、心跳），
//! 跑完整转换，断言输出行与数据侧 spec-v1 demo 样例形状一致。

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use spec_export::convert_session;

fn write_zstd_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    let compressed = zstd::encode_all(body.as_bytes(), 0).unwrap();
    fs::write(path, compressed).unwrap();
}

fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn session_json() -> Value {
    json!({
        "schema_version": "dm-local-dev-session-2",
        "session_id": "sess-e2e",
        "created_at_utc": "2026-07-24T08:00:00Z",
        "annotator_id": "ann_test",
        "consent": {"id": "cons_v3", "grant_id": "grant_1", "checked_at_utc": "2026-07-24T07:59:00Z"},
        "game": {"id": "game_sandbox", "slug": "sandbox", "version": "1.2.3", "exe": "sandbox.exe"},
        "capture_policy": {
            "source": "game_window_only",
            "width": 2560, "height": 1440,
            "fps_num": 60, "fps_den": 1,
            "segment_seconds": 60, "audio": false
        },
        "versions": {"client": "0.1.0+test", "contract": "dm-1"}
    })
}

fn mapping_json() -> Value {
    json!({
        "mapping_id": "game.sandbox.pc",
        "version": "2026-07-22.1",
        "bindings": [
            {"device": "keyboard", "scan_code": 17, "extended": false, "semantic": "move.forward"},
            {"device": "mouse", "control": "delta_x", "semantic": "camera.yaw"},
            {"device": "mouse", "button": 1, "semantic": "combat.primary_attack"}
        ]
    })
}

/// 两段目录式布局的合成会话（10MHz：ns 恰为 qpc×100）。
fn build_directory_layout_session(session_dir: &Path) {
    fs::create_dir_all(session_dir).unwrap();
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_vec_pretty(&session_json()).unwrap(),
    )
    .unwrap();
    fs::write(
        session_dir.join("mapping.json"),
        serde_json::to_vec_pretty(&mapping_json()).unwrap(),
    )
    .unwrap();
    write_jsonl(
        &session_dir.join("heartbeat.jsonl"),
        &[
            json!({"qpc": 10_000_000_u64, "seq": 0, "state": "recording"}),
            json!({"qpc": 20_000_000_u64, "seq": 1, "state": "paused"}),
            json!({"qpc": 30_000_000_u64, "seq": 2, "state": "stopping"}),
        ],
    );

    let run_dir = session_dir.join("runs/r0000");
    fs::create_dir_all(run_dir.join("segments/00000")).unwrap();
    fs::create_dir_all(run_dir.join("segments/00001")).unwrap();
    fs::write(
        run_dir.join("run.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": "dm-local-dev-recording-1",
            "runId": "r0000",
            "qpcFrequency": 10_000_000_u64,
            "anchorQpc": 10_000_000_u64
        }))
        .unwrap(),
    )
    .unwrap();

    // 段 0 事件故意乱序写入，验证 (qpc, seq) 全局排序。
    write_zstd_jsonl(
        &run_dir.join("segments/00000/events.jsonl.zst"),
        &[
            json!({"seq": 1, "run_id": "r0000", "qpc": 10_600_000_u64, "type": "key_down",
                   "device": "kbd:8f21", "vk": 87, "scan_code": 17, "flags": 0}),
            json!({"seq": 0, "run_id": "r0000", "qpc": 10_500_000_u64, "type": "mouse_move",
                   "device": "mouse:43ac", "dx": 12, "dy": -3, "absolute": false}),
            json!({"seq": 2, "run_id": "r0000", "qpc": 10_700_000_u64, "type": "mouse_down",
                   "device": "mouse:43ac", "vk": 1}),
        ],
    );
    write_zstd_jsonl(
        &run_dir.join("segments/00001/events.jsonl.zst"),
        &[
            json!({"seq": 3, "run_id": "r0000", "qpc": 11_000_000_u64, "type": "mouse_up",
                   "device": "mouse:43ac", "vk": 1}),
            json!({"seq": 4, "run_id": "r0000", "qpc": 11_100_000_u64, "type": "key_up",
                   "device": "kbd:8f21", "vk": 87, "scan_code": 17, "flags": 0}),
            json!({"seq": 5, "run_id": "r0000", "qpc": 11_200_000_u64, "type": "wheel",
                   "device": "mouse:43ac", "delta": -120}),
            json!({"seq": 6, "run_id": "r0000", "qpc": 11_300_000_u64, "type": "key_down",
                   "device": "kbd:8f21", "vk": 119, "scan_code": 66, "flags": 0, "control": true}),
        ],
    );

    write_zstd_jsonl(
        &run_dir.join("segments/00000/frames.jsonl.zst"),
        &[
            json!({"frame": 0, "session_frame": 0, "run_id": "r0000",
                   "output_qpc_start": 10_000_000_u64, "output_qpc_end": 10_166_667_u64,
                   "source_qpc": 10_005_000_u64, "duplicate": false}),
            json!({"frame": 1, "session_frame": 1, "run_id": "r0000",
                   "output_qpc_start": 10_166_667_u64, "output_qpc_end": 10_333_334_u64,
                   "source_qpc": 10_170_000_u64, "duplicate": false}),
            json!({"frame": 2, "session_frame": 2, "run_id": "r0000",
                   "output_qpc_start": 10_333_334_u64, "output_qpc_end": 10_500_001_u64,
                   "source_qpc": 10_170_000_u64, "duplicate": true}),
        ],
    );
    write_zstd_jsonl(
        &run_dir.join("segments/00001/frames.jsonl.zst"),
        &[
            json!({"frame": 0, "session_frame": 3, "run_id": "r0000",
                   "output_qpc_start": 10_500_001_u64, "output_qpc_end": 10_666_668_u64,
                   "source_qpc": 10_510_000_u64, "duplicate": false}),
            json!({"frame": 1, "session_frame": 4, "run_id": "r0000",
                   "output_qpc_start": 10_666_668_u64, "output_qpc_end": 10_833_335_u64,
                   "source_qpc": 10_670_000_u64, "duplicate": false}),
        ],
    );
}

#[test]
fn converts_a_two_segment_session_to_spec_v1() {
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sess-e2e");
    let output_dir = temp.path().join("out");
    build_directory_layout_session(&session_dir);

    let report = convert_session(&session_dir, &output_dir).unwrap();
    assert_eq!(report.session_id, "sess-e2e");
    assert_eq!(report.frames, 5);
    assert_eq!(report.actions, 7);
    assert_eq!(report.heartbeats, 3);
    assert!(
        report.warnings.is_empty(),
        "unexpected warnings: {:?}",
        report.warnings
    );

    // actions.jsonl：排序 + spec demo 形状。
    let actions = read_jsonl(&output_dir.join("actions.jsonl"));
    assert_eq!(actions.len(), 7);
    assert_eq!(
        actions[0],
        json!({"t_mono_ns": 1_050_000_000_i64, "type": "mouse_move", "dx": 12.0, "dy": -3.0})
    );
    assert_eq!(
        actions[1],
        json!({"t_mono_ns": 1_060_000_000_i64, "type": "key_down", "code": "W", "vk": 87})
    );
    assert_eq!(
        actions[2],
        json!({"t_mono_ns": 1_070_000_000_i64, "type": "mouse_button",
               "button": "left", "action": "down", "vk": 1})
    );
    assert_eq!(
        actions[3],
        json!({"t_mono_ns": 1_100_000_000_i64, "type": "mouse_button",
               "button": "left", "action": "up", "vk": 1})
    );
    assert_eq!(
        actions[4],
        json!({"t_mono_ns": 1_110_000_000_i64, "type": "key_up", "code": "W", "vk": 87})
    );
    assert_eq!(
        actions[5],
        json!({"t_mono_ns": 1_120_000_000_i64, "type": "wheel", "delta": -120})
    );
    // F8 控制热键保留并带 control 扩展字段。
    assert_eq!(
        actions[6],
        json!({"t_mono_ns": 1_130_000_000_i64, "type": "key_down",
               "code": "F8", "vk": 119, "control": true})
    );

    // frames.jsonl：session_frame → frame_idx，t 用 output_qpc_start 换算。
    let frames = read_jsonl(&output_dir.join("frames.jsonl"));
    assert_eq!(frames.len(), 5);
    assert_eq!(
        frames[0],
        json!({"frame_idx": 0, "t_mono_ns": 1_000_000_000_i64,
               "duplicate": false, "source_t_mono_ns": 1_000_500_000_i64})
    );
    assert_eq!(
        frames[2],
        json!({"frame_idx": 2, "t_mono_ns": 1_033_333_400_i64,
               "duplicate": true, "source_t_mono_ns": 1_017_000_000_i64})
    );
    let frame_idx: Vec<u64> = frames
        .iter()
        .map(|frame| frame["frame_idx"].as_u64().unwrap())
        .collect();
    assert_eq!(frame_idx, vec![0, 1, 2, 3, 4]);

    // heartbeat.jsonl：state 直通 collector。
    let heartbeats = read_jsonl(&output_dir.join("heartbeat.jsonl"));
    assert_eq!(
        heartbeats,
        vec![
            json!({"t_mono_ns": 1_000_000_000_i64, "seq": 0, "collector": "recording"}),
            json!({"t_mono_ns": 2_000_000_000_i64, "seq": 1, "collector": "paused"}),
            json!({"t_mono_ns": 3_000_000_000_i64, "seq": 2, "collector": "stopping"}),
        ]
    );

    // meta.json。
    let meta: Value =
        serde_json::from_str(&fs::read_to_string(output_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(
        meta,
        json!({
            "session_id": "sess-e2e",
            "game_id": "sandbox",
            "game_version": "1.2.3",
            "resolution": [2560, 1440],
            "target_fps": 60,
            "input_device": "mouse+keyboard",
            "player_anon_id": "ann_test",
            "consent": {"granted": true, "version": "cons_v3"},
            "pii_state": "raw",
            "clock": "monotonic_ns",
            "started_t_mono_ns": 1_000_000_000_i64,
            "ended_t_mono_ns": 1_066_666_800_i64,
            "target_fps_num": 60,
            "target_fps_den": 1,
            "recorded_keys": ["keyboard:scan17", "mouse:delta_x", "mouse:button1"],
            "spec_schema_version": "v1-20260724",
            "source_contract": "dm-1"
        })
    );

    // manifest.json：counts 与实际行数一致。
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(output_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest,
        json!({
            "session_id": "sess-e2e",
            "source": "client",
            "params": {
                "fps": 60,
                "fps_num": 60,
                "fps_den": 1,
                "width": 2560,
                "height": 1440,
                "client_version": "0.1.0+test",
                "source_schema_version": "dm-local-dev-session-2"
            },
            "counts": {"frames": 5, "actions": 7, "heartbeats": 3}
        })
    );
}

#[test]
fn flat_segment_layout_and_missing_heartbeat_are_supported() {
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sess-flat");
    let output_dir = temp.path().join("out");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_vec_pretty(&session_json()).unwrap(),
    )
    .unwrap();
    fs::write(
        session_dir.join("mapping.json"),
        serde_json::to_vec_pretty(&mapping_json()).unwrap(),
    )
    .unwrap();
    let run_dir = session_dir.join("runs/r0000");
    fs::create_dir_all(run_dir.join("segments")).unwrap();
    fs::write(
        run_dir.join("run.json"),
        serde_json::to_vec(&json!({"qpcFrequency": 10_000_000_u64})).unwrap(),
    )
    .unwrap();
    // docs/02、docs/08 的平铺命名：<seq>.events.jsonl.zst。
    write_zstd_jsonl(
        &run_dir.join("segments/00000.events.jsonl.zst"),
        &[json!({"seq": 0, "qpc": 10_600_000_u64, "type": "key_down", "vk": 87})],
    );
    write_zstd_jsonl(
        &run_dir.join("segments/00000.frames.jsonl.zst"),
        &[json!({"frame": 0, "session_frame": 0,
                 "output_qpc_start": 10_000_000_u64, "output_qpc_end": 10_166_667_u64,
                 "source_qpc": 10_005_000_u64, "duplicate": false})],
    );

    let report = convert_session(&session_dir, &output_dir).unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.actions, 1);
    assert_eq!(report.heartbeats, 0);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("heartbeat")),
        "missing heartbeat must be surfaced as a warning: {:?}",
        report.warnings
    );
    assert!(!output_dir.join("heartbeat.jsonl").exists());
}

#[test]
fn smoke_check_rejects_non_monotonic_frame_idx() {
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sess-bad");
    let output_dir = temp.path().join("out");
    build_directory_layout_session(&session_dir);
    // 把段 1 的帧改成 session_frame 倒退。
    write_zstd_jsonl(
        &session_dir.join("runs/r0000/segments/00001/frames.jsonl.zst"),
        &[json!({"frame": 0, "session_frame": 1,
                 "output_qpc_start": 10_500_001_u64, "output_qpc_end": 10_666_668_u64,
                 "source_qpc": 10_510_000_u64, "duplicate": false})],
    );

    let error = convert_session(&session_dir, &output_dir).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("frame_idx") && message.contains("smoke"),
        "expected smoke check failure about frame_idx, got: {message}"
    );
}

#[test]
fn refuses_a_non_empty_output_directory() {
    let temp = tempfile::tempdir().unwrap();
    let session_dir = temp.path().join("sess-e2e");
    let output_dir = temp.path().join("out");
    build_directory_layout_session(&session_dir);
    fs::create_dir_all(&output_dir).unwrap();
    fs::write(output_dir.join("stale.txt"), b"stale").unwrap();

    let error = convert_session(&session_dir, &output_dir).unwrap_err();
    assert!(format!("{error:#}").contains("not empty"));
}
