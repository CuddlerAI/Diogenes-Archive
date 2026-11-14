//! `depth-lab` CLI —— E2E 链路的离线一半。
//!
//! ```text
//! depth-lab pair       --frames frames.jsonl --samples watermark-samples.jsonl \
//!                      --depth depth-index.jsonl --out frame-pairs.jsonl \
//!                      --report e2e-report.json [--camera-mode player|freecam]
//!
//! depth-lab materialize --raw <raw 目录> --depth depth-index.jsonl \
//!                       --out <variant-r 目录> --recipe variant-s-recipe.json \
//!                       --near N [--far F] [--metric-scale S] [--keep-spec <目录>]
//! ```
//!
//! 退出码：`0` 两道门都过；`1` 有门没过（报告仍然写出）；`2` 用法或 IO 错。
//! **门没过要区别于跑挂了**——前者是被测对象的问题，后者是工具的问题。

use std::collections::BTreeMap;
use std::fs;
#[cfg(feature = "client-orchestration")]
use std::io::Read;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use depth_lab::pairing::CameraMode;
use depth_lab::pipeline::{DepthIndexRow, RecorderFrameRow, WatermarkSample, run};

fn usage() -> &'static str {
    "用法：\n  \
     depth-lab pair --frames <file|segments-dir> --samples <file|samples-dir> --depth <d> --out <o> --report <r> [--camera-mode player|freecam]\n  \
     depth-lab clip-plan --frames <file|segments-dir> --samples <file|samples-dir> --depth <d> --target-frames <n> --out <plan.json>\n    \
       相位对齐选帧：给出「每个偶数输出帧都有 depth」的最长子序列与剔帧清单（docs/102 §6 PR-B）。\n  \
     depth-lab video-concat --segments <segments-dir> --out <video.mp4> [--plan <clip-plan.json>]\n  \
     depth-lab prepare-fallout4-action-context --frames <segments-dir> --session <session.json> --calibration <input-calibration.json> --axis-ini <Fallout4.ini> --wallclock-out <jsonl> --mouse-out <json>\n  \
     depth-lab camera-evidence --static <camera-candidates-POSE_STATIC.json> --walk <camera-candidates-POSE_WALK.json> --turn <camera-candidates-POSE_TURN.json> --out <camera-evidence-report.json> [--analysis-mode rigid-view|fallout-render-hybrid]\n  \
     depth-lab camera-evidence-sweep --root <camera-discovery-dir> --out <camera-evidence-sweep-report.json> [--analysis-mode rigid-view|fallout-render-hybrid] [--static-override <report>] [--walk-override <report>] [--turn-override <report>]\n  \
     depth-lab camera-binding-author --sweep <report> --static <report> --walk <report> --turn <report> --template <binding> [--trial-source <source.json> --trial-reason <reason>] --out <candidate.json> --manifest <authoring.json>\n  \
     depth-lab dsv-discovery-validate --input <probe-report.json> --evidence-root <dir> --out <new-child-dir>\n    \
       只校验并复制 discovery-only DSV 观测；不选择候选、不生成 profile/binding。\n  \
     depth-lab dsv-discovery-pass-summary --input <probe-report.json> --evidence-root <dir> --candidate <dsv-NNNN> --out <new-child-dir>\n    \
       需 --features evidence-authoring；绑定报告字节并输出解绑序号直方图，仍不选择候选/序号。\n  \
     depth-lab dsv-selection-attest --evidence-root <dir> --run-a <direct-child-dir> --candidate-a <dsv-NNNN> --run-b <direct-child-dir> --candidate-b <dsv-NNNN> --out <new-json>\n    \
       需 --features evidence-authoring；双独立 run + 双人工视觉复核，只输出 bridge-only DSV selection attestation。\n  \
     depth-lab dsv-selection-validate --evidence-root <abs-dir> --input <direct-child-json> --expected-locator <leaf.json> --expected-sha256 <sha256:...> --expected-executable-sha256 <sha256:...>\n    \
       需 --features selection-validation；重放 attestation 全部来源，只向 stdout 输出 bridge-only JSON。\n  \
     depth-lab camera-discovery-config-validate --input <probe-config.json> --expected-sha256 <sha256:...>\n    \
       需 --features selection-validation；重放 v3 selection 并验证纯 discovery-only plan，只向 stdout 输出 JSON。\n  \
     depth-lab camera-discovery-phase-validate --input <sealed-report.json> --expected-config <probe-config.json> --expected-config-sha256 <sha256:...>\n    \
       需 --features selection-validation；重放 config 绑定的 selection，并严格验证单份 v3 raw phase；只向 stdout 输出 discovery-only JSON。\n  \
     depth-lab perf-certification-manifest-validate --manifest <central-manifest.json>\n  \
     depth-lab perf-certification-check --manifest <central-manifest.json> --runtime <runtime-manifest.json> --dll-sha256 <sha256:...> --key <perf-key.json> --hardware <hardware.json>\n  \
     depth-lab assemble-action --depth <jsonl> --camera <jsonl> --player <jsonl> --actions <jsonl.zst> --pairs <jsonl> --wallclock <jsonl> --mouse <json> --qpc-frequency <hz> --out <jsonl>\n  \
     depth-lab assemble-action-30 --camera <jsonl> --player <jsonl> --actions <jsonl.zst> --pairs <jsonl> --wallclock <jsonl> --mouse <json> --qpc-frequency <hz> --out <jsonl>\n  \
     depth-lab materialize --raw <dir> --depth <d> --out <dir> --recipe <f> --near <N> [--far <F>] [--metric-scale <S>] [--keep-spec <dir>]\n  \
     depth-lab deliver --capture <采集目录> --pairs <frame-pairs.jsonl> --out <交付根> --clip-id <名> [--video <mp4>] [--pose <pose.jsonl>] [--pose30 <pose.jsonl>] [--camera-index <camera-pose-index.jsonl>] [--plan <clip-plan.json>|--limit <N>]\n    \
       产 68 号五件套（video/depth/两份 json/systeminfo）。缺件仍出 depth 但退出码非零。\n  \
       FO4 render-hybrid pose 必须同时给原始 --camera-index；deliver 会在写任何输出前核对仓库内固定 E-geo 授权。\n  \
     depth-lab push --dir <采集目录> --bucket <桶名> [--dry-run] [--allow-nondeliverable]\n    \
       凭据从 R2_ACCOUNT_ID / R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY 读，不接受命令行参数。\n  \
     depth-lab ets2-pose --telemetry <jsonl> --pairs <frame-pairs.jsonl> --config <json> --out <pose.jsonl> [--out-index <jsonl>] --report <json>\n    \
       ETS2：SCS 遥测流按 QPC 对齐配对表。--out 是全速率（喂 --pose30），\n    \
       --out-index 是按 present_seq 去重的（喂 --pose），缺省为 <out>.index.jsonl。\n  \
     depth-lab ets2-map-bounds --sectors <每行一个扇区文件名的 txt> --out <json>\n    \
       ETS2：解包出的 sec*.base 文件名 → 68 号 §6.1 的四角边界。"
}

#[cfg(test)]
mod perf_certification_cli_tests {
    use std::collections::BTreeMap;

    use super::cmd_perf_certification_check;

    fn sha(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn key() -> serde_json::Value {
        serde_json::json!({
            "gameId": "fallout4-steam-release-test",
            "rendererBackend": "d3d11",
            "backendGeneration": 4,
            "depthProfileSha256": sha('b'),
            "cameraBindingSha256": sha('c'),
            "selectedPresentMode": "gpu_ring_v1",
            "unbindFromEnd": 4,
            "feedbackSchema": 3,
            "queueCapacity": 256,
            "ringCapacity": 96,
            "maxFeedbackLagPresents": 88,
            "guardSlots": 8,
            "width": 1280,
            "height": 720,
            "bytesPerPixel": 4,
            "vramLimitBytes": 536870912,
            "vramReserveBytes": 268435456
        })
    }

    fn hardware(model: &str) -> serde_json::Value {
        serde_json::json!({
            "gpuModel": model,
            "gpuDriverVersion": "32.0.1",
            "gpuAdapterLuidHigh": 1,
            "gpuAdapterLuidLow": 2
        })
    }

    fn runtime() -> serde_json::Value {
        serde_json::json!({
            "schema": "dm-depth-probe-runtime-manifest-v1",
            "runtimeVersion": "depth-2026.08.1",
            "dllSha256": sha('a'),
            "d3d11Generation": 4,
            "d3d12Generation": 9
        })
    }

    fn manifest() -> serde_json::Value {
        serde_json::json!({
            "schema": "dm-depth-performance-certifications-v1",
            "approvedRuntimes": [{
                "runtimeVersion": "depth-2026.08.1",
                "dllSha256": sha('a'),
                "d3d11Generation": 4,
                "d3d12Generation": 9
            }],
            "certifications": [{
                "certificationId": "fo4-g4",
                "key": key(),
                "measuredRuntime": {
                    "runtimeVersion": "depth-2026.08.1",
                    "dllSha256": sha('a'),
                    "d3d11Generation": 4,
                    "d3d12Generation": 9
                },
                "baselineHardware": hardware("GPU A"),
                "coveredGpuModels": ["GPU A", "GPU B"],
                "avgFpsDrop": 0.01,
                "low1FpsDrop": 0.02,
                "measuredOn": "2026-08-28"
            }]
        })
    }

    fn run(
        manifest: serde_json::Value,
        runtime: serde_json::Value,
        key: serde_json::Value,
        hardware: serde_json::Value,
        dll_sha256: String,
    ) -> Result<bool, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dm-perf-cert-check-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let manifest_path = root.join("manifest.json");
        let runtime_path = root.join("runtime.json");
        let key_path = root.join("key.json");
        let hardware_path = root.join("hardware.json");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        std::fs::write(&runtime_path, serde_json::to_vec(&runtime).unwrap()).unwrap();
        std::fs::write(&key_path, serde_json::to_vec(&key).unwrap()).unwrap();
        std::fs::write(&hardware_path, serde_json::to_vec(&hardware).unwrap()).unwrap();
        let arguments = BTreeMap::from([
            ("manifest".into(), manifest_path.display().to_string()),
            ("runtime".into(), runtime_path.display().to_string()),
            ("key".into(), key_path.display().to_string()),
            ("hardware".into(), hardware_path.display().to_string()),
            ("dll-sha256".into(), dll_sha256),
        ]);
        let result = cmd_perf_certification_check(&arguments);
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn explicit_covered_hardware_passes() {
        assert_eq!(
            run(manifest(), runtime(), key(), hardware("GPU B"), sha('a')),
            Ok(true)
        );
    }

    #[test]
    fn unknown_hardware_and_tampered_dll_miss() {
        assert_eq!(
            run(manifest(), runtime(), key(), hardware("GPU C"), sha('a')),
            Ok(false)
        );
        assert_eq!(
            run(manifest(), runtime(), key(), hardware("GPU A"), sha('d')),
            Ok(false)
        );
    }
}

/// ETS2：遥测流 → 位姿旁车。
#[cfg(feature = "ets2")]
fn cmd_ets2_pose(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::ets2;

    let telemetry = ets2::read_telemetry(std::path::Path::new(&require(m, "telemetry")?))?;
    let pairs = ets2::read_pairs(std::path::Path::new(&require(m, "pairs")?))?;
    let config = ets2::read_config(std::path::Path::new(&require(m, "config")?))?;
    let (rows, report) = ets2::build_pose_rows(&pairs, &telemetry, &config)?;
    // 两份文件不是冗余：--pose30 要每个视频帧一行，--pose 要每个 present_seq 一行，
    // 而 CFR duplicate 帧合法地共用一个 Present。详见 ets2::collapse_by_present_seq。
    let full_rate = PathBuf::from(require(m, "out")?);
    ets2::write_pose_index(&full_rate, &rows)?;
    let index_path = match m.get("out-index") {
        Some(path) => PathBuf::from(path),
        None => full_rate.with_extension("index.jsonl"),
    };
    let collapsed = ets2::collapse_by_present_seq(&rows)?;
    ets2::write_pose_index(&index_path, &collapsed)?;
    let report_path = PathBuf::from(require(m, "report")?);
    fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写报告失败: {e}"))?;
    println!(
        "位姿 {} 行（去重后 {} 行 → {}）/ 视频 {} 帧；skew p50={}us p95={}us max={}us（闸 {}us）",
        report.pose_rows,
        collapsed.len(),
        index_path.display(),
        report.video_frames,
        report.skew_p50_us,
        report.skew_p95_us,
        report.skew_max_us,
        report.max_skew_us
    );
    if !report.roll_calibrated {
        println!("⚠️ roll 符号未实机标定；交付元数据会带 `unvalidated`");
    }
    if !report.offset_composition_calibrated {
        println!("⚠️ 驾驶舱/头部偏移复合次序未标定；本次要求偏移可忽略");
    }
    if !report.is_deliverable() {
        println!(
            "**不可直接交付**：{} 个视频帧没有位姿。明细见 {}",
            report.rejected.len(),
            report_path.display()
        );
    }
    Ok(report.is_deliverable())
}

/// ETS2：扇区文件名 → 地图边界。
///
/// 输入是 `scs_extractor` 解出 `base_map.scs` 之后 `map/europe/` 下的文件清单
/// （每行一个名字，`sec+0001+0002.base` 这种）。转换本身是纯算术，
/// 所以解包那一步不在本工具里——它需要游戏本体。
#[cfg(feature = "ets2")]
fn cmd_ets2_map_bounds(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::map_bounds::SectorGrid;

    let path = PathBuf::from(require(m, "sectors")?);
    let text = fs::read_to_string(&path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if names.is_empty() {
        return Err(format!("{} 里没有任何扇区名", path.display()));
    }
    let grid = SectorGrid::from_sector_names(names.iter().copied())
        .map_err(|error| format!("扇区名解析失败: {error:?}"))?;
    let bounds = grid.to_delivery_bounds_m();
    let (extent_x, extent_z) = grid.sector_extent();
    let (size_x, size_z) = bounds.size();
    let report = serde_json::json!({
        "schema": "ets2-map-bounds/v1",
        "source": path.display().to_string(),
        "sector_files": names.len(),
        "sector_extent": [extent_x, extent_z],
        // 68 号 §6.1 第 1 条：地图比例 + 边界四个坐标。
        "metric_scale": 1.0,
        "corners_xz_m": bounds.corners_xz(),
        "size_m": [size_x, size_z],
        "confidence": "unpacked_sector_filenames",
        // 「基础地图」的定义还没跟客户对齐（98 号 §5.7.1 阻塞项 ②）：
        // base 与 base+DLC 的扇区集合不同，边界跟着变。这一格必须随数字带出。
        "sector_set_note": "以本次解包的扇区集合为准；base 与 base+DLC 的边界不同"
    });
    fs::write(
        PathBuf::from(require(m, "out")?),
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写边界失败: {e}"))?;
    println!(
        "{} 个扇区 → {:.0} m × {:.0} m，四角 {:?}",
        names.len(),
        size_x,
        size_z,
        bounds.corners_xz()
    );
    Ok(true)
}

#[cfg(feature = "client-orchestration")]
fn cmd_assemble_action(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let qpc_frequency_hz = require(m, "qpc-frequency")?
        .parse::<u64>()
        .map_err(|error| format!("--qpc-frequency is not an integer: {error}"))?;
    let rows = depth_lab::final_action_io::assemble_pose_index(
        &PathBuf::from(require(m, "depth")?),
        &PathBuf::from(require(m, "camera")?),
        &PathBuf::from(require(m, "player")?),
        &PathBuf::from(require(m, "actions")?),
        &PathBuf::from(require(m, "pairs")?),
        &PathBuf::from(require(m, "wallclock")?),
        &PathBuf::from(require(m, "mouse")?),
        qpc_frequency_hz,
        &PathBuf::from(require(m, "out")?),
    )?;
    println!("assembled {rows} strict final pose rows");
    Ok(true)
}

#[cfg(feature = "client-orchestration")]
fn cmd_assemble_action_30(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let qpc_frequency_hz = require(m, "qpc-frequency")?
        .parse::<u64>()
        .map_err(|error| format!("--qpc-frequency is not an integer: {error}"))?;
    let rows = depth_lab::final_action_io::assemble_pose_index_full_rate(
        &PathBuf::from(require(m, "camera")?),
        &PathBuf::from(require(m, "player")?),
        &PathBuf::from(require(m, "actions")?),
        &PathBuf::from(require(m, "pairs")?),
        &PathBuf::from(require(m, "wallclock")?),
        &PathBuf::from(require(m, "mouse")?),
        qpc_frequency_hz,
        &PathBuf::from(require(m, "out")?),
    )?;
    println!("assembled {rows} strict full-rate final pose rows");
    Ok(true)
}

/// 不带取值的开关。**必须列举而不是靠「下一个 token 是不是 `--` 开头」推断**：
/// 后者会把 `--out --dry-run` 这种漏填取值的命令悄悄读成「out 是个开关」，
/// 然后在别处以「缺少 --out」报错，排障方向就错了。
const VALUELESS_FLAGS: &[&str] = &["dry-run", "allow-nondeliverable"];

fn parse_args(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let k = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("无法识别的参数 `{}`", args[i]))?;
        if VALUELESS_FLAGS.contains(&k) {
            map.insert(k.to_string(), "1".to_string());
            i += 1;
            continue;
        }
        let v = args
            .get(i + 1)
            .ok_or_else(|| format!("`--{k}` 缺少取值"))?
            .clone();
        map.insert(k.to_string(), v);
        i += 2;
    }
    Ok(map)
}

fn cmd_dsv_discovery_validate(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let path = depth_lab::materialize_validated_report(
        Path::new(require(m, "input")?),
        Path::new(require(m, "evidence-root")?),
        Path::new(require(m, "out")?),
    )?;
    println!(
        "validated discovery-only D3D11 DSV evidence: {}",
        path.display()
    );
    Ok(true)
}

#[cfg(feature = "evidence-authoring")]
fn cmd_dsv_discovery_pass_summary(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let path = depth_lab::materialize_dsv_candidate_pass_summary(
        Path::new(require(m, "input")?),
        Path::new(require(m, "evidence-root")?),
        Path::new(require(m, "out")?),
        require(m, "candidate")?,
    )?;
    println!(
        "materialized review-only D3D11 DSV pass histogram: {}",
        path.display()
    );
    Ok(true)
}

#[cfg(feature = "evidence-authoring")]
fn cmd_dsv_selection_attest(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let path = depth_lab::materialize_dsv_selection_attestation(
        Path::new(require(m, "evidence-root")?),
        [
            depth_lab::DsvSelectionRunInput {
                run_directory: Path::new(require(m, "run-a")?),
                selected_candidate_id: require(m, "candidate-a")?,
            },
            depth_lab::DsvSelectionRunInput {
                run_directory: Path::new(require(m, "run-b")?),
                selected_candidate_id: require(m, "candidate-b")?,
            },
        ],
        Path::new(require(m, "out")?),
    )?;
    println!(
        "materialized bridge-only D3D11 DSV selection attestation: {}",
        path.display()
    );
    Ok(true)
}

#[cfg(feature = "selection-validation")]
fn cmd_dsv_selection_validate(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let evidence_root = PathBuf::from(require(m, "evidence-root")?);
    let input_argument = PathBuf::from(require(m, "input")?);
    let input = if input_argument.is_absolute() {
        input_argument
    } else {
        evidence_root.join(input_argument)
    };
    let expected_locator = require(m, "expected-locator")?;
    let expected_sha256 = require(m, "expected-sha256")?;
    let expected_executable_sha256 = require(m, "expected-executable-sha256")?;
    if !depth_lab::camera_binding::is_sha256_digest(expected_executable_sha256) {
        return Err("expected executable hash must be canonical lowercase sha256:<64 hex>".into());
    }
    let validated = depth_lab::validate_dsv_selection_attestation_with_sources(
        &evidence_root,
        &input,
        expected_sha256,
    )?;
    if validated.locator != *expected_locator
        || validated.exact_bytes_sha256 != *expected_sha256
        || validated.target.executable.sha256 != *expected_executable_sha256
    {
        return Err(
            "replayed DSV selection locator/hash/executable differs from the explicit expectation"
                .into(),
        );
    }
    let output = serde_json::json!({
        "schema": "d3d11-dsv-selection-validation/v1",
        "status": "pass",
        "locator": validated.locator,
        "exactBytesSha256": validated.exact_bytes_sha256,
        "target": validated.target,
        "selectedDescriptor": validated.selected_descriptor,
        "capabilityCeiling": "dsv_selection_bridge_only",
        "publicationAllowed": false,
        "recordingReady": false,
    });
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| format!("serialize DSV selection validation failed: {error}"))?
    );
    Ok(true)
}

#[cfg(feature = "selection-validation")]
fn validate_camera_discovery_config_contract(
    config: &depth_lab::probe_config::ProbeConfig,
) -> Result<
    (
        &depth_lab::probe_config::CameraDiscoveryConfig,
        &depth_lab::probe_config::CameraDiscoveryDsvSelection,
    ),
    String,
> {
    if !config.enabled || !config.camera_probe {
        return Err("probe config is not an enabled camera discovery plan".into());
    }
    let discovery = config
        .camera_discovery
        .as_ref()
        .ok_or("probe config has no camera discovery config")?;
    if discovery.schema != depth_lab::CAMERA_DISCOVERY_SCHEMA_V3 {
        return Err("camera discovery config validation requires config/v3".into());
    }
    let selection = discovery
        .dsv_selection
        .as_ref()
        .ok_or("camera discovery config/v3 has no DSV selection binding")?;
    if discovery.render.is_some()
        || config.representative_scene_attestation_sha256.is_some()
        || config.dsv_discovery.is_some()
        || config.camera_production.is_some()
        || config.depth_profile_path.is_some()
        || config.depth_profile_sha256.is_some()
    {
        return Err(
            "camera discovery config plan carries legacy render, DSV inventory, production camera, or depth-profile authority"
                .into(),
        );
    }
    if let Some(reference) = &discovery.comparison_camera_reference {
        reference
            .validate()
            .map_err(|error| format!("comparison camera reference is invalid: {error}"))?;
    }
    Ok((discovery, selection))
}

#[cfg(feature = "selection-validation")]
fn validate_exact_config_sha256(bytes: &[u8], expected_sha256: &str) -> Result<String, String> {
    if !depth_lab::camera_binding::is_sha256_digest(expected_sha256) {
        return Err("expected config hash must be canonical lowercase sha256:<64 hex>".into());
    }
    let exact_sha256 = format!("sha256:{}", sha256_hex(bytes));
    if exact_sha256 != expected_sha256 {
        return Err(format!(
            "probe config exact-bytes hash mismatch: expected={expected_sha256} actual={exact_sha256}"
        ));
    }
    Ok(exact_sha256)
}

#[cfg(feature = "selection-validation")]
fn cmd_camera_discovery_config_validate(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let input = PathBuf::from(require(m, "input")?);
    let expected_sha256 = require(m, "expected-sha256")?;
    if !depth_lab::camera_binding::is_sha256_digest(expected_sha256) {
        return Err("expected config hash must be canonical lowercase sha256:<64 hex>".into());
    }
    let (config, config_bytes) = depth_lab::probe_config::load_from_with_bytes(&input)
        .map_err(|error| format!("load probe config failed: {error}"))?;
    let exact_config_sha256 = validate_exact_config_sha256(&config_bytes, expected_sha256)?;
    let (discovery, selection) = validate_camera_discovery_config_contract(&config)?;
    let evidence_root = Path::new(&selection.evidence_root);
    let validated_selection = depth_lab::validate_dsv_selection_attestation_with_sources(
        evidence_root,
        &evidence_root.join(&selection.attestation_locator),
        &selection.attestation_sha256,
    )?;
    if validated_selection.locator != selection.attestation_locator
        || validated_selection.exact_bytes_sha256 != selection.attestation_sha256
        || validated_selection.target.executable.sha256 != discovery.executable_sha256
    {
        return Err(
            "camera discovery config differs from its replayed selection locator/hash/executable"
                .into(),
        );
    }
    let output = serde_json::json!({
        "schema": "camera-discovery-config-validation/v1",
        "status": "pass",
        "exactConfigSha256": exact_config_sha256,
        "phaseId": discovery.phase_id,
        "executableSha256": discovery.executable_sha256,
        "dsvSelection": selection,
        "selectedDsvDescriptor": validated_selection.selected_descriptor,
        "comparisonCameraReferencePresent": discovery.comparison_camera_reference.is_some(),
        "capabilityCeiling": "camera_discovery_only",
        "automaticSelectionAuthorized": false,
        "productionBindingAuthorized": false,
        "publicationAllowed": false,
        "recordingReady": false,
    });
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| format!("serialize camera config validation failed: {error}"))?
    );
    Ok(true)
}

#[cfg(all(test, feature = "selection-validation"))]
mod camera_discovery_config_validation_cli_tests {
    use super::*;
    use depth_lab::camera_binding::GameCameraBinding;
    use depth_lab::probe_config::{
        CAMERA_DISCOVERY_SCHEMA_V2, CAMERA_DISCOVERY_SCHEMA_V3, CameraDiscoveryConfig,
        CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity, DiscoveryDepthFormat,
        ProbeConfig,
    };

    fn comparison_reference() -> GameCameraBinding {
        GameCameraBinding {
            provider: "exact-build-camera-reference/v1".into(),
            camera_singleton_relocation: 1,
            camera_root_offset: 2,
            world_transform_offset: 3,
            metres_per_game_unit: "0.01".into(),
            axis_mapping: "x_z_y".into(),
        }
    }

    fn v3_config(tag: &str) -> ProbeConfig {
        let root = std::env::temp_dir().join(format!(
            "dm-camera-config-validation-{tag}-{}",
            std::process::id()
        ));
        ProbeConfig {
            enabled: true,
            output_dir: root.join("output").to_string_lossy().into_owned(),
            unbind_from_end: 4,
            max_frames: None,
            present_only: false,
            watermark_only: false,
            writer_bypass: false,
            target_depth_hz: None,
            lane_fps: None,
            camera_probe: true,
            camera_discovery: Some(CameraDiscoveryConfig {
                schema: CAMERA_DISCOVERY_SCHEMA_V3.into(),
                phase_id: "POSE_STATIC".into(),
                executable_sha256: format!("sha256:{}", "a".repeat(64)),
                render: None,
                dsv_selection: Some(CameraDiscoveryDsvSelection {
                    evidence_root: root.join("evidence").to_string_lossy().into_owned(),
                    attestation_locator: "selection.json".into(),
                    attestation_sha256: format!("sha256:{}", "b".repeat(64)),
                }),
                sample_every_n_presents: 1,
                max_main_passes_per_present: 4,
                draw_window_start: 0,
                draw_window_count: 8,
                comparison_camera_reference: None,
            }),
            representative_scene_attestation_sha256: None,
            dsv_discovery: None,
            camera_production: None,
            depth_profile_path: None,
            depth_profile_sha256: None,
            session_id: format!("camera-config-validation-{tag}"),
        }
    }

    fn invoke_invalid(tag: &str, config: &ProbeConfig) -> String {
        let directory = std::env::temp_dir().join(format!(
            "dm-camera-config-validation-input-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let input = directory.join("probe-config.json");
        let bytes = serde_json::to_vec(config).unwrap();
        fs::write(&input, &bytes).unwrap();
        let args = BTreeMap::from([
            ("input".into(), input.to_string_lossy().into_owned()),
            (
                "expected-sha256".into(),
                format!("sha256:{}", sha256_hex(&bytes)),
            ),
        ]);
        let error = cmd_camera_discovery_config_validate(&args).unwrap_err();
        fs::remove_dir_all(&directory).unwrap();
        error
    }

    fn write_config(tag: &str, config: &ProbeConfig) -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "dm-camera-config-hash-input-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let input = directory.join("probe-config.json");
        fs::write(&input, serde_json::to_vec(config).unwrap()).unwrap();
        (directory, input)
    }

    #[test]
    fn rejects_invalid_comparison_reference_before_selection_replay() {
        let mut config = v3_config("invalid-comparison");
        let reference = config
            .camera_discovery
            .as_mut()
            .unwrap()
            .comparison_camera_reference
            .insert(comparison_reference());
        reference.metres_per_game_unit = "NaN".into();
        assert!(
            invoke_invalid("invalid-comparison", &config)
                .contains("comparison camera reference is invalid")
        );
    }

    #[test]
    fn rejects_legacy_camera_discovery_config_before_selection_replay() {
        let mut config = v3_config("legacy");
        let discovery = config.camera_discovery.as_mut().unwrap();
        discovery.schema = CAMERA_DISCOVERY_SCHEMA_V2.into();
        discovery.render = Some(CameraDiscoveryRenderIdentity {
            width: 1920,
            height: 1080,
            sample_count: 1,
            formats: vec![DiscoveryDepthFormat::D32Float],
        });
        discovery.dsv_selection = None;
        discovery.comparison_camera_reference = Some(comparison_reference());
        assert!(invoke_invalid("legacy", &config).contains("requires config/v3"));
    }

    #[test]
    fn rejects_depth_profile_authority_before_selection_replay() {
        let mut config = v3_config("profile-authority");
        config.depth_profile_path = Some(
            std::env::temp_dir()
                .join("forbidden-depth-profile.json")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(invoke_invalid("profile-authority", &config).contains("depth-profile authority"));
    }

    #[test]
    fn config_validation_rejects_snapshot_hash_mismatch_before_selection_replay() {
        let config = v3_config("config-hash-mismatch");
        let (directory, input) = write_config("config-hash-mismatch", &config);
        let args = BTreeMap::from([
            ("input".into(), input.to_string_lossy().into_owned()),
            (
                "expected-sha256".into(),
                format!("sha256:{}", "c".repeat(64)),
            ),
        ]);
        assert!(
            cmd_camera_discovery_config_validate(&args)
                .unwrap_err()
                .contains("exact-bytes hash mismatch")
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn phase_validation_rejects_config_snapshot_hash_mismatch_before_selection_replay() {
        let config = v3_config("phase-config-hash-mismatch");
        let (directory, expected_config) = write_config("phase-config-hash-mismatch", &config);
        let args = BTreeMap::from([
            (
                "input".into(),
                directory
                    .join("report-does-not-need-to-exist.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "expected-config".into(),
                expected_config.to_string_lossy().into_owned(),
            ),
            (
                "expected-config-sha256".into(),
                format!("sha256:{}", "c".repeat(64)),
            ),
        ]);
        assert!(
            cmd_camera_discovery_phase_validate(&args)
                .unwrap_err()
                .contains("exact-bytes hash mismatch")
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}

#[cfg(feature = "selection-validation")]
fn cmd_camera_discovery_phase_validate(m: &BTreeMap<String, String>) -> Result<bool, String> {
    const MAX_CAMERA_DISCOVERY_REPORT_BYTES: u64 = 256 * 1024 * 1024;
    let input = PathBuf::from(require(m, "input")?);
    let expected_config_path = PathBuf::from(require(m, "expected-config")?);
    let expected_config_sha256 = require(m, "expected-config-sha256")?;
    if !depth_lab::camera_binding::is_sha256_digest(expected_config_sha256) {
        return Err("expected config hash must be canonical lowercase sha256:<64 hex>".into());
    }
    let (expected_config, expected_config_bytes) =
        depth_lab::probe_config::load_from_with_bytes(&expected_config_path)
            .map_err(|error| format!("load expected probe config failed: {error}"))?;
    let exact_config_sha256 =
        validate_exact_config_sha256(&expected_config_bytes, expected_config_sha256)?;
    let (expected_discovery, expected_selection) =
        validate_camera_discovery_config_contract(&expected_config)?;
    let evidence_root = Path::new(&expected_selection.evidence_root);
    let validated_selection = depth_lab::validate_dsv_selection_attestation_with_sources(
        evidence_root,
        &evidence_root.join(&expected_selection.attestation_locator),
        &expected_selection.attestation_sha256,
    )?;
    if validated_selection.locator != expected_selection.attestation_locator
        || validated_selection.exact_bytes_sha256 != expected_selection.attestation_sha256
        || validated_selection.target.executable.sha256 != expected_discovery.executable_sha256
    {
        return Err(
            "camera discovery phase config differs from its replayed selection source".into(),
        );
    }
    let metadata = fs::symlink_metadata(&input)
        .map_err(|error| format!("inspect sealed report {} failed: {error}", input.display()))?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_CAMERA_DISCOVERY_REPORT_BYTES
    {
        return Err("sealed camera discovery phase report is not a bounded regular file".into());
    }
    let report_bytes = fs::read(&input)
        .map_err(|error| format!("read sealed report {} failed: {error}", input.display()))?;
    if report_bytes.len() as u64 != metadata.len() {
        return Err("sealed camera discovery phase report changed during read".into());
    }
    let report: depth_lab::CameraDiscoveryReport = serde_json::from_slice(&report_bytes)
        .map_err(|error| format!("parse sealed camera discovery report failed: {error}"))?;
    report.validate_dsv_selection_binding()?;
    if report.schema != depth_lab::CAMERA_DISCOVERY_REPORT_SCHEMA_V4
        || report.config != *expected_discovery
        || report.phase_id != expected_discovery.phase_id
        || report.config.phase_id != expected_discovery.phase_id
        || !report.reliable
        || !report.blockers.is_empty()
        || report.candidate_overflows != 0
        || report.view_observation_overflows != 0
    {
        return Err(
            "sealed camera discovery phase report is not the exact reliable blocker-free v4 report"
                .into(),
        );
    }
    match expected_discovery.comparison_camera_reference.as_ref() {
        Some(_) => {
            if report.comparison_camera_samples.len() < 8
                || !report.comparison_camera_errors.is_empty()
            {
                return Err(
                    "camera discovery phase with a comparison reference lacks clean samples".into(),
                );
            }
        }
        None => {
            if !report.comparison_camera_samples.is_empty()
                || !report.comparison_camera_errors.is_empty()
            {
                return Err(
                    "reference-free camera discovery phase carries comparison samples or errors"
                        .into(),
                );
            }
        }
    }
    let dsv_selection = report
        .dsv_selection
        .clone()
        .ok_or("validated v3 phase report lost its DSV selection binding")?;
    let selected_dsv_descriptor = report
        .selected_dsv_descriptor
        .ok_or("validated v3 phase report lost its selected DSV descriptor")?;
    if selected_dsv_descriptor != validated_selection.selected_descriptor {
        return Err(
            "sealed camera discovery phase descriptor differs from strict selection replay".into(),
        );
    }
    let output = serde_json::json!({
        "schema": "camera-discovery-phase-validation/v1",
        "status": "pass",
        "exactConfigSha256": exact_config_sha256,
        "phaseId": report.phase_id,
        "dsvSelection": dsv_selection,
        "selectedDsvDescriptor": validated_selection.selected_descriptor,
        "comparisonCameraReference": report.config.comparison_camera_reference,
        "capabilityCeiling": "camera_discovery_only",
        "publicationAllowed": false,
        "recordingReady": false,
    });
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| format!("serialize camera phase validation failed: {error}"))?
    );
    Ok(true)
}

/// 读一个 JSONL。**逐行报错并带行号**——一份几十万行的文件里，
/// 「第 N 行解析失败」和「解析失败」的排障成本差一个数量级。
fn read_jsonl<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<Vec<T>, String> {
    // zstd 挂在 client-orchestration 之下（probe DLL 不该背 C 库，见该 feature 的
    // 注释）。CLI 未启用它时**明确拒绝**，不静默把 .zst 当纯文本读——那会得到一堆
    // 乱码的「第 N 行解析失败」，把真因埋掉。
    let text = if path.extension().is_some_and(|extension| extension == "zst") {
        #[cfg(not(feature = "client-orchestration"))]
        {
            return Err(format!(
                "{} 是 zstd 压缩的，但本次构建未启用 client-orchestration feature；\
                 请用 --features client-orchestration 重新构建 depth-lab",
                path.display()
            ));
        }
        #[cfg(feature = "client-orchestration")]
        {
            let file =
                fs::File::open(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
            let mut text = String::new();
            zstd::stream::read::Decoder::new(file)
                .map_err(|e| format!("打开不了 zstd 流 {}: {e}", path.display()))?
                .read_to_string(&mut text)
                .map_err(|e| format!("解压不了 {}: {e}", path.display()))?;
            text
        }
    } else {
        fs::read_to_string(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?
    };
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line)
                .map_err(|e| format!("{}:{} 解析失败: {e}", path.display(), n + 1))?,
        );
    }
    Ok(out)
}

fn read_jsonl_set<T, F>(path: &PathBuf, accept: F) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned,
    F: Fn(&PathBuf) -> bool,
{
    if path.is_file() {
        return read_jsonl(path);
    }
    if !path.is_dir() {
        return Err(format!("输入不存在或不是文件/目录: {}", path.display()));
    }

    let mut pending = vec![path.clone()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("读不了目录 {}: {error}", directory.display()))?
        {
            let entry =
                entry.map_err(|error| format!("枚举目录 {} 失败: {error}", directory.display()))?;
            let file_type = entry.file_type().map_err(|error| {
                format!("读取文件类型 {} 失败: {error}", entry.path().display())
            })?;
            let entry_path = entry.path();
            if file_type.is_dir() {
                pending.push(entry_path);
            } else if file_type.is_file() && accept(&entry_path) {
                files.push(entry_path);
            }
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("目录 {} 中没有匹配的 JSONL 输入", path.display()));
    }

    let mut rows = Vec::new();
    for file in files {
        rows.extend(read_jsonl(&file)?);
    }
    Ok(rows)
}

fn read_recorder_frames(path: &PathBuf) -> Result<Vec<RecorderFrameRow>, String> {
    read_jsonl_set(path, |candidate| {
        candidate
            .file_name()
            .is_some_and(|name| name == "frames.jsonl" || name == "frames.jsonl.zst")
    })
}

fn read_watermark_samples(path: &PathBuf) -> Result<Vec<WatermarkSample>, String> {
    read_jsonl_set(path, |candidate| {
        candidate.file_name().is_some_and(|name| {
            name.to_string_lossy().starts_with("watermark-samples-")
                && candidate
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
        })
    })
}

fn collect_files<F>(root: &Path, accept: F) -> Result<Vec<PathBuf>, String>
where
    F: Fn(&Path) -> bool,
{
    if !root.is_dir() {
        return Err(format!("输入不是目录: {}", root.display()));
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("读不了目录 {}: {error}", directory.display()))?
        {
            let entry =
                entry.map_err(|error| format!("枚举目录 {} 失败: {error}", directory.display()))?;
            let kind = entry.file_type().map_err(|error| {
                format!("读取文件类型 {} 失败: {error}", entry.path().display())
            })?;
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && accept(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoDescriptor {
    movie_timescale: u32,
    track_timescale: u32,
    language: String,
    width: u16,
    height: u16,
    sequence_parameter_set: Vec<u8>,
    picture_parameter_set: Vec<u8>,
}

fn video_descriptor(
    reader: &mp4::Mp4Reader<BufReader<fs::File>>,
    path: &Path,
) -> Result<(u32, u32, VideoDescriptor), String> {
    if reader.is_fragmented() {
        return Err(format!("不支持分片 MP4: {}", path.display()));
    }
    if reader.tracks().len() != 1 {
        return Err(format!("MP4 必须只有一条视频轨: {}", path.display()));
    }
    let track = reader
        .tracks()
        .values()
        .next()
        .expect("track count checked");
    if track
        .track_type()
        .map_err(|error| format!("读取视频轨类型失败 {}: {error}", path.display()))?
        != mp4::TrackType::Video
    {
        return Err(format!("MP4 轨道不是视频: {}", path.display()));
    }
    let descriptor = VideoDescriptor {
        movie_timescale: reader.timescale(),
        track_timescale: track.timescale(),
        language: track.language().to_owned(),
        width: track.width(),
        height: track.height(),
        sequence_parameter_set: track
            .sequence_parameter_set()
            .map_err(|error| format!("读取 SPS 失败 {}: {error}", path.display()))?
            .to_vec(),
        picture_parameter_set: track
            .picture_parameter_set()
            .map_err(|error| format!("读取 PPS 失败 {}: {error}", path.display()))?
            .to_vec(),
    };
    Ok((track.track_id(), track.sample_count(), descriptor))
}

fn cmd_video_concat(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Reader, Mp4Writer, TrackConfig, TrackType};

    let segments = PathBuf::from(require(m, "segments")?);
    let out = PathBuf::from(require(m, "out")?);
    let inputs = collect_files(&segments, |path| {
        path.file_name().is_some_and(|name| name == "video.mp4")
    })?;
    if inputs.is_empty() {
        return Err(format!("目录 {} 中没有 video.mp4", segments.display()));
    }
    let selected_frames = m
        .get("plan")
        .map(|plan_path| {
            let text = fs::read_to_string(plan_path)
                .map_err(|error| format!("读不了 clip plan {plan_path}: {error}"))?;
            let plan: depth_lab::clip_plan::ClipPlan = serde_json::from_str(&text)
                .map_err(|error| format!("解析 clip plan {plan_path} 失败: {error}"))?;
            if plan.schema != depth_lab::clip_plan::CLIP_PLAN_SCHEMA_V1
                || !plan.met_target
                || plan.achieved_frames != plan.selected.len()
                || plan.realign_drops != 0
            {
                return Err("clip plan 未达到可交付状态".to_string());
            }
            if plan
                .selected
                .windows(2)
                .any(|window| window[0] >= window[1])
            {
                return Err("clip plan selected 必须严格递增".to_string());
            }
            Ok(plan.selected)
        })
        .transpose()?;

    let first_file = fs::File::open(&inputs[0])
        .map_err(|error| format!("打开 {} 失败: {error}", inputs[0].display()))?;
    let first_size = first_file
        .metadata()
        .map_err(|error| format!("读取 {} 元数据失败: {error}", inputs[0].display()))?
        .len();
    let first_reader = Mp4Reader::read_header(BufReader::new(first_file), first_size)
        .map_err(|error| format!("解析 {} 失败: {error}", inputs[0].display()))?;
    let (_, _, expected) = video_descriptor(&first_reader, &inputs[0])?;
    drop(first_reader);

    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
    }
    let output = BufWriter::new(
        fs::File::create(&out).map_err(|error| format!("创建 {} 失败: {error}", out.display()))?,
    );
    let config = Mp4Config {
        major_brand: "isom"
            .parse()
            .map_err(|error| format!("MP4 brand 无效: {error}"))?,
        minor_version: 512,
        compatible_brands: ["isom", "iso2", "avc1", "mp41"]
            .into_iter()
            .map(|brand| {
                brand
                    .parse()
                    .map_err(|error| format!("MP4 brand 无效: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        timescale: expected.movie_timescale,
    };
    let mut writer = Mp4Writer::write_start(output, &config)
        .map_err(|error| format!("初始化 {} 失败: {error}", out.display()))?;
    writer
        .add_track(&TrackConfig {
            track_type: TrackType::Video,
            timescale: expected.track_timescale,
            language: expected.language.clone(),
            media_conf: MediaConfig::AvcConfig(AvcConfig {
                width: expected.width,
                height: expected.height,
                seq_param_set: expected.sequence_parameter_set.clone(),
                pic_param_set: expected.picture_parameter_set.clone(),
            }),
        })
        .map_err(|error| format!("创建输出视频轨失败: {error}"))?;

    let mut total_samples = 0_u64;
    let mut source_samples = 0_u64;
    let mut next_start_time = 0_u64;
    for input in &inputs {
        let file = fs::File::open(input)
            .map_err(|error| format!("打开 {} 失败: {error}", input.display()))?;
        let size = file
            .metadata()
            .map_err(|error| format!("读取 {} 元数据失败: {error}", input.display()))?
            .len();
        let mut reader = Mp4Reader::read_header(BufReader::new(file), size)
            .map_err(|error| format!("解析 {} 失败: {error}", input.display()))?;
        let (track_id, sample_count, descriptor) = video_descriptor(&reader, input)?;
        if descriptor != expected {
            return Err(format!("视频编码参数与首段不一致: {}", input.display()));
        }
        for sample_id in 1..=sample_count {
            let mut sample = reader
                .read_sample(track_id, sample_id)
                .map_err(|error| {
                    format!("读取 {} 第 {sample_id} 帧失败: {error}", input.display())
                })?
                .ok_or_else(|| format!("{} 缺少第 {sample_id} 帧", input.display()))?;
            let source_frame = source_samples;
            source_samples += 1;
            if selected_frames
                .as_ref()
                .is_some_and(|selected| selected.binary_search(&source_frame).is_err())
            {
                continue;
            }
            if sample.duration == 0 || sample.rendering_offset != 0 {
                return Err(format!(
                    "{} 第 {sample_id} 帧时间戳不符合 CFR/no-B-frame 契约",
                    input.display()
                ));
            }
            sample.start_time = next_start_time;
            next_start_time = next_start_time
                .checked_add(u64::from(sample.duration))
                .ok_or_else(|| "输出视频时间轴溢出".to_string())?;
            writer
                .write_sample(1, &sample)
                .map_err(|error| format!("写第 {total_samples} 帧失败: {error}"))?;
            total_samples += 1;
        }
    }
    if let Some(selected) = &selected_frames
        && total_samples != selected.len() as u64
    {
        return Err(format!(
            "clip plan 选中 {} 帧，但输入视频只找到 {total_samples} 帧（源视频共 {source_samples} 帧）",
            selected.len()
        ));
    }
    writer
        .write_end()
        .map_err(|error| format!("完成 {} 失败: {error}", out.display()))?;
    let mut output = writer.into_writer();
    output
        .flush()
        .map_err(|error| format!("刷新 {} 失败: {error}", out.display()))?;
    output
        .get_ref()
        .sync_all()
        .map_err(|error| format!("同步 {} 失败: {error}", out.display()))?;
    println!(
        "拼接 {} 段视频，共 {} 帧：{}",
        inputs.len(),
        total_samples,
        out.display()
    );
    Ok(true)
}

fn json_f64(value: &serde_json::Value, pointer: &str, source: &Path) -> Result<f64, String> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| format!("{} 缺少数值字段 {pointer}", source.display()))
}

fn ini_value(text: &str, key: &str) -> Result<f64, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(candidate, value)| {
            candidate
                .trim()
                .eq_ignore_ascii_case(key)
                .then(|| value.trim().parse::<f64>())
        })
        .ok_or_else(|| format!("INI 缺少 {key}"))?
        .map_err(|error| format!("INI 的 {key} 不是数值: {error}"))
}

fn cmd_prepare_fallout4_action_context(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::camera_action::{ActionFrameRow, MouseNormalization};

    let frames_path = PathBuf::from(require(m, "frames")?);
    let session_path = PathBuf::from(require(m, "session")?);
    let calibration_path = PathBuf::from(require(m, "calibration")?);
    let axis_ini_path = PathBuf::from(require(m, "axis-ini")?);
    let wallclock_out = PathBuf::from(require(m, "wallclock-out")?);
    let mouse_out = PathBuf::from(require(m, "mouse-out")?);

    let mut frames: Vec<ActionFrameRow> = read_jsonl_set(&frames_path, |candidate| {
        candidate
            .file_name()
            .is_some_and(|name| name == "frames.jsonl" || name == "frames.jsonl.zst")
    })?;
    frames.sort_by_key(|row| row.session_frame);
    for (index, frame) in frames.iter().enumerate() {
        if frame.session_frame != index as u64 {
            return Err(format!(
                "frames 的 session_frame 不连续：第 {index} 行是 {}",
                frame.session_frame
            ));
        }
    }

    let session_bytes = fs::read(&session_path)
        .map_err(|error| format!("读取 {} 失败: {error}", session_path.display()))?;
    let session: serde_json::Value = serde_json::from_slice(&session_bytes)
        .map_err(|error| format!("解析 {} 失败: {error}", session_path.display()))?;
    let session_id = session
        .pointer("/session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} 缺少 session_id", session_path.display()))?;
    let game_id = session
        .pointer("/game/id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} 缺少 game.id", session_path.display()))?;
    if frames.first().is_none_or(|row| row.run_id != "r0000") {
        return Err("当前动作上下文只允许从与会话起点同源的 r0000 生成".into());
    }
    let start_unix_ms: i128 = session_id
        .split('-')
        .nth(1)
        .ok_or_else(|| format!("session_id 不能解析开始时间: {session_id}"))?
        .parse::<i64>()
        .map(i128::from)
        .map_err(|error| format!("session_id 不能解析开始时间: {error}"))?;

    let calibration_bytes = fs::read(&calibration_path)
        .map_err(|error| format!("读取 {} 失败: {error}", calibration_path.display()))?;
    let calibration: serde_json::Value = serde_json::from_slice(&calibration_bytes)
        .map_err(|error| format!("解析 {} 失败: {error}", calibration_path.display()))?;
    let calibration_id = calibration
        .pointer("/calibration_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} 缺少 calibration_id", calibration_path.display()))?;
    if calibration
        .pointer("/verdict")
        .and_then(serde_json::Value::as_str)
        != Some("pass")
    {
        return Err("输入标定没有 pass，拒绝生成鼠标归一化".into());
    }
    if calibration
        .pointer("/game_id")
        .and_then(serde_json::Value::as_str)
        != Some(game_id)
    {
        return Err("输入标定的 game_id 与会话不一致".into());
    }
    let counts_per_unit = json_f64(
        &calibration,
        "/derived/counts_per_360_measured",
        &calibration_path,
    )?;
    let delta_counts_full_scale = json_f64(&session, "/contract/pointer_ref", &session_path)?;
    if !counts_per_unit.is_finite()
        || counts_per_unit <= 0.0
        || !delta_counts_full_scale.is_finite()
        || delta_counts_full_scale <= 0.0
    {
        return Err("标定 counts_per_360 或会话 pointer_ref 无效".into());
    }

    let axis_ini_bytes = fs::read(&axis_ini_path)
        .map_err(|error| format!("读取 {} 失败: {error}", axis_ini_path.display()))?;
    let axis_ini = std::str::from_utf8(&axis_ini_bytes)
        .map_err(|error| format!("{} 不是 UTF-8: {error}", axis_ini_path.display()))?;
    let axis_x = ini_value(axis_ini, "fMouseHeadingXScale")?;
    let axis_y = ini_value(axis_ini, "fMouseHeadingYScale")?;
    if !axis_x.is_finite()
        || axis_x <= 0.0
        || !axis_y.is_finite()
        || axis_y <= 0.0
        || (axis_x - axis_y).abs() > 1e-9
    {
        return Err(format!(
            "Fallout 4 X/Y 鼠标轴缩放不相等或无效: {axis_x} / {axis_y}"
        ));
    }

    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]",
    )
    .map_err(|error| format!("内部 wall-clock 格式无效: {error}"))?;
    let start = time::OffsetDateTime::from_unix_timestamp_nanos(start_unix_ms * 1_000_000)
        .map_err(|error| format!("session 开始时间无效: {error}"))?;
    if let Some(parent) = wallclock_out.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
    }
    let mut wallclock = BufWriter::new(
        fs::File::create(&wallclock_out)
            .map_err(|error| format!("创建 {} 失败: {error}", wallclock_out.display()))?,
    );
    for frame in &frames {
        let offset =
            i64::try_from(frame.video_t_us).map_err(|_| "video_t_us 超出时间库范围".to_string())?;
        let time = (start + time::Duration::microseconds(offset))
            .format(&format)
            .map_err(|error| format!("格式化 wall-clock 失败: {error}"))?;
        serde_json::to_writer(
            &mut wallclock,
            &serde_json::json!({"session_frame": frame.session_frame, "time": time}),
        )
        .map_err(|error| format!("序列化 wall-clock 失败: {error}"))?;
        wallclock
            .write_all(b"\n")
            .map_err(|error| format!("写 {} 失败: {error}", wallclock_out.display()))?;
    }
    wallclock
        .flush()
        .map_err(|error| format!("刷新 {} 失败: {error}", wallclock_out.display()))?;

    let mouse = MouseNormalization {
        evidence_id: format!(
            "dm-input-calibration-2:{calibration_id};fallout4-axis-ini-sha256:{}",
            sha256_hex(&axis_ini_bytes)
        ),
        initial_x: 0.5,
        initial_y: 0.5,
        horizontal_counts_per_unit: counts_per_unit,
        vertical_counts_per_unit: counts_per_unit,
        delta_counts_full_scale,
    };
    if let Some(parent) = mouse_out.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
    }
    let mut mouse_file = BufWriter::new(
        fs::File::create(&mouse_out)
            .map_err(|error| format!("创建 {} 失败: {error}", mouse_out.display()))?,
    );
    serde_json::to_writer_pretty(&mut mouse_file, &mouse)
        .map_err(|error| format!("序列化鼠标归一化失败: {error}"))?;
    mouse_file
        .write_all(b"\n")
        .and_then(|()| mouse_file.flush())
        .map_err(|error| format!("写 {} 失败: {error}", mouse_out.display()))?;
    println!(
        "动作上下文已生成：{} 帧 wall-clock；标定 {}，counts/unit={}，delta full-scale={}",
        frames.len(),
        calibration_id,
        counts_per_unit,
        delta_counts_full_scale
    );
    Ok(true)
}

fn camera_analysis_mode(
    m: &BTreeMap<String, String>,
) -> Result<depth_lab::CameraEvidenceAnalysisMode, String> {
    match m
        .get("analysis-mode")
        .map(String::as_str)
        .unwrap_or("rigid-view")
    {
        "rigid-view" => Ok(depth_lab::CameraEvidenceAnalysisMode::RigidView),
        "fallout-render-hybrid" => Ok(depth_lab::CameraEvidenceAnalysisMode::FalloutRenderHybrid),
        other => Err(format!("unsupported camera --analysis-mode {other}")),
    }
}

fn write_camera_evidence_output(
    path: &Path,
    bytes: &[u8],
    mode: depth_lab::CameraEvidenceAnalysisMode,
) -> Result<(), String> {
    if mode == depth_lab::CameraEvidenceAnalysisMode::RigidView {
        return fs::write(path, bytes)
            .map_err(|error| format!("write {} failed: {error}", path.display()));
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "create-new hybrid evidence {} failed: {error}",
                path.display()
            )
        })?;
    output
        .write_all(bytes)
        .and_then(|()| output.sync_all())
        .map_err(|error| format!("write hybrid evidence {} failed: {error}", path.display()))
}

#[cfg(test)]
mod camera_analysis_cli_tests {
    use super::{camera_analysis_mode, write_camera_evidence_output};
    use depth_lab::CameraEvidenceAnalysisMode;
    use std::collections::BTreeMap;

    #[test]
    fn hybrid_analysis_is_explicit_and_unknown_modes_are_rejected() {
        let mut args = BTreeMap::new();
        assert_eq!(
            camera_analysis_mode(&args).unwrap(),
            CameraEvidenceAnalysisMode::RigidView
        );
        args.insert("analysis-mode".into(), "fallout-render-hybrid".into());
        assert_eq!(
            camera_analysis_mode(&args).unwrap(),
            CameraEvidenceAnalysisMode::FalloutRenderHybrid
        );
        args.insert("analysis-mode".into(), "fallout".into());
        assert!(
            camera_analysis_mode(&args)
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn hybrid_evidence_never_overwrites_an_existing_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("evidence.json");
        let mode = CameraEvidenceAnalysisMode::FalloutRenderHybrid;
        write_camera_evidence_output(&path, b"original evidence", mode).unwrap();
        assert!(write_camera_evidence_output(&path, b"replacement", mode).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"original evidence");
    }

    #[test]
    fn legacy_evidence_output_keeps_its_existing_overwrite_behavior() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.json");
        let mode = CameraEvidenceAnalysisMode::RigidView;
        write_camera_evidence_output(&path, b"old", mode).unwrap();
        write_camera_evidence_output(&path, b"new", mode).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"new");
    }
}

fn cmd_camera_evidence(m: &BTreeMap<String, String>) -> Result<bool, String> {
    fn read_report(path: &Path) -> Result<depth_lab::CameraDiscoveryReport, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("读 {} 失败: {error}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("解析 {} 失败: {error}", path.display()))
    }

    let static_path = PathBuf::from(require(m, "static")?);
    let walk_path = PathBuf::from(require(m, "walk")?);
    let turn_path = PathBuf::from(require(m, "turn")?);
    let out_path = PathBuf::from(require(m, "out")?);
    let static_report = read_report(&static_path)?;
    let walk_report = read_report(&walk_path)?;
    let turn_report = read_report(&turn_path)?;
    let mode = camera_analysis_mode(m)?;
    let references = [
        static_report.config.comparison_camera_reference.is_some(),
        walk_report.config.comparison_camera_reference.is_some(),
        turn_report.config.comparison_camera_reference.is_some(),
    ];
    match references {
        [true, true, true] => {
            // Preserve the v2 report bytes and exit behavior for the existing referenced path.
            let report = depth_lab::analyze_camera_evidence_with_mode(
                &static_report,
                &walk_report,
                &turn_report,
                [
                    &static_report.phase_id,
                    &walk_report.phase_id,
                    &turn_report.phase_id,
                ],
                mode,
            )?;
            let selected = matches!(
                report.selection,
                depth_lab::CameraEvidenceSelection::Selected { .. }
            );
            let bytes = serde_json::to_vec_pretty(&report)
                .map_err(|error| format!("序列化相机取证报告失败: {error}"))?;
            write_camera_evidence_output(&out_path, &bytes, mode)?;
            println!(
                "相机取证候选={}，selection={:?}；仍需 E-geo，未授权 production binding。报告：{}",
                report.candidates.len(),
                report.selection,
                out_path.display()
            );
            Ok(selected)
        }
        [false, false, false] => {
            if mode != depth_lab::CameraEvidenceAnalysisMode::RigidView {
                return Err(
                    "Fallout hybrid analysis cannot run without an exact-build reference".into(),
                );
            }
            let report = depth_lab::analyze_reference_free_camera_candidate_review(
                &static_report,
                &walk_report,
                &turn_report,
            )?;
            let bytes = serde_json::to_vec_pretty(&report).map_err(|error| {
                format!("serialize reference-free camera review failed: {error}")
            })?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out_path)
                .map_err(|error| {
                    format!(
                        "create-new reference-free camera review {} failed: {error}",
                        out_path.display()
                    )
                })?;
            output
                .write_all(&bytes)
                .and_then(|()| output.sync_all())
                .map_err(|error| {
                    format!(
                        "write reference-free camera review {} failed: {error}",
                        out_path.display()
                    )
                })?;
            println!(
                "reference-free camera candidates={}; human review, metric validation, and E-geo remain required; no source selected; bridge-only exit=1; report={}",
                report.candidates.len(),
                out_path.display()
            );
            Ok(false)
        }
        _ => {
            Err("camera evidence reports mix comparison-reference and reference-free modes".into())
        }
    }
}

fn collect_camera_evidence_phase(
    root: &Path,
    phase: &str,
) -> Result<BTreeMap<u32, PathBuf>, String> {
    let prefix = format!("camera-candidates-{phase}_W");
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root)
        .map_err(|error| format!("read directory {} failed: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| {
            format!("read directory entry in {} failed: {error}", root.display())
        })?;
        if !entry
            .file_type()
            .map_err(|error| format!("stat {} failed: {error}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(raw_start) = name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let start = raw_start.parse::<u32>().map_err(|error| {
            format!(
                "invalid draw-window suffix in {}: {error}",
                entry.path().display()
            )
        })?;
        if raw_start != format!("{start:03}") {
            return Err(format!(
                "draw-window suffix must be zero-padded to three digits: {}",
                entry.path().display()
            ));
        }
        if files.insert(start, entry.path()).is_some() {
            return Err(format!(
                "duplicate {phase} report for draw window start={start}"
            ));
        }
    }
    Ok(files)
}

fn cmd_camera_evidence_sweep(m: &BTreeMap<String, String>) -> Result<bool, String> {
    fn read_report(path: &Path) -> Result<depth_lab::CameraDiscoveryReport, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("read {} failed: {error}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {} failed: {error}", path.display()))
    }

    let root = PathBuf::from(require(m, "root")?);
    let out_path = PathBuf::from(require(m, "out")?);
    let mode = camera_analysis_mode(m)?;
    let mut static_files = collect_camera_evidence_phase(&root, "POSE_STATIC")?;
    let mut walk_files = collect_camera_evidence_phase(&root, "POSE_WALK")?;
    let mut turn_files = collect_camera_evidence_phase(&root, "POSE_TURN")?;
    for (argument, phase, files) in [
        ("static-override", "POSE_STATIC", &mut static_files),
        ("walk-override", "POSE_WALK", &mut walk_files),
        ("turn-override", "POSE_TURN", &mut turn_files),
    ] {
        let Some(path) = m.get(argument).map(PathBuf::from) else {
            continue;
        };
        let report = read_report(&path)?;
        let start = report.config.draw_window_start;
        let expected_phase = format!("{phase}_W{start:03}");
        if report.phase_id != expected_phase || report.config.phase_id != expected_phase {
            return Err(format!(
                "--{argument} identity mismatch: expected phase={expected_phase}, report={} config={}",
                report.phase_id, report.config.phase_id
            ));
        }
        files.insert(start, path);
    }
    if static_files.is_empty() {
        return Err(format!(
            "no camera-candidates-POSE_STATIC_WNNN.json reports in {}",
            root.display()
        ));
    }
    let static_starts = static_files.keys().copied().collect::<Vec<_>>();
    let walk_starts = walk_files.keys().copied().collect::<Vec<_>>();
    let turn_starts = turn_files.keys().copied().collect::<Vec<_>>();
    if static_starts != walk_starts || static_starts != turn_starts {
        return Err(format!(
            "camera evidence phase windows differ: static={static_starts:?} walk={walk_starts:?} turn={turn_starts:?}"
        ));
    }
    let mut inputs = Vec::with_capacity(static_starts.len());
    for start in static_starts {
        inputs.push(depth_lab::CameraEvidenceSweepInput {
            static_report: read_report(&static_files[&start])?,
            walk_report: read_report(&walk_files[&start])?,
            turn_report: read_report(&turn_files[&start])?,
        });
    }
    let reference_count = inputs
        .iter()
        .flat_map(|input| [&input.static_report, &input.walk_report, &input.turn_report])
        .filter(|report| report.config.comparison_camera_reference.is_some())
        .count();
    let report_count = inputs.len() * 3;
    if reference_count == report_count {
        let mut builder = depth_lab::CameraEvidenceSweepBuilder::with_analysis_mode(mode);
        for input in inputs {
            builder.push(input)?;
        }
        let report = builder.finish()?;
        let selected = matches!(
            report.selection,
            depth_lab::CameraEvidenceSelection::Selected { .. }
        );
        let bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("serialize camera evidence sweep failed: {error}"))?;
        write_camera_evidence_output(&out_path, &bytes, mode)?;
        println!(
            "camera evidence sweep windows={} selection={:?}; E-geo still required; report={}",
            report.windows.len(),
            report.selection,
            out_path.display()
        );
        Ok(selected)
    } else if reference_count == 0 {
        if mode != depth_lab::CameraEvidenceAnalysisMode::RigidView {
            return Err(
                "Fallout hybrid analysis cannot run without an exact-build reference".into(),
            );
        }
        let report = depth_lab::analyze_reference_free_camera_candidate_review_sweep(inputs)?;
        let bytes = serde_json::to_vec_pretty(&report).map_err(|error| {
            format!("serialize reference-free camera review sweep failed: {error}")
        })?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .map_err(|error| {
                format!(
                    "create-new reference-free camera review sweep {} failed: {error}",
                    out_path.display()
                )
            })?;
        output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(|error| {
                format!(
                    "write reference-free camera review sweep {} failed: {error}",
                    out_path.display()
                )
            })?;
        println!(
            "reference-free camera review sweep windows={} common_candidates={}; no source selected; bridge-only exit=1; report={}",
            report.windows.len(),
            report.common_sources.len(),
            out_path.display()
        );
        Ok(false)
    } else {
        Err("camera evidence sweep mixes comparison-reference and reference-free modes".into())
    }
}

fn cmd_camera_binding_author(m: &BTreeMap<String, String>) -> Result<bool, String> {
    fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<(T, Vec<u8>), String> {
        let bytes =
            fs::read(path).map_err(|error| format!("read {} failed: {error}", path.display()))?;
        let value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {} failed: {error}", path.display()))?;
        Ok((value, bytes))
    }
    fn artifact(path: &Path, bytes: &[u8]) -> depth_lab::AuthoringArtifact {
        depth_lab::AuthoringArtifact {
            path: path.display().to_string(),
            sha256: format!("sha256:{}", sha256_hex(bytes)),
        }
    }

    let sweep_path = PathBuf::from(require(m, "sweep")?);
    let static_path = PathBuf::from(require(m, "static")?);
    let walk_path = PathBuf::from(require(m, "walk")?);
    let turn_path = PathBuf::from(require(m, "turn")?);
    let template_path = PathBuf::from(require(m, "template")?);
    let out_path = PathBuf::from(require(m, "out")?);
    let manifest_path = PathBuf::from(require(m, "manifest")?);
    let (sweep, sweep_bytes) = read_json::<depth_lab::CameraEvidenceSweepReport>(&sweep_path)?;
    let (static_report, static_bytes) =
        read_json::<depth_lab::CameraDiscoveryReport>(&static_path)?;
    let (walk_report, walk_bytes) = read_json::<depth_lab::CameraDiscoveryReport>(&walk_path)?;
    let (turn_report, turn_bytes) = read_json::<depth_lab::CameraDiscoveryReport>(&turn_path)?;
    let (template, _) = read_json::<depth_lab::CameraBinding>(&template_path)?;
    if out_path.exists() || manifest_path.exists() || out_path == manifest_path {
        return Err(
            "authoring output paths must be distinct and new; refusing to overwrite evidence"
                .into(),
        );
    }
    let trial_reason = m.get("trial-reason").cloned();
    let candidate = if let Some(path) = m.get("trial-source") {
        if trial_reason
            .as_ref()
            .is_none_or(|reason| reason.trim().is_empty() || reason.len() > 1024)
        {
            return Err(
                "--trial-source requires a nonempty --trial-reason (at most 1024 bytes)".into(),
            );
        }
        let (source, _) = read_json::<depth_lab::ViewSourceBinding>(Path::new(path))?;
        depth_lab::author_render_camera_trial(
            &sweep,
            [&static_report, &walk_report, &turn_report],
            &template,
            source,
        )?
    } else {
        if trial_reason.is_some() {
            return Err("--trial-reason requires --trial-source".into());
        }
        depth_lab::author_render_camera_binding(
            &sweep,
            [&static_report, &walk_report, &turn_report],
            &template,
        )?
    };
    let selected_view = candidate.view_sources[0];
    let selected_projection = candidate.projection_sources[0];
    let position_source = candidate
        .game_camera
        .clone()
        .ok_or("authored render CameraBinding lost its exact-build position source")?;
    let view_metres_per_unit = position_source.metres_per_game_unit.clone();
    let candidate_bytes = serde_json::to_vec_pretty(&candidate)
        .map_err(|error| format!("serialize authored CameraBinding failed: {error}"))?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {} failed: {error}", parent.display()))?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out_path)
        .and_then(|mut file| file.write_all(&candidate_bytes))
        .map_err(|error| format!("write {} failed: {error}", out_path.display()))?;
    let manifest = depth_lab::CameraBindingAuthoringManifest {
        schema: if trial_reason.is_some() {
            depth_lab::CAMERA_BINDING_TRIAL_SCHEMA_V1
        } else {
            depth_lab::CAMERA_BINDING_AUTHORING_SCHEMA_V1
        }
        .into(),
        candidate_binding: artifact(&out_path, &candidate_bytes),
        sweep_evidence: artifact(&sweep_path, &sweep_bytes),
        phase_reports: vec![
            artifact(&static_path, &static_bytes),
            artifact(&walk_path, &walk_bytes),
            artifact(&turn_path, &turn_bytes),
        ],
        selected_view,
        selected_projection,
        position_source,
        view_metres_per_unit,
        requires_e_geo_validation: true,
        production_binding_authorized: false,
        trial_reason,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("serialize CameraBinding authoring manifest failed: {error}"))?;
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {} failed: {error}", parent.display()))?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .and_then(|mut file| file.write_all(&manifest_bytes))
        .map_err(|error| format!("write {} failed: {error}", manifest_path.display()))?;
    println!(
        "authored non-production CameraBinding={} manifest={}; E-geo validation is still required",
        out_path.display(),
        manifest_path.display()
    );
    Ok(true)
}

fn cmd_perf_certification_check(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let manifest_path = PathBuf::from(require(m, "manifest")?);
    let runtime_path = PathBuf::from(require(m, "runtime")?);
    let key_path = PathBuf::from(require(m, "key")?);
    let hardware_path = PathBuf::from(require(m, "hardware")?);
    let dll_sha256 = require(m, "dll-sha256")?;
    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|error| format!("read {} failed: {error}", manifest_path.display()))?;
    let runtime_bytes = fs::read(&runtime_path)
        .map_err(|error| format!("read {} failed: {error}", runtime_path.display()))?;
    let key_bytes = fs::read(&key_path)
        .map_err(|error| format!("read {} failed: {error}", key_path.display()))?;
    let hardware_bytes = fs::read(&hardware_path)
        .map_err(|error| format!("read {} failed: {error}", hardware_path.display()))?;
    let manifest: depth_lab::certification::DepthPerfCertificationManifest =
        serde_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("parse {} failed: {error}", manifest_path.display()))?;
    let runtime: depth_lab::certification::DepthProbeRuntimeManifest =
        serde_json::from_slice(&runtime_bytes)
            .map_err(|error| format!("parse {} failed: {error}", runtime_path.display()))?;
    let key: depth_lab::certification::PerfCertificationKey = serde_json::from_slice(&key_bytes)
        .map_err(|error| format!("parse {} failed: {error}", key_path.display()))?;
    let hardware: depth_lab::certification::PerfHardwareIdentity =
        serde_json::from_slice(&hardware_bytes)
            .map_err(|error| format!("parse {} failed: {error}", hardware_path.display()))?;
    if m.contains_key("device-profile-sha256") != m.contains_key("recording-profile-sha256") {
        return Err(
            "device-profile-sha256 and recording-profile-sha256 must be supplied together".into(),
        );
    }
    let certified = if let (Some(device), Some(recording)) = (
        m.get("device-profile-sha256"),
        m.get("recording-profile-sha256"),
    ) {
        depth_lab::certification::evaluate_device_profile_certification(
            &manifest,
            &runtime,
            dll_sha256,
            &key,
            &hardware,
            depth_lab::certification::DeviceProfileContext {
                device_profile_sha256: device,
                recording_profile_sha256: recording,
            },
        )
        .is_ok()
    } else {
        depth_lab::certification::evaluate_perf_certification(
            &manifest, &runtime, dll_sha256, &key, &hardware,
        )
        .is_ok()
    };
    println!(
        "performance certification {} for generation key {} and hardware {} (credential check only; local Ready not executed)",
        if certified { "PASS" } else { "MISS" },
        key_path.display(),
        hardware_path.display()
    );
    Ok(certified)
}

fn cmd_perf_certification_manifest_validate(m: &BTreeMap<String, String>) -> Result<(), String> {
    let manifest_path = PathBuf::from(require(m, "manifest")?);
    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|error| format!("read {} failed: {error}", manifest_path.display()))?;
    let manifest: depth_lab::certification::DepthPerfCertificationManifest =
        serde_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("parse {} failed: {error}", manifest_path.display()))?;
    manifest.validate()?;
    println!(
        "Depth performance certification manifest PASS: runtimes={} certifications={}",
        manifest.approved_runtimes.len(),
        manifest.certifications.len()
    );
    Ok(())
}

#[cfg(test)]
mod jsonl_input_tests {
    use super::{collect_camera_evidence_phase, read_jsonl, read_jsonl_set};
    #[cfg(feature = "client-orchestration")]
    use std::io::Write;

    #[cfg(feature = "client-orchestration")]
    #[test]
    fn reads_production_zstd_jsonl_inputs() {
        let path = std::env::temp_dir().join(format!(
            "depth-lab-jsonl-input-{}-{}.jsonl.zst",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(file, 1).unwrap();
        encoder
            .write_all(b"{\"frame\":1}\n{\"frame\":2}\n")
            .unwrap();
        encoder.finish().unwrap();

        let rows: Vec<serde_json::Value> = read_jsonl(&path).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["frame"], 1);
        assert_eq!(rows[1]["frame"], 2);

        let _ = std::fs::remove_file(path);
    }

    #[cfg(not(feature = "client-orchestration"))]
    #[test]
    fn rejects_production_zstd_jsonl_without_client_orchestration() {
        let path = std::env::temp_dir().join(format!(
            "depth-lab-jsonl-input-{}-{}.jsonl.zst",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"not decoded without the feature").unwrap();

        let error = read_jsonl::<serde_json::Value>(&path).unwrap_err();
        assert!(
            error.contains("未启用 client-orchestration feature"),
            "unexpected error: {error}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_nested_segment_directories_in_path_order() {
        let root = std::env::temp_dir().join(format!(
            "depth-lab-jsonl-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = root.join("00000");
        let second = root.join("00001");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("rows.jsonl"), b"{\"frame\":2}\n").unwrap();
        std::fs::write(first.join("rows.jsonl"), b"{\"frame\":1}\n").unwrap();

        let rows: Vec<serde_json::Value> =
            read_jsonl_set(&root, |path| path.file_name().unwrap() == "rows.jsonl").unwrap();
        assert_eq!(rows[0]["frame"], 1);
        assert_eq!(rows[1]["frame"], 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_report_discovery_accepts_only_zero_padded_sealed_phase_files() {
        let root = std::env::temp_dir().join(format!(
            "depth-lab-camera-sweep-files-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for name in [
            "camera-candidates-POSE_STATIC_W000.json",
            "camera-candidates-POSE_STATIC_W008.json",
            "camera-candidates-POSE_WALK_W000.json",
            "camera-candidates.json",
            "camera-candidates-HOLD_AFTER_STATIC.json",
        ] {
            std::fs::write(root.join(name), b"{}").unwrap();
        }
        let files = collect_camera_evidence_phase(&root, "POSE_STATIC").unwrap();
        assert_eq!(files.keys().copied().collect::<Vec<_>>(), vec![0, 8]);
        assert!(files[&0].ends_with("camera-candidates-POSE_STATIC_W000.json"));

        std::fs::write(root.join("camera-candidates-POSE_STATIC_W16.json"), b"{}").unwrap();
        assert!(collect_camera_evidence_phase(&root, "POSE_STATIC").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}

fn require<'a>(m: &'a BTreeMap<String, String>, k: &str) -> Result<&'a String, String> {
    m.get(k).ok_or_else(|| format!("缺少 --{k}\n{}", usage()))
}

fn cmd_clip_plan(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::clip_plan;

    let frames = read_recorder_frames(&PathBuf::from(require(m, "frames")?))?;
    let samples = read_watermark_samples(&PathBuf::from(require(m, "samples")?))?;
    let depth: Vec<DepthIndexRow> = read_jsonl(&PathBuf::from(require(m, "depth")?))?;
    let target: usize = require(m, "target-frames")?
        .parse()
        .map_err(|e| format!("--target-frames 不是数字: {e}"))?;

    let plan = clip_plan::plan(&frames, &samples, &depth, target)?;

    let out = PathBuf::from(require(m, "out")?);
    fs::write(
        &out,
        serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写不了 {}: {e}", out.display()))?;

    println!(
        "选帧 {}/{}(目标 {}):源帧 {}..{},剔 {}(重复 {} / 重对齐 {})→ {}",
        plan.achieved_frames,
        frames.len(),
        plan.target_frames,
        plan.start_session_frame,
        plan.end_session_frame,
        plan.drops.len(),
        plan.duplicate_drops,
        plan.realign_drops,
        out.display()
    );
    if !plan.met_target {
        println!(
            "❌ 未达目标长度——最长可构造 {} 帧;要么换采集段,要么接受更短档位",
            plan.achieved_frames
        );
    }
    Ok(plan.met_target)
}

fn cmd_pair(m: &BTreeMap<String, String>) -> Result<bool, String> {
    let frames = read_recorder_frames(&PathBuf::from(require(m, "frames")?))?;
    let samples = read_watermark_samples(&PathBuf::from(require(m, "samples")?))?;
    let depth: Vec<DepthIndexRow> = read_jsonl(&PathBuf::from(require(m, "depth")?))?;

    let mode = match m.get("camera-mode").map(String::as_str) {
        None | Some("player") => CameraMode::Player,
        Some("freecam") => CameraMode::Freecam,
        Some(other) => {
            return Err(format!(
                "--camera-mode 只能是 player 或 freecam，收到 `{other}`"
            ));
        }
    };

    let (pairs, report) = run(&frames, &samples, &depth, mode).map_err(|e| e.to_string())?;

    let out = PathBuf::from(require(m, "out")?);
    let mut f = fs::File::create(&out).map_err(|e| format!("写不了 {}: {e}", out.display()))?;
    for p in &pairs {
        writeln!(
            f,
            "{}",
            serde_json::to_string(p).map_err(|e| e.to_string())?
        )
        .map_err(|e| e.to_string())?;
    }

    let rp = PathBuf::from(require(m, "report")?);
    fs::write(
        &rp,
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写不了 {}: {e}", rp.display()))?;

    println!("帧对 {} 行 → {}", pairs.len(), out.display());
    println!(
        "水印解码 {}/{}（同步字不符 {}、模糊 {}、缺样本 {}、在线身份不一致 {}）",
        report.watermark.decoded,
        frames.len(),
        report.watermark.sync_mismatch,
        report.watermark.ambiguous,
        report.watermark.missing,
        report.watermark.identity_mismatch
    );
    println!(
        "配对覆盖率 {:.4}（可配对偶数帧 {}，配上 {}，继承 {}，未被引用的 depth {}）",
        report.pairing.coverage,
        report.pairing.eligible_even_frames,
        report.pairing.paired_even_frames,
        report.pairing.inherited_frames,
        report.pairing.unreferenced_depth_frames
    );
    if report.passed {
        println!("✅ 两道门均通过");
    } else {
        println!("❌ 未通过：");
        for x in &report.failures {
            println!("   - {x}");
        }
    }
    Ok(report.passed)
}

/// `materialize`：raw 归一化深度 → EXR 交付层。
///
/// 产出 **Variant R 常驻**（fp32 EXR）+ **Variant S 的配方**
/// （逐帧预期 sha256，字节按需再生 —— 77 号 §5.5.8 旋钮 1）。
///
/// `--keep-spec` 可要求同时留下 S 的字节，用于 P1f 的交付形态自检。
#[cfg(feature = "exr-out")]
fn cmd_materialize(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::depth::{Projection, Variant};
    use depth_lab::exr_out;

    let raw_dir = PathBuf::from(require(m, "raw")?);
    let index: Vec<DepthIndexRow> = read_jsonl(&PathBuf::from(require(m, "depth")?))?;
    let out_dir = PathBuf::from(require(m, "out")?);
    let recipe_path = PathBuf::from(require(m, "recipe")?);

    let near: f64 = require(m, "near")?
        .parse()
        .map_err(|e| format!("--near 不是数字: {e}"))?;
    let metric_scale: f64 = m
        .get("metric-scale")
        .map_or(Ok(1.0), |v| v.parse())
        .map_err(|e| format!("--metric-scale 不是数字: {e}"))?;
    // 给了 --far 就是传统 Z，没给就是 reversed-Z + 无穷远。
    // 两种约定的 (A,B) 完全不同，猜错会让整份深度反过来。
    let projection = match m.get("far") {
        Some(f) => Projection::traditional_z(
            near,
            f.parse().map_err(|e| format!("--far 不是数字: {e}"))?,
            metric_scale,
        ),
        None => Projection::reversed_z_infinite(near, metric_scale),
    };
    let keep_spec = m.get("keep-spec").map(PathBuf::from);

    let mut recipe_rows = Vec::new();
    let mut done = 0usize;

    for row in &index {
        let raw_path = raw_dir.join(format!("{:012}.f32", row.present_seq));
        let bytes =
            fs::read(&raw_path).map_err(|e| format!("读 {} 失败: {e}", raw_path.display()))?;
        if !bytes.len().is_multiple_of(4) {
            return Err(format!("{} 字节数不是 4 的倍数", raw_path.display()));
        }
        let depth: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let (w, h) = (row.width as usize, row.height as usize);

        let name = format!("depth_{}", row.present_seq);
        exr_out::write_frame(
            &out_dir.join(format!("{name}.exr")),
            &depth,
            w,
            h,
            projection,
            Variant::Reference,
        )?;

        // Variant S：生成一次算出预期哈希；不常驻则随后删掉
        let spec_path = keep_spec.as_ref().map_or_else(
            || out_dir.join(format!(".{name}.spec.tmp")),
            |d| d.join(format!("{name}.exr")),
        );
        exr_out::write_frame(&spec_path, &depth, w, h, projection, Variant::Spec)?;
        let spec_bytes = fs::read(&spec_path).map_err(|e| e.to_string())?;
        let digest = sha256_hex(&spec_bytes);
        if keep_spec.is_none() {
            let _ = fs::remove_file(&spec_path);
        }

        recipe_rows.push(serde_json::json!({
            "present_seq": row.present_seq,
            "depth_name": name,
            "expected_sha256": digest,
            "bytes": spec_bytes.len(),
        }));
        done += 1;
    }

    let recipe = serde_json::json!({
        "variant": "spec",
        "note": "Variant S 不常驻；用本配方从 Variant R 的同一 raw 输入再生，并逐帧核对 expected_sha256",
        "encoder": "zip",
        "channel": exr_out::CHANNEL_NAME,
        "projection": { "a": projection.a, "b": projection.b, "metric_scale": projection.metric_scale },
        // 这是**声明的交付约定**，不是从投影矩阵测出来的值——
        // materialize 只处理深度，拿不到内参。P4 接上之后应改为
        // 测量值，并先过 Intrinsics::check_square_pixels()。
        // 字段名带 _source 就是为了让下游能分清「声明」与「实测」。
        "pixel_aspect_ratio": 1.0,
        "pixel_aspect_ratio_source": "declared_square_pixels",
        "frames": recipe_rows,
    });
    fs::write(
        &recipe_path,
        serde_json::to_string_pretty(&recipe).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写 {} 失败: {e}", recipe_path.display()))?;

    println!("物化 {done} 帧 → {}", out_dir.display());
    println!("Variant S 配方 → {}", recipe_path.display());
    if keep_spec.is_some() {
        println!("（同时保留了 Variant S 字节）");
    }
    Ok(true)
}

/// 极简 SHA-256。不引依赖——用处少且要能被逐位核对。
///
/// **不要给它加特性门。** 调用点有两个：`materialize` 的配方摘要（exr-out），
/// 以及 `prepare-fallout4-action-context` 里 axis ini 的 evidence_id（默认特性，
/// 无门）。曾经这里挂着 `#[cfg(feature = "exr-out")]`，于是默认特性下
/// `cargo test -p depth-lab`——也就是本 crate README 教开发者跑的那条——直接编不过。
/// `push_s3.rs` 里另有一个同名私有函数，那个走 sha2 crate，两者不要合并。
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while !(msg.len() % 64).eq(&56) {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].into_iter().enumerate() {
            h[i] = h[i].wrapping_add(v);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

// 这些用例**不能**跟着下面 exr-out 那个 mod 走：sha256_hex 也在默认特性下的
// prepare-fallout4-action-context 里给 evidence_id 供哈希，那条路径必须在默认
// 特性的 CI lane 上被验到。
#[cfg(test)]
mod sha256_tests {
    use super::sha256_hex;

    #[test]
    fn sha256_matches_the_published_vectors() {
        // 手写实现必须对着标准向量验，否则配方里的哈希是假的、
        // 而「再生后核对哈希」这套机制会变成永远通过。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_handles_the_length_padding_boundaries() {
        // 55/56/64 字节是 SHA-256 补位逻辑最容易写错的三个长度
        for n in [54usize, 55, 56, 57, 63, 64, 65, 119, 120] {
            let data = vec![0x61u8; n];
            let got = sha256_hex(&data);
            assert_eq!(got.len(), 64, "长度 {n} 的输出不是 64 个 hex 字符");
        }
        // 56 字节：补位后正好跨到第二个块
        assert_eq!(
            sha256_hex(&[0x61u8; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
    }
}

/// 采集 → 68 号三件交付物。
///
/// 与上面的 `sha256_hex` 相反：这里唯一的非测试调用点是 `cmd_deliver`，它本身就
/// 挂着 exr-out，所以门是对的——不加的话默认特性下这是一个 dead_code，
/// `cargo clippy -- -D warnings` 会红。哪天默认特性的路径也要调它，
/// 记得连门一起摘，别只加调用点（那正是 #630 踩的坑）。
#[cfg(feature = "exr-out")]
fn apply_clip_plan(
    pairs: Vec<depth_lab::pairing::FramePair>,
    plan: &depth_lab::clip_plan::ClipPlan,
    capture_dsv: &BTreeMap<u64, String>,
) -> Result<Vec<depth_lab::pairing::FramePair>, String> {
    if plan.schema != depth_lab::clip_plan::CLIP_PLAN_SCHEMA_V1 {
        return Err(format!("不支持的 clip plan schema: {}", plan.schema));
    }
    if !plan.met_target || plan.achieved_frames != plan.selected.len() {
        return Err("clip plan 未达到目标，或 achieved_frames 与 selected 不一致".into());
    }
    if plan.realign_drops != 0 {
        return Err(format!(
            "clip plan 有 {} 个 realign drop；every_present 交付要求为 0",
            plan.realign_drops
        ));
    }

    let mut by_session_frame = BTreeMap::new();
    for pair in pairs {
        let session_frame = pair.session_frame;
        if by_session_frame.insert(session_frame, pair).is_some() {
            return Err(format!("配对表重复 session_frame {session_frame}"));
        }
    }
    let mut selected = Vec::with_capacity(plan.selected.len());
    let mut previous = None;
    for (output_frame, &session_frame) in plan.selected.iter().enumerate() {
        if previous.is_some_and(|value| value >= session_frame) {
            return Err("clip plan selected 必须严格递增".into());
        }
        previous = Some(session_frame);
        let mut pair = by_session_frame
            .remove(&session_frame)
            .ok_or_else(|| format!("配对表缺少 clip plan 选中的帧 {session_frame}"))?;
        pair.session_frame = output_frame as u64;
        if output_frame.is_multiple_of(2) {
            let present_seq = pair
                .present_seq
                .or_else(|| pair.depth.as_ref().map(|depth| depth.present_seq))
                .ok_or_else(|| format!("源帧 {session_frame} 没有可用的 watermark present_seq"))?;
            let dsv_id = capture_dsv.get(&present_seq).ok_or_else(|| {
                format!("源帧 {session_frame} 引用的 present_seq {present_seq} 没有 depth")
            })?;
            pair.depth = Some(depth_lab::pairing::PairedDepth {
                present_seq,
                depth_name: format!("depth_{output_frame}"),
                dsv_id: dsv_id.clone(),
                inherited: false,
            });
            pair.depth_absent_reason = None;
        } else {
            pair.depth = None;
            pair.depth_absent_reason = Some(depth_lab::pairing::AbsentReason::OddFrameBySpec);
        }
        selected.push(pair);
    }
    Ok(selected)
}

#[cfg(feature = "exr-out")]
fn read_delivery_poses(
    path: &Path,
    selected_source_frames: Option<&std::collections::BTreeSet<u64>>,
) -> Result<BTreeMap<u64, depth_lab::deliver::PoseRow>, String> {
    let Some(selected_source_frames) = selected_source_frames else {
        return depth_lab::deliver::read_pose_index(path);
    };
    let mut poses = BTreeMap::new();
    for row in depth_lab::deliver::read_pose_rows(path)? {
        if !selected_source_frames.contains(&row.session_frame) {
            continue;
        }
        let present_seq = row.present_seq;
        if poses.insert(present_seq, row).is_some() {
            return Err(format!(
                "选帧后的 pose 仍重复 present_seq {present_seq}；clip plan 没有消除继承帧"
            ));
        }
    }
    if poses.len() != selected_source_frames.len() {
        return Err(format!(
            "clip plan 选中 {} 帧，但只找到 {} 行对应 pose",
            selected_source_frames.len(),
            poses.len()
        ));
    }
    Ok(poses)
}

#[cfg(feature = "exr-out")]
const FALLOUT4_CAMERA_EVIDENCE: &[u8] =
    include_bytes!("../../pr405-fallout4/evidence/fallout4-render-camera-egeo-20260811.json");
// catalog bundle 里的那一份就是唯一事实源——客户端、pr405 脚本、这里的授权门
// 读的必须是同一个文件。见 130 号 §2.4。
#[cfg(feature = "exr-out")]
const FALLOUT4_PRODUCTION_BINDING: &[u8] =
    include_bytes!("../../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json");

#[cfg(feature = "exr-out")]
#[derive(serde::Deserialize)]
struct Fallout4PromotionEvidence {
    schema: String,
    runtime: Fallout4EvidenceRuntime,
    e_geo: Fallout4EGeoEvidence,
    production_binding: Fallout4ProductionBindingEvidence,
}

#[cfg(feature = "exr-out")]
#[derive(serde::Deserialize)]
struct Fallout4EvidenceRuntime {
    executable_sha256: String,
}

#[cfg(feature = "exr-out")]
#[derive(serde::Deserialize)]
struct Fallout4EGeoEvidence {
    schema: String,
    passed: bool,
    production_binding_authorized: bool,
}

#[cfg(feature = "exr-out")]
#[derive(serde::Deserialize)]
struct Fallout4ProductionBindingEvidence {
    binding_id: String,
    binding_version: String,
    sha256: String,
    profile_executable_render_and_camera_sources_exactly_equal: bool,
}

#[cfg(feature = "exr-out")]
fn verify_fallout4_camera_authorization(
    poses: &[&depth_lab::deliver::PoseRow],
    camera_index_path: Option<&str>,
    evidence_bytes: &[u8],
    binding_bytes: &[u8],
) -> Result<(), String> {
    use depth_lab::{
        CameraBinding, CameraPoseProvenance, CameraPoseRow, CameraPoseSource, FinalCameraIdentity,
    };

    let needs_fallout4_gate = poses
        .iter()
        .any(|row| row.camera_source == CameraPoseSource::ResolvedRenderCameraHybrid);
    if !needs_fallout4_gate {
        return Ok(());
    }
    if poses
        .iter()
        .any(|row| row.camera_source != CameraPoseSource::ResolvedRenderCameraHybrid)
    {
        return Err("FO4 render-hybrid 交付混入了其他相机来源".into());
    }

    let evidence: Fallout4PromotionEvidence = serde_json::from_slice(evidence_bytes)
        .map_err(|error| format!("仓库内 FO4 E-geo 授权证据解析失败: {error}"))?;
    if evidence.schema != "fallout4-render-camera-promotion-evidence/v1" {
        return Err(format!(
            "FO4 E-geo 授权证据 schema 不受支持: {}",
            evidence.schema
        ));
    }
    if evidence.e_geo.schema != "camera-egeo-validation/v3"
        || !evidence.e_geo.passed
        || !evidence.e_geo.production_binding_authorized
    {
        return Err("FO4 E-geo v3 没有通过并授权正式 CameraBinding".into());
    }
    if !evidence
        .production_binding
        .profile_executable_render_and_camera_sources_exactly_equal
    {
        return Err("FO4 E-geo 证据没有确认 candidate 与正式 binding 的语义字段相等".into());
    }

    let binding: CameraBinding = serde_json::from_slice(binding_bytes)
        .map_err(|error| format!("仓库内 FO4 正式 CameraBinding 解析失败: {error}"))?;
    let actual_binding_sha = format!("sha256:{}", sha256_hex(binding_bytes));
    if !actual_binding_sha.eq_ignore_ascii_case(&evidence.production_binding.sha256)
        || binding.binding_id != evidence.production_binding.binding_id
        || binding.binding_version != evidence.production_binding.binding_version
        || binding.executable_sha256 != evidence.runtime.executable_sha256
    {
        return Err("仓库内 FO4 正式 CameraBinding 与 E-geo 授权记录不匹配".into());
    }
    let Some(expected_game_camera) = binding.game_camera.as_ref() else {
        return Err("FO4 正式 CameraBinding 缺少 exact-build GameCamera".into());
    };
    if binding.view_sources.len() != 1 || binding.projection_sources.len() != 1 {
        return Err("FO4 正式 CameraBinding 不是唯一 render-basis/projection source".into());
    }

    let camera_index_path = camera_index_path.ok_or(
        "FO4 render-hybrid 交付必须给 --camera-index，不能只信已剥离 provenance 的最终 pose",
    )?;
    let camera_rows: Vec<CameraPoseRow> = read_jsonl(&PathBuf::from(camera_index_path))?;
    let first_identity = camera_rows
        .first()
        .map(|row| &row.identity)
        .ok_or("FO4 camera-index 为空")?;
    let mut by_present = BTreeMap::new();
    for row in &camera_rows {
        if &row.identity != first_identity {
            return Err(format!(
                "FO4 camera-index 跨 stream identity: present_seq {}",
                row.present_seq
            ));
        }
        depth_lab::validate_camera_pose_row(row).map_err(|reason| {
            format!(
                "FO4 camera-index present_seq {} 无效: {reason}",
                row.present_seq
            )
        })?;
        if by_present.insert(row.present_seq, row).is_some() {
            return Err(format!(
                "FO4 camera-index 重复 present_seq {}",
                row.present_seq
            ));
        }
    }

    let mut observed_executable_sha256: Option<&str> = None;
    for pose in poses {
        let FinalCameraIdentity::ProbeStream {
            session_id,
            run_id,
            generation,
        } = &pose.camera_identity
        else {
            return Err(format!(
                "FO4 pose present_seq {} 没有 probe stream identity",
                pose.present_seq
            ));
        };
        let camera = by_present.get(&pose.present_seq).ok_or_else(|| {
            format!(
                "FO4 camera-index 缺少交付 pose 的 present_seq {}",
                pose.present_seq
            )
        })?;
        if camera.identity.session_id != *session_id
            || camera.identity.run_id != *run_id
            || camera.identity.generation != *generation
        {
            return Err(format!(
                "FO4 pose/camera-index stream identity 不一致: present_seq {}",
                pose.present_seq
            ));
        }
        if camera.source != CameraPoseSource::ResolvedRenderCameraHybrid
            || camera.camera_position != pose.camera_position
            || camera.camera_rotation_quaternion != pose.camera_rotation_quaternion
            || camera.camera_intrinsics != pose.camera_intrinsics
        {
            return Err(format!(
                "FO4 pose 与 camera-index 行不一致: present_seq {}",
                pose.present_seq
            ));
        }
        let CameraPoseProvenance::ResolvedRenderCameraHybrid {
            binding_id,
            binding_version,
            profile_id,
            profile_version,
            executable_sha256,
            render_width,
            render_height,
            game_camera,
            render_basis_source,
            projection_source,
            ..
        } = &camera.provenance
        else {
            return Err(format!(
                "FO4 camera-index present_seq {} 不是 render-hybrid provenance",
                pose.present_seq
            ));
        };
        if let Some(observed) = observed_executable_sha256 {
            if observed != executable_sha256 {
                return Err(format!(
                    "FO4 camera-index 混入多个实际 EXE hash: present_seq {}",
                    pose.present_seq
                ));
            }
        } else {
            observed_executable_sha256 = Some(executable_sha256.as_str());
        }
        if binding_id != &binding.binding_id
            || binding_version != &binding.binding_version
            || profile_id != &binding.profile_id
            || profile_version != &binding.profile_version
            || executable_sha256 != &binding.executable_sha256
            || render_width != &binding.render_width
            || render_height != &binding.render_height
            || game_camera != expected_game_camera
            || render_basis_source != &binding.view_sources[0]
            || projection_source != &binding.projection_sources[0]
        {
            return Err(format!(
                "FO4 camera-index provenance 与正式 CameraBinding 不一致: present_seq {}",
                pose.present_seq
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod default_feature_tests {
    use super::sha256_hex;

    #[test]
    fn action_context_sha256_helper_is_available_without_exr_out() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

#[cfg(feature = "exr-out")]
fn cmd_deliver(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::deliver;

    let capture = PathBuf::from(require(m, "capture")?);
    let pairs_path = PathBuf::from(require(m, "pairs")?);
    let out_root = PathBuf::from(require(m, "out")?);
    let clip_id = require(m, "clip-id")?;
    for obsolete in ["near", "far", "metric-scale", "projection-provenance"] {
        if m.contains_key(obsolete) {
            return Err(format!(
                "deliver 不再接受 --{obsolete}：正式转换只使用 depth-index.jsonl 逐帧记录的 (A,B,metric_scale) 及来源"
            ));
        }
    }

    if m.contains_key("plan") && m.contains_key("limit") {
        return Err("--plan 与 --limit 不能同时使用".into());
    }
    let rows = deliver::read_capture_index(&capture)?;
    let mut pairs: Vec<depth_lab::pairing::FramePair> = read_jsonl(&pairs_path)?;
    let source_pair_count = pairs.len();
    let mut selected_source_frames = None;
    if let Some(plan_path) = m.get("plan") {
        let text = fs::read_to_string(plan_path)
            .map_err(|error| format!("读不了 clip plan {plan_path}: {error}"))?;
        let plan: depth_lab::clip_plan::ClipPlan = serde_json::from_str(&text)
            .map_err(|error| format!("解析 clip plan {plan_path} 失败: {error}"))?;
        let capture_dsv = rows
            .iter()
            .map(|row| (row.present_seq, row.dsv_id.clone()))
            .collect();
        selected_source_frames = Some(plan.selected.iter().copied().collect());
        pairs = apply_clip_plan(pairs, &plan, &capture_dsv)?;
    }
    let mut delivery_rows = deliver::select_delivery_depth_rows(&rows, &pairs)?;
    let verified_depth_rows = delivery_rows.len();
    if let Some(limit) = m.get("limit") {
        let n: usize = limit
            .parse()
            .map_err(|e| format!("--limit 不是数字: {e}"))?;
        delivery_rows.truncate(n);
    }
    println!(
        "采集索引 {} 行；配对表 {} 行；完整验证通过 {} 个 depth 帧；本次转换 {} 个",
        rows.len(),
        source_pair_count,
        verified_depth_rows,
        delivery_rows.len()
    );

    // 采集侧阻塞项**在做任何转换之前**先报。
    //
    // 正式转换只使用采集索引逐帧记录的参数；缺参数或可信来源时**必须重采**，
    // 已有字节永远补不回来。#490 的 61 GiB 当时没有任何一道门看得见这个问题。
    //
    // 先报再转，是为了让「白跑一小时转换才发现要重采」这件事不再发生。
    let selected_capture_rows: Vec<_> = delivery_rows
        .iter()
        .map(|row| row.capture().clone())
        .collect();
    let mut capture_blocking = deliver::capture_blockers(&selected_capture_rows);
    // present 帧率裁决（#801）：violated 与缺投影参数同级——重采，不是重跑转换。
    if let Some(blocker) = deliver::present_rate_blocker(&capture)? {
        capture_blocking.push(blocker);
    }
    if !capture_blocking.is_empty() {
        println!("\n❌ 采集侧阻塞（这些要重新采集，不是重跑转换）：");
        for x in &capture_blocking {
            println!("   - {x}");
        }
        println!();
        // 输入本身已经证明必须重采，继续转换只会白跑并留下看似可用的半成品。
        // `Ok(false)` 表示输入不完整（退出码 1），不是工具内部故障（退出码 2）。
        return Ok(false);
    }
    let (projection, projection_provenance) = deliver::delivery_projection(&delivery_rows)?;
    let prepared_poses = m
        .get("pose")
        .map(|path| read_delivery_poses(Path::new(path), selected_source_frames.as_ref()))
        .transpose()?;
    let prepared_poses30 = m
        .get("pose30")
        .map(|path| deliver::read_pose_rows(Path::new(path)))
        .transpose()?;
    let camera_gate_poses = prepared_poses
        .iter()
        .flat_map(|rows| rows.values())
        .chain(prepared_poses30.iter().flatten())
        .collect::<Vec<_>>();
    verify_fallout4_camera_authorization(
        &camera_gate_poses,
        m.get("camera-index").map(String::as_str),
        FALLOUT4_CAMERA_EVIDENCE,
        FALLOUT4_PRODUCTION_BINDING,
    )?;

    let dir = deliver::clip_dir(&out_root, clip_id);
    fs::create_dir_all(&dir).map_err(|e| format!("建 {} 失败: {e}", dir.display()))?;

    let mut blocking = Vec::new();

    // ---- depth ----
    let package = deliver::build_depth_zip(&capture, &delivery_rows)?;
    let depth_path = dir.join("depth.zip");
    fs::write(&depth_path, &package.zip_bytes).map_err(|e| format!("写 depth.zip 失败: {e}"))?;
    println!(
        "depth.zip：{} 帧，{:.2} MiB，{}..{}",
        package.frames,
        package.zip_bytes.len() as f64 / (1u64 << 20) as f64,
        package.first_name.clone().unwrap_or_default(),
        package.last_name.clone().unwrap_or_default()
    );
    let depth_state = deliver::ArtifactState::Produced {
        path: depth_path.display().to_string(),
        bytes: package.zip_bytes.len() as u64,
    };

    // ---- video ----
    let video_state = match m.get("video") {
        Some(src) => {
            let dst = dir.join("video.mp4");
            let bytes = fs::copy(src, &dst).map_err(|e| format!("拷贝 {src} 失败: {e}"))?;
            // 68 号要 1080p/30fps。这里只做能离线做的那一半核对——真正的
            // 帧数对齐要等 QC，但分辨率/帧率对不上时越早报越好。
            match deliver::probe_video(&dst) {
                Ok(info) => {
                    println!(
                        "video.mp4：{}×{} @{:.3}fps，{} 帧",
                        info.width, info.height, info.fps, info.frames
                    );
                    if info.width != 1920 || info.height != 1080 {
                        blocking.push(format!(
                            "video.mp4 是 {}×{}，68 号 §6.1 要 1080p（1920×1080）",
                            info.width, info.height
                        ));
                    }
                    if (info.fps - 30.0).abs() > 0.05 {
                        blocking.push(format!("video.mp4 是 {:.3} fps，68 号要 30 fps", info.fps));
                    }
                    // depth 是偶数帧抽样，所以视频帧数应当覆盖 depth 的最后一帧
                    let need = delivery_rows
                        .last()
                        .map_or(0, |row| row.session_frame() as usize + 1);
                    if info.frames > 0 && (info.frames as usize) < need {
                        blocking.push(format!(
                            "video.mp4 只有 {} 帧，而 depth 覆盖到第 {} 帧——两者对不齐",
                            info.frames,
                            need - 1
                        ));
                    }
                }
                Err(e) => blocking.push(format!("video.mp4 探测失败（装了 ffprobe 吗）：{e}")),
            }
            deliver::ArtifactState::Produced {
                path: dst.display().to_string(),
                bytes,
            }
        }
        None => {
            let reason = "没给 --video。RGB 视频来自 DataMiner 客户端的采集，探针不产它；\
                          68 号要 1080p/30fps h264，且必须与 depth 同源同帧"
                .to_string();
            blocking.push(format!("video.mp4：{reason}"));
            deliver::ArtifactState::Missing { reason }
        }
    };

    // ---- action_camera.json ----
    // 生成而不是拷贝：depth_name / frame 必须与 depth.zip 里的命名逐字一致，
    // 而那套命名是本次转换才决定的（clip 内从 0 重数）。
    let pose_state = match prepared_poses {
        Some(poses) => {
            let json = deliver::build_action_camera(&delivery_rows, &poses, 30.0)?;
            let dst = dir.join("action_camera.json");
            fs::write(&dst, &json).map_err(|e| format!("写 action_camera.json 失败: {e}"))?;
            println!(
                "action_camera.json：{} 帧，与 depth.zip 同名同序",
                delivery_rows.len()
            );
            deliver::ArtifactState::Produced {
                path: dst.display().to_string(),
                bytes: json.len() as u64,
            }
        }
        None => {
            let reason = "没给 --pose。逐帧相机位姿来自 P4 的常量缓冲还原（depth_lab::camera）\
                          或游戏遥测；探针目前不产它"
                .to_string();
            blocking.push(format!("action_camera.json：{reason}"));
            deliver::ArtifactState::Missing { reason }
        }
    };

    // ---- action_camera_30fps.json ----
    // 68 号 §1 要的是**两个** action_camera：15fps 那个绑 depth（带 depth_name），
    // 30fps 那个覆盖每一个 RGB 帧且**没有** depth_name。两者用同一份位姿旁车即可，
    // 但 30fps 侧要求 session_frame 逐帧连续，所以走 read_pose_rows 而不是按
    // present_seq 收敛的 read_pose_index（重复帧共用一个 Present 是合法的）。
    if let Some(poses) = prepared_poses30 {
        let json = deliver::build_action_camera_30fps(&poses)?;
        let dst = dir.join("action_camera_30fps.json");
        fs::write(&dst, &json).map_err(|e| format!("写 action_camera_30fps.json 失败: {e}"))?;
        println!("action_camera_30fps.json：{} 帧", poses.len());
    } else {
        blocking.push(
            "action_camera_30fps.json：没给 --pose30（68 号 §1 要求 15fps / 30fps 两份）"
                .to_string(),
        );
    }

    let manifest = deliver::DeliveryManifest {
        clip_id: clip_id.clone(),
        spec: "docs/68 §6.4/§6.5".into(),
        video: video_state,
        depth: depth_state,
        action_camera: pose_state,
        depth_frames: package.frames,
        depth_first_name: package.first_name,
        depth_last_name: package.last_name,
        projection: deliver::ProjectionRecord {
            a: projection.a,
            b: projection.b,
            metric_scale: projection.metric_scale,
            provenance: projection_provenance.into(),
        },
        blocking,
    };
    let manifest_path = dir.join("delivery-manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写清单失败: {e}"))?;

    if manifest.is_complete() {
        println!("交付完整：{}", dir.display());
    } else {
        println!("交付**不完整**，缺：");
        for item in &manifest.blocking {
            println!("  - {item}");
        }
        println!("清单：{}", manifest_path.display());
    }
    Ok(manifest.is_complete())
}

/// 把一次采集目录推上 R2（85 号 M4）。
///
/// 返回 `Ok(false)` 表示「跑完了但远端没收全」——这要重跑，与工具本身出错
/// （`Err`）是两种不同的处置，所以退出码也分开。
#[cfg(feature = "push")]
fn cmd_push(m: &BTreeMap<String, String>) -> Result<bool, String> {
    use depth_lab::push;
    use depth_lab::push_s3::{R2Client, R2Credentials};

    let dir = PathBuf::from(require(m, "dir")?);
    let bucket = require(m, "bucket")?;
    let dry_run = m.contains_key("dry-run");

    let manifest = push::scan_capture_dir(&dir)?;
    println!(
        "采集 {}：{} 个对象，{:.2} GiB，索引 {} 行",
        manifest.capture_id,
        manifest.objects.len(),
        manifest.total_bytes() as f64 / (1u64 << 30) as f64,
        manifest.index_rows
    );

    // 上传前先判断这批采集本身能不能交付。缺投影参数、同段投影变化或
    // present_seq 断口都只能重采，不能等字节进桶后再假装补得回来。
    // 性能/格式样本仍允许上传，但必须由操作者显式确认其不可交付属性。
    let rows = depth_lab::deliver::read_capture_index(&dir)?;
    let blockers = depth_lab::deliver::capture_blockers(&rows);
    if !blockers.is_empty() {
        println!("\n⚠️ 这批采集不能交付：");
        for blocker in &blockers {
            println!("   - {blocker}");
        }
        if !m.contains_key("allow-nondeliverable") {
            return Err(
                "拒绝上传：这批采集不满足 68 号交付规格，且缺的是采集时的输入，\
                 事后补不回来。确实只想留作性能/格式样本的话，加 --allow-nondeliverable。"
                    .into(),
            );
        }
        println!("   （--allow-nondeliverable 已给出，按不可交付样本上传）\n");
    }

    let client = R2Client::new(R2Credentials::from_env()?, bucket.clone())?;
    let prefix = format!("{}/", manifest.capture_id);
    let remote = client.list_prefix(&prefix)?;
    // 远端键带前缀，本地键不带——比对前先对齐，否则每次都判成「全都要传」
    let remote: Vec<push::RemoteObject> = remote
        .into_iter()
        .filter_map(|r| {
            r.key.strip_prefix(&prefix).map(|k| push::RemoteObject {
                key: k.to_string(),
                size_bytes: r.size_bytes,
            })
        })
        .collect();
    println!("远端已有 {} 个对象", remote.len());

    let plan = push::plan_upload(&manifest, &remote);
    let mismatched: Vec<&push::PlannedObject> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, push::ObjectAction::ReuploadSizeMismatch { .. }))
        .collect();
    println!(
        "计划：传 {} 个（{:.2} GiB），跳过 {} 个，其中大小不符需重传 {} 个",
        plan.to_upload().len(),
        plan.bytes_to_upload() as f64 / (1u64 << 30) as f64,
        plan.skipped(),
        mismatched.len()
    );
    for item in &mismatched {
        println!(
            "  大小不符（远端 {} 字节，本地 {}）：{}",
            match item.action {
                push::ObjectAction::ReuploadSizeMismatch { remote_bytes } => remote_bytes,
                _ => 0,
            },
            item.size_bytes,
            item.key
        );
    }
    if dry_run {
        println!("--dry-run，未上传。");
        return Ok(true);
    }

    let todo = plan.to_upload();
    let total = todo.len();
    for (i, item) in todo.iter().enumerate() {
        let path = push::local_path(&dir, &item.key);
        let body = fs::read(&path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
        client.put_object(&push::remote_key(&manifest.capture_id, &item.key), body)?;
        if (i + 1) % 100 == 0 || i + 1 == total {
            println!("  已传 {}/{total}", i + 1);
        }
    }

    // 传完必须重新列一次远端核对。「PUT 都返回 200」不等于「远端收全了」——
    // 中途被打断、并发写、桶侧限流都会让两者分叉。
    let remote_after: Vec<push::RemoteObject> = client
        .list_prefix(&prefix)?
        .into_iter()
        .filter_map(|r| {
            r.key.strip_prefix(&prefix).map(|k| push::RemoteObject {
                key: k.to_string(),
                size_bytes: r.size_bytes,
            })
        })
        .collect();
    let report = push::verify_complete(&manifest, &remote_after);
    println!(
        "核对：远端 {}/{} 个对象，索引与帧数{}",
        report.present_objects,
        report.expected_objects,
        if report.index_consistent {
            "自洽"
        } else {
            "不自洽"
        }
    );
    for key in report.missing.iter().take(20) {
        println!("  缺失：{key}");
    }
    if report.missing.len() > 20 {
        println!("  …还有 {} 个", report.missing.len() - 20);
    }
    for key in report.size_mismatch.iter().take(20) {
        println!("  大小不符：{key}");
    }
    Ok(report.is_complete())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    let m = match parse_args(rest) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match cmd.as_str() {
        #[cfg(feature = "client-orchestration")]
        "assemble-action" => match cmd_assemble_action(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "client-orchestration")]
        "assemble-action-30" => match cmd_assemble_action_30(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "ets2")]
        "ets2-pose" => match cmd_ets2_pose(&m) {
            Ok(true) => ExitCode::SUCCESS,
            // 门没过 ≠ 工具挂了：位姿文件与报告都已写出，可以据此排障。
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "ets2")]
        "ets2-map-bounds" => match cmd_ets2_map_bounds(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "clip-plan" => match cmd_clip_plan(&m) {
            Ok(true) => ExitCode::SUCCESS,
            // 没凑够目标长度 ≠ 工具挂了,退出码要能区分
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "pair" => match cmd_pair(&m) {
            Ok(true) => ExitCode::SUCCESS,
            // 门没过 ≠ 工具挂了，退出码要能区分
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "video-concat" => match cmd_video_concat(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "prepare-fallout4-action-context" => match cmd_prepare_fallout4_action_context(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "camera-evidence" => match cmd_camera_evidence(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "camera-evidence-sweep" => match cmd_camera_evidence_sweep(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        "camera-binding-author" => match cmd_camera_binding_author(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        "dsv-discovery-validate" => match cmd_dsv_discovery_validate(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "evidence-authoring")]
        "dsv-discovery-pass-summary" => match cmd_dsv_discovery_pass_summary(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "evidence-authoring")]
        "dsv-selection-attest" => match cmd_dsv_selection_attest(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "selection-validation")]
        "dsv-selection-validate" => match cmd_dsv_selection_validate(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(not(feature = "selection-validation"))]
        "dsv-selection-validate" => {
            eprintln!(
                "dsv-selection-validate requires depth-lab built with --features selection-validation"
            );
            ExitCode::from(2)
        }
        #[cfg(feature = "selection-validation")]
        "camera-discovery-config-validate" => match cmd_camera_discovery_config_validate(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(not(feature = "selection-validation"))]
        "camera-discovery-config-validate" => {
            eprintln!(
                "camera-discovery-config-validate requires depth-lab built with --features selection-validation"
            );
            ExitCode::from(2)
        }
        #[cfg(feature = "selection-validation")]
        "camera-discovery-phase-validate" => match cmd_camera_discovery_phase_validate(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(not(feature = "selection-validation"))]
        "camera-discovery-phase-validate" => {
            eprintln!(
                "camera-discovery-phase-validate requires depth-lab built with --features selection-validation"
            );
            ExitCode::from(2)
        }
        "perf-certification-check" => match cmd_perf_certification_check(&m) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        "perf-certification-manifest-validate" => {
            match cmd_perf_certification_manifest_validate(&m) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
        #[cfg(feature = "exr-out")]
        "materialize" => match cmd_materialize(&m) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "exr-out")]
        "deliver" => match cmd_deliver(&m) {
            Ok(true) => ExitCode::SUCCESS,
            // 缺件 ≠ 工具挂了。前者要去补输入，后者要修代码。
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(feature = "push")]
        "push" => match cmd_push(&m) {
            Ok(true) => ExitCode::SUCCESS,
            // 传完但没传全 ≠ 工具挂了：前者要重跑，后者要修
            Ok(false) => ExitCode::from(1),
            Err(e) => {
                eprintln!("错误: {e}");
                ExitCode::from(2)
            }
        },
        #[cfg(not(feature = "push"))]
        "push" => {
            eprintln!("push 需要用 --features push 构建 depth-lab");
            ExitCode::from(2)
        }
        #[cfg(not(feature = "exr-out"))]
        "materialize" => {
            eprintln!("materialize 需要用 --features exr-out 构建 depth-lab");
            ExitCode::from(2)
        }
        other => {
            eprintln!("未知子命令 `{other}`\n{}", usage());
            ExitCode::from(2)
        }
    }
}

#[cfg(all(test, feature = "exr-out"))]
mod tests {
    use std::collections::BTreeMap;

    #[cfg(feature = "push")]
    use super::cmd_push;
    use super::{
        FALLOUT4_CAMERA_EVIDENCE, FALLOUT4_PRODUCTION_BINDING, apply_clip_plan, cmd_deliver,
        parse_args, verify_fallout4_camera_authorization,
    };

    fn hybrid_camera_row(present_seq: u64) -> depth_lab::CameraPoseRow {
        use depth_lab::camera::{Mat4, TranslationSlot, intrinsics_from_projection};
        use depth_lab::{
            CameraBinding, CameraPoseProvenance, CameraPoseSource, StreamIdentity,
            fallout_render_rotation_from_basis,
        };

        let binding: CameraBinding = serde_json::from_slice(FALLOUT4_PRODUCTION_BINDING).unwrap();
        let render_basis: Mat4 = [
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let projection: Mat4 = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 16.0 / 9.0, 0.0, 0.0],
            [0.0, 0.0, 1.001, 1.0],
            [0.0, 0.0, -0.1, 0.0],
        ];
        let identity = StreamIdentity {
            session_id: "session-fo4".into(),
            run_id: "r0000".into(),
            generation: 7,
        };
        depth_lab::CameraPoseRow {
            schema: depth_lab::CAMERA_POSE_INDEX_SCHEMA_V1.into(),
            identity,
            present_seq,
            present_qpc: 10_000 + present_seq as i64,
            source: CameraPoseSource::ResolvedRenderCameraHybrid,
            provenance: CameraPoseProvenance::ResolvedRenderCameraHybrid {
                binding_id: binding.binding_id,
                binding_version: binding.binding_version,
                profile_id: binding.profile_id,
                profile_version: binding.profile_version,
                executable_sha256: binding.executable_sha256,
                dsv_id: "main-scene-v1".into(),
                render_width: binding.render_width,
                render_height: binding.render_height,
                game_camera: binding.game_camera.unwrap(),
                render_basis_source: binding.view_sources[0],
                render_basis_matrix: Box::new(render_basis),
                projection_source: binding.projection_sources[0],
                projection_matrix: Box::new(projection),
            },
            camera_position: [1.0, 2.0, 3.0],
            camera_rotation_quaternion: fallout_render_rotation_from_basis(
                &render_basis,
                TranslationSlot::Row3,
            )
            .unwrap()
            .map(f64::from),
            camera_intrinsics: intrinsics_from_projection(&projection, 1920.0, 1080.0),
            camera_speed: None,
        }
    }

    fn hybrid_pose(camera: &depth_lab::CameraPoseRow) -> depth_lab::FinalPoseRow {
        depth_lab::FinalPoseRow {
            present_seq: camera.present_seq,
            session_frame: camera.present_seq,
            camera_identity: depth_lab::FinalCameraIdentity::from(&camera.identity),
            camera_source: camera.source,
            time: "2026-08-11 00:00:00.000".into(),
            fps: 30.0,
            camera_position: camera.camera_position,
            camera_rotation_quaternion: camera.camera_rotation_quaternion,
            camera_follow_offset: [0.0; 3],
            camera_intrinsics: camera.camera_intrinsics,
            camera_speed: [0.0; 3],
            player_position: [0.0; 3],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: [0.0; 3],
            mouse_x: 0.5,
            mouse_y: 0.5,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            key_code: Vec::new(),
        }
    }

    fn write_camera_index(rows: &[depth_lab::CameraPoseRow]) -> tempfile::NamedTempFile {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        for row in rows {
            serde_json::to_writer(&mut file, row).unwrap();
            writeln!(file).unwrap();
        }
        file
    }

    fn pair(session_frame: u64, duplicate: bool) -> depth_lab::pairing::FramePair {
        depth_lab::pairing::FramePair {
            session_frame,
            segment_index: 0,
            segment_frame: session_frame as u32,
            source_qpc: session_frame as i64,
            duplicate,
            camera_mode: depth_lab::pairing::CameraMode::Player,
            present_seq: Some(session_frame),
            depth: None,
            depth_absent_reason: None,
        }
    }

    #[test]
    fn clip_plan_filters_duplicates_and_preserves_selected_order() {
        let plan = depth_lab::clip_plan::ClipPlan {
            schema: depth_lab::clip_plan::CLIP_PLAN_SCHEMA_V1.into(),
            target_frames: 2,
            achieved_frames: 2,
            met_target: true,
            start_session_frame: 0,
            end_session_frame: 2,
            selected: vec![0, 2],
            drops: vec![depth_lab::clip_plan::PlannedDrop {
                session_frame: 1,
                reason: depth_lab::clip_plan::DropReason::Duplicate,
            }],
            duplicate_drops: 1,
            realign_drops: 0,
            watermark: depth_lab::pipeline::WatermarkStats::default(),
        };
        let selected = apply_clip_plan(
            vec![pair(0, false), pair(1, true), pair(2, false)],
            &plan,
            &BTreeMap::from([(0, "main-scene-v1".into())]),
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|row| row.session_frame)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(selected.iter().all(|row| !row.duplicate));
        assert_eq!(selected[0].depth.as_ref().unwrap().depth_name, "depth_0");
        assert_eq!(
            selected[1].depth_absent_reason,
            Some(depth_lab::pairing::AbsentReason::OddFrameBySpec)
        );
    }

    #[test]
    fn allow_nondeliverable_is_a_valueless_explicit_override() {
        let args = vec!["--allow-nondeliverable".to_string()];
        let parsed = parse_args(&args).unwrap();
        assert_eq!(
            parsed.get("allow-nondeliverable").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn fallout4_camera_gate_accepts_the_fixed_egeo_authorization_and_matching_pose() {
        let camera = hybrid_camera_row(17);
        depth_lab::validate_camera_pose_row(&camera).unwrap();
        let pose = hybrid_pose(&camera);
        let index = write_camera_index(&[camera]);

        verify_fallout4_camera_authorization(
            &[&pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap();
    }

    #[test]
    fn fallout4_camera_gate_rejects_unapproved_or_stale_egeo_evidence() {
        let camera = hybrid_camera_row(17);
        let pose = hybrid_pose(&camera);
        let index = write_camera_index(&[camera]);
        let mut evidence: serde_json::Value =
            serde_json::from_slice(FALLOUT4_CAMERA_EVIDENCE).unwrap();

        for (pointer, replacement, expected) in [
            (
                "/schema",
                serde_json::json!("fallout4-render-camera-promotion-evidence/v0"),
                "schema",
            ),
            (
                "/e_geo/schema",
                serde_json::json!("camera-egeo-validation/v2"),
                "E-geo v3",
            ),
            ("/e_geo/passed", serde_json::json!(false), "E-geo v3"),
            (
                "/e_geo/production_binding_authorized",
                serde_json::json!(false),
                "E-geo v3",
            ),
            (
                "/production_binding/sha256",
                serde_json::json!(format!("sha256:{}", "0".repeat(64))),
                "不匹配",
            ),
        ] {
            let old = evidence.pointer(pointer).unwrap().clone();
            *evidence.pointer_mut(pointer).unwrap() = replacement;
            let bytes = serde_json::to_vec(&evidence).unwrap();
            let error = verify_fallout4_camera_authorization(
                &[&pose],
                Some(index.path().to_str().unwrap()),
                &bytes,
                FALLOUT4_PRODUCTION_BINDING,
            )
            .unwrap_err();
            assert!(error.contains(expected), "{pointer}: {error}");
            *evidence.pointer_mut(pointer).unwrap() = old;
        }
    }

    #[test]
    fn fallout4_camera_gate_rejects_missing_mismatched_or_cross_identity_rows() {
        let camera = hybrid_camera_row(17);
        let pose = hybrid_pose(&camera);

        let error = verify_fallout4_camera_authorization(
            &[&pose],
            None,
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("--camera-index"), "{error}");

        let missing = write_camera_index(&[hybrid_camera_row(18)]);
        let error = verify_fallout4_camera_authorization(
            &[&pose],
            Some(missing.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("缺少交付 pose"), "{error}");

        let mut other_identity = hybrid_camera_row(18);
        other_identity.identity.generation += 1;
        let crossed = write_camera_index(&[camera.clone(), other_identity]);
        let error = verify_fallout4_camera_authorization(
            &[&pose],
            Some(crossed.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("跨 stream identity"), "{error}");

        let mut wrong_pose = pose.clone();
        wrong_pose.camera_position[0] += 1.0;
        let index = write_camera_index(&[camera]);
        let error = verify_fallout4_camera_authorization(
            &[&wrong_pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("行不一致"), "{error}");
    }

    #[test]
    fn fallout4_camera_gate_rejects_wrong_source_and_binding() {
        let camera = hybrid_camera_row(17);
        let pose = hybrid_pose(&camera);

        let mut wrong_source = camera.clone();
        wrong_source.source = depth_lab::CameraPoseSource::ResolvedMainCameraHook;
        let index = write_camera_index(&[wrong_source]);
        let error = verify_fallout4_camera_authorization(
            &[&pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("无效"), "{error}");

        let mut wrong = camera.clone();
        let depth_lab::CameraPoseProvenance::ResolvedRenderCameraHybrid {
            binding_version, ..
        } = &mut wrong.provenance
        else {
            unreachable!();
        };
        *binding_version = "999".into();
        let index = write_camera_index(&[wrong]);
        let error = verify_fallout4_camera_authorization(
            &[&pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("正式 CameraBinding"), "{error}");
    }

    #[test]
    fn fallout4_camera_gate_rejects_unverified_and_mixed_executable_hashes() {
        let mut camera = hybrid_camera_row(17);
        let depth_lab::CameraPoseProvenance::ResolvedRenderCameraHybrid {
            executable_sha256, ..
        } = &mut camera.provenance
        else {
            unreachable!();
        };
        *executable_sha256 = format!("sha256:{}", "f".repeat(64));
        let pose = hybrid_pose(&camera);
        let index = write_camera_index(&[camera.clone()]);
        let error = verify_fallout4_camera_authorization(
            &[&pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("正式 CameraBinding"), "{error}");

        let camera = hybrid_camera_row(17);
        let pose = hybrid_pose(&camera);
        let mut other = hybrid_camera_row(18);
        let depth_lab::CameraPoseProvenance::ResolvedRenderCameraHybrid {
            executable_sha256, ..
        } = &mut other.provenance
        else {
            unreachable!();
        };
        *executable_sha256 = format!("sha256:{}", "e".repeat(64));
        let other_pose = hybrid_pose(&other);
        let index = write_camera_index(&[camera, other]);
        let error = verify_fallout4_camera_authorization(
            &[&pose, &other_pose],
            Some(index.path().to_str().unwrap()),
            FALLOUT4_CAMERA_EVIDENCE,
            FALLOUT4_PRODUCTION_BINDING,
        )
        .unwrap_err();
        assert!(error.contains("多个实际 EXE hash"), "{error}");
    }

    #[cfg(feature = "push")]
    #[test]
    fn push_blockers_stop_before_r2_credentials_or_network() {
        let capture =
            std::env::temp_dir().join(format!("dm-push-capture-blocker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&capture);
        std::fs::create_dir_all(&capture).unwrap();
        std::fs::write(
            capture.join("depth-index.jsonl"),
            r#"{"present_seq":0,"dsv_id":"main-scene-v1","width":1,"height":1,"path":"raw/missing.exr"}"#,
        )
        .unwrap();

        let mut args = BTreeMap::new();
        args.insert("dir".into(), capture.to_string_lossy().into_owned());
        args.insert("bucket".into(), "must-not-be-contacted".into());
        let error = cmd_push(&args).unwrap_err();
        assert!(error.contains("拒绝上传"), "unexpected push error: {error}");

        let _ = std::fs::remove_dir_all(capture);
    }

    #[test]
    fn capture_blockers_stop_before_any_conversion_or_output_creation() {
        let root =
            std::env::temp_dir().join(format!("dm-deliver-capture-blocker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let capture = root.join("capture");
        let out = root.join("out");
        std::fs::create_dir_all(&capture).unwrap();
        // 没有 projection 的旧行会解析成显式 unknown。raw 文件故意不存在：
        // 如果命令错误地继续转换，这个测试会从 Ok(false) 变成读文件失败。
        std::fs::write(
            capture.join("depth-index.jsonl"),
            r#"{"present_seq":0,"dsv_id":"main-scene-v1","width":1,"height":1,"path":"raw/missing.exr"}"#,
        )
        .unwrap();
        let pairs = root.join("frame-pairs.jsonl");
        std::fs::write(
            &pairs,
            r#"{"session_frame":0,"segment_index":0,"segment_frame":0,"source_qpc":0,"duplicate":false,"camera_mode":"player","present_seq":0,"depth":{"present_seq":0,"depth_name":"depth_0","dsv_id":"main-scene-v1","inherited":false}}"#,
        )
        .unwrap();

        let mut args = BTreeMap::new();
        args.insert("capture".into(), capture.display().to_string());
        args.insert("pairs".into(), pairs.display().to_string());
        args.insert("out".into(), out.display().to_string());
        args.insert("clip-id".into(), "clip-0".into());
        assert_eq!(cmd_deliver(&args), Ok(false));
        assert!(!out.exists(), "采集侧阻塞时不该留下交付半成品");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fallout4_camera_gate_stops_before_output_creation() {
        let root = std::env::temp_dir().join(format!(
            "dm-deliver-camera-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let capture = root.join("capture");
        let out = root.join("out");
        std::fs::create_dir_all(&capture).unwrap();
        let projection = depth_lab::capture_timing::IndexProjection::observed(
            depth_lab::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(1.0),
        );
        let capture_row = serde_json::json!({
            "schema": depth_lab::capture_timing::INDEX_SCHEMA_V3,
            "present_seq": 17,
            "dsv_id": "main-scene-v1",
            "width": 1280,
            "height": 720,
            "path": "raw/missing.exr",
            "projection": projection,
        });
        std::fs::write(
            capture.join("depth-index.jsonl"),
            format!("{}\n", serde_json::to_string(&capture_row).unwrap()),
        )
        .unwrap();
        let pairs = root.join("frame-pairs.jsonl");
        std::fs::write(
            &pairs,
            r#"{"session_frame":0,"segment_index":0,"segment_frame":0,"source_qpc":1,"duplicate":false,"camera_mode":"player","present_seq":17,"depth":{"present_seq":17,"depth_name":"depth_0","dsv_id":"main-scene-v1","inherited":false}}"#,
        )
        .unwrap();
        let camera = hybrid_camera_row(17);
        let mut pose = hybrid_pose(&camera);
        pose.session_frame = 0;
        let pose_path = root.join("pose.jsonl");
        std::fs::write(
            &pose_path,
            format!("{}\n", serde_json::to_string(&pose).unwrap()),
        )
        .unwrap();

        let args = BTreeMap::from([
            ("capture".into(), capture.display().to_string()),
            ("pairs".into(), pairs.display().to_string()),
            ("out".into(), out.display().to_string()),
            ("clip-id".into(), "clip-0".into()),
            ("pose".into(), pose_path.display().to_string()),
        ]);
        let error = cmd_deliver(&args).unwrap_err();
        assert!(error.contains("--camera-index"), "{error}");
        assert!(!out.exists(), "相机授权失败时不该留下交付半成品");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deliver_rejects_projection_overrides() {
        let mut args = BTreeMap::new();
        args.insert("capture".into(), "unused-capture".into());
        args.insert("pairs".into(), "unused-pairs".into());
        args.insert("out".into(), "unused-out".into());
        args.insert("clip-id".into(), "clip-0".into());
        args.insert("near".into(), "0.1".into());

        let error = cmd_deliver(&args).unwrap_err();
        assert!(error.contains("不再接受 --near"), "{error}");
        assert!(error.contains("depth-index.jsonl"), "{error}");
    }
}
