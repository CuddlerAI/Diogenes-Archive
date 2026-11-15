"""Read-only reproduction of real Story observations; no calibration approval."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import math
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

OUT = Path(__file__).resolve().parent
CAPTURE = OUT.parents[3]
BASE = CAPTURE / "out/mc2-20260908"
sys.path.append(str(OUT.parent / "plot-dependencies"))
os.environ["MPLCONFIGDIR"] = str(OUT / "matplotlib-cache")
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


def binding(path):
    path = Path(path).resolve()
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


checked_files = {}
def bound_read(record):
    path = Path(record["path"]).resolve()
    data = path.read_bytes()
    actual = {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    assert actual == record, f"Input hash mismatch: {path.name}"
    checked_files[str(path)] = actual
    return data


def close(actual, expected, name, atol=1e-8):
    assert np.allclose(actual, expected, rtol=1e-11, atol=atol), f"Mismatch: {name}"


def stats(values):
    a = np.asarray(values, dtype=float)
    assert a.size and np.isfinite(a).all()
    return {"count": int(a.size), "rms": float(np.sqrt(np.mean(a * a))),
            "median": float(np.median(a)), "p95": float(np.percentile(a, 95)),
            "p99": float(np.percentile(a, 99)), "max": float(np.max(a))}


def compare_stats(actual, expected, name):
    assert actual["count"] == expected["count"]
    for key in actual.keys() - {"count"}:
        close(actual[key], expected[key], f"{name}.{key}")


def multiply(a, b):
    av, aw = np.asarray(a[:3]), a[3]
    bv, bw = np.asarray(b[:3]), b[3]
    return np.r_[aw * bv + bw * av + np.cross(av, bv), aw * bw - av @ bv]


def euler_quaternion(euler):
    a = np.radians(euler) / 2
    qx, qy, qz = ([math.sin(a[0]), 0, 0, math.cos(a[0])],
                  [0, math.sin(a[1]), 0, math.cos(a[1])],
                  [0, 0, math.sin(a[2]), math.cos(a[2])])
    return multiply(multiply(qz, qx), qy)


def independent_ray(observation):
    # Rotate world displacement with inverse quaternion via cross products.
    # Camera coordinates are right, forward, up; image y points downward.
    camera = observation["camera_before"]
    q = np.asarray(camera["quaternion"], dtype=float)
    close(np.linalg.norm(q), 1.0, "quaternion unit length")
    close(q, euler_quaternion(camera["euler_deg"]), "native Euler to qz*qx*qy")
    d = np.asarray(observation["point_world"]) - camera["position"]
    qv = -q[:3]
    local = d + 2 * (q[3] * np.cross(qv, d) + np.cross(qv, np.cross(qv, d)))
    assert local[1] > 2
    return np.array([local[0] / local[1], -local[2] / local[1]]), float(local[1])


def rotation_angle_degrees(a, b):
    a, b = np.asarray(a), np.asarray(b)
    chord = min(np.linalg.norm(a-b), np.linalg.norm(a+b))
    return float(math.degrees(4 * math.asin(min(1.0, chord / 2))))


def independent_fit(rays, pixels, centered=False):
    # Closed-form covariance fit, independent from analyzer's lstsq helper.
    if centered:
        principal = np.array([960.0, 540.0])
        f = np.sum(rays * (pixels - principal), axis=0) / np.sum(rays * rays, axis=0)
    else:
        dr, dp = rays - rays.mean(axis=0), pixels - pixels.mean(axis=0)
        f = np.sum(dr * dp, axis=0) / np.sum(dr * dr, axis=0)
        principal = pixels.mean(axis=0) - f * rays.mean(axis=0)
    assert np.isfinite(f).all() and (f > 0).all()
    return f, principal


def json_write(path, obj):
    with Path(path).open("x", encoding="utf-8") as handle:
        json.dump(obj, handle, indent=2, ensure_ascii=False, allow_nan=False)


def main():
    for name in ("review.json", "review.md", "coverage-error-focal.png"):
        assert not (OUT / name).exists(), "Refuse to overwrite review artifacts"
    analysis_record = binding(BASE / "story-projection-analysis-01/analysis.json")
    analysis = json.loads(bound_read(analysis_record))
    manifest = json.loads(bound_read(analysis["manifest"]))
    for key in ("analysis_source", "detector_source"):
        bound_read(analysis[key])
    bound_read(manifest["generator"])
    bound_read(manifest["reference_smoke_request"])
    assert analysis["approval_status"] == "draft"
    assert all(analysis[k] is False for k in ("editor_equivalence_verified", "delivery_eligible", "metric_scale_approved"))
    assert analysis["non_trainable"] is True
    assert analysis["native_uv_used_for_detection"] is False
    assert analysis["planned_uv_used_for_detection"] is False
    assert analysis["omitted_or_unresolved"] == []
    assert len(analysis["samples"]) == len(manifest["samples"]) == 128
    planned = {s["sample_id"]: s for s in manifest["samples"]}
    assert len(planned) == 128

    detector_path = Path(analysis["detector_source"]["path"])
    spec = importlib.util.spec_from_file_location("frozen_png_detector", detector_path)
    detector = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(detector)
    assert detector.PARAMETERS == analysis["detector_parameters"]
    rows, checks, max_motion = [], [], dict(position=0.0, quaternion=0.0, fov=0.0, native_uv=0.0)
    for index, row in enumerate(analysis["samples"]):
        sample = planned.pop(row["sample_id"])
        observation = json.loads(bound_read(row["observation"]))
        receipt = json.loads(bound_read(row["capture_receipt"]))
        png_bytes = bound_read(row["png"])
        for record in receipt["files"].values():
            bound_read(record)
        bound_read(receipt["helper"])
        request_bytes = bound_read(receipt["files"]["request.json"])
        request = json.loads(request_bytes)
        runtime = json.loads(bound_read(receipt["files"]["runtime-before.json"]))
        assert runtime["story_mode"] is True and runtime["online_session"] is False
        assert runtime["game_connected"] is True and runtime["framework_ready"] is True
        assert observation["controller_generation"] == runtime["controller_generation"] > 0
        assert receipt["approval_status"] == "draft"
        assert receipt["editor_equivalence_verified"] is False and receipt["delivery_eligible"] is False
        planned_request = dict(sample["request"])
        planned_request["path"] = str((Path(analysis["manifest"]["path"]).parent / planned_request["path"]).resolve())
        assert request_bytes == bound_read(planned_request)
        assert receipt["sample_id"] == row["sample_id"]
        assert receipt["manifest_sha256"] == analysis["manifest"]["sha256"]
        assert receipt["files"]["point.png"] == row["png"]
        assert receipt["files"]["observation.json"] == row["observation"]
        assert observation["request_sha256"] == receipt["files"]["request.json"]["sha256"]
        assert observation["request_path"] == receipt["files"]["request.json"]["path"]
        assert observation["output_sha256"] == row["png"]["sha256"]
        assert observation["output_bytes"] == len(png_bytes)
        assert observation["output_path"] == row["png"]["path"]
        assert observation["schema"] == "dm-gta-story-calibration-frame/1"
        assert observation["mock"] is False and observation["approval_status"] == "draft"
        assert observation["editor_equivalence_verified"] is False and observation["delivery_eligible"] is False
        assert observation["width"] == 1920 and observation["height"] == 1080
        assert observation["marker_mode"] == request["marker_mode"] == "world_cross"
        assert observation["projection_visible"] is True
        assert observation["nominal_focal_mm"] == request["nominal_focal_mm"] == row["nominal_focal_mm"]
        close(observation["requested_point_world"], request["point_world"], "requested world", atol=0)
        close(observation["point_world"], np.asarray(request["point_world"], dtype=np.float32).astype(float), "native float world", atol=0)
        before, after = observation["camera_before"], observation["camera_after"]
        differences = dict(position=float(np.linalg.norm(np.asarray(before["position"])-after["position"])),
                           quaternion=float(np.linalg.norm(np.asarray(before["quaternion"])-after["quaternion"])),
                           fov=abs(before["fov_deg"]-after["fov_deg"]),
                           native_uv=float(np.max(np.abs(np.asarray(before["screen_normalized"])-after["screen_normalized"]))))
        assert differences["position"] <= 0.001 and differences["quaternion"] <= 1e-5
        assert differences["fov"] <= 0.001 and differences["native_uv"] <= 1e-6
        for k, value in differences.items(): max_motion[k] = max(max_motion[k], value)
        close(before["position"], np.asarray(request["camera_position"], dtype=np.float32).astype(float), "requested camera", atol=0.001)
        assert rotation_angle_degrees(before["quaternion"], euler_quaternion(request["camera_euler_deg"])) <= 0.01
        close(before["fov_deg"], request["fov_deg"], "requested fov", atol=0.001)
        close(before["screen_normalized"], observation["screen_normalized"], "top native UV")
        assert before["projection_visible"] is after["projection_visible"] is True
        assert after["game_frame"] >= before["game_frame"]
        tr = observation["transport"]
        assert tr["protocol"] == 18 and tr["editor_writer_fence"] is False
        assert tr["request_id"] > 0
        assert tr["request_id"] == tr["pixel_ready_id"] == tr["observer_release_id"] == tr["ack_id"]
        assert tr["generation_echo"] == tr["pixel_ready_generation_echo"] > 0
        assert before["qpc"] <= tr["published_qpc"] <= tr["ready_observed_qpc"] <= after["qpc"] <= tr["ack_observed_qpc"]
        assert observation["settled_game_ticks"] >= 12 and observation["consecutive_stable_ticks"] >= 4
        assert observation["camera_restoration"]["private_camera_destroyed"] is True
        assert observation["camera_restoration"]["gameplay_restore_requested"] is True
        # Reproduce the same frozen detector; it receives only actual PNG bytes.
        detection = detector.detect(png_bytes)
        assert detection["status"] == "detected_unique" and row["used_in_fit"] is True
        close(detection["center_px"], row["pixel"], "PNG-only center reproduction")
        rays, depth = independent_ray(observation)
        close(rays, row["ray"], "independent inverse quaternion ray")
        close(depth, row["depth_units"], "independent optical depth")
        native_px = np.array(observation["screen_normalized"]) * [1920, 1080]
        close(native_px, row["native_pixel"], "native UV edge-origin conversion")
        error = float(np.linalg.norm(native_px - row["pixel"]))
        close(error, row["native_to_pixel_error_px"], "native pixel error")
        rows.append(dict(sample_id=row["sample_id"], nominal_focal_mm=row["nominal_focal_mm"],
                         pixel=row["pixel"], native_pixel=native_px.tolist(), ray=rays.tolist(), depth_units=depth,
                         pitch_deg=before["euler_deg"][0], native_error_px=error))
        if (index + 1) % 32 == 0: print(f"Verified actual PNG, bindings, camera and transport: {index+1}/128", flush=True)
    assert not planned

    groups, centered_errors = [], []
    for focal in (18, 24, 35, 50):
        group = [r for r in rows if r["nominal_focal_mm"] == focal]
        assert len(group) == 32
        assert all(min(abs(r["pitch_deg"]-target) for target in (30, 45)) <= 0.01 for r in group)
        assert all(sum(abs(r["pitch_deg"]-target) <= 0.01 for r in group) == 16 for target in (30, 45))
        rays = np.array([r["ray"] for r in group])
        pixels = np.array([r["pixel"] for r in group])
        f, center = independent_fit(rays, pixels)
        fit_errors = np.linalg.norm(rays * f + center - pixels, axis=1)
        loo = []
        for i in range(32):
            keep = np.arange(32) != i
            lf, lc = independent_fit(rays[keep], pixels[keep])
            loo.append(float(np.linalg.norm(rays[i] * lf + lc - pixels[i])))
        cf, cc = independent_fit(rays, pixels, centered=True)
        ce = np.linalg.norm(rays * cf + cc - pixels, axis=1)
        centered_errors.extend(ce.tolist())
        expected = next(g for g in analysis["per_focal"] if g["nominal_focal_mm"] == focal)
        close([*f, *center], [expected["pixel_intrinsics_with_fitted_principal_point"][k] for k in ("fx", "fy", "cx", "cy")], "free K")
        compare_stats(stats(fit_errors), expected["fit_error_px"], "free fit errors")
        compare_stats(stats(loo), expected["leave_one_out_error_px"], "leave one out errors")
        compare_stats(stats([r["native_error_px"] for r in group]), expected["native_to_pixel_error_px"], "focal native errors")
        spread = (np.ptp(pixels, axis=0) / [1920, 1080]).tolist()
        close(spread, expected["measured_spread_fraction"], "measured coverage")
        groups.append(dict(nominal_focal_mm=focal, count=32, fitted_fx_px=float(f[0]), fitted_fy_px=float(f[1]),
                           fitted_cx_px=float(center[0]), fitted_cy_px=float(center[1]),
                           fit_error_px=stats(fit_errors), leave_one_out_error_px=stats(loo),
                           centered_error_px=stats(ce), centered_focal_px=cf.tolist(),
                           measured_spread_fraction=spread, measured_depth_units_range=[min(r["depth_units"] for r in group), max(r["depth_units"] for r in group)]))
    native_stats, centered_stats = stats([r["native_error_px"] for r in rows]), stats(centered_errors)
    compare_stats(native_stats, analysis["native_to_independent_pixel_error_px"], "global native errors")
    close(centered_stats["rms"], analysis["existing_centered_solver_diagnostic"]["reprojection"]["error_px"], "global centered RMS")
    close(centered_stats["max"], analysis["existing_centered_solver_diagnostic"]["reprojection"]["max_px"], "global centered max")
    assert analysis["numerical_sampling_criteria_met"] is True
    assert centered_stats["rms"] <= 1 and native_stats["p95"] <= 1 and native_stats["p99"] <= 2

    # All scatter points are measured PNG centers or actual measured residuals.
    # Coverage sites intentionally overplot: no jitter, fabricated points, or fitted warp.
    colors = ["#1677b8", "#da7133", "#229970", "#8054b8"]
    plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 10, "axes.spines.top": False,
                         "axes.spines.right": False, "savefig.facecolor": "white"})
    fig = plt.figure(figsize=(14, 8.5))
    grid = fig.add_gridspec(2, 4, height_ratios=[1, 1.25], hspace=0.55, wspace=0.35)
    for i, (group, color) in enumerate(zip(groups, colors)):
        ax = fig.add_subplot(grid[0, i])
        data = [r for r in rows if r["nominal_focal_mm"] == group["nominal_focal_mm"]]
        pixels = np.array([r["pixel"] for r in data])
        ax.scatter(pixels[:, 0], pixels[:, 1], s=30, color=color, alpha=0.38, edgecolors="none")
        ax.set(xlim=(0, 1920), ylim=(1080, 0), xticks=[0, 960, 1920], yticks=[0, 540, 1080], xlabel="Image x (px)",
               title=f"{group['nominal_focal_mm']} mm label · 32 observations")
        if i == 0: ax.set_ylabel("Image y (px, downward)")
        ax.set_aspect("equal", adjustable="box")
        ax.grid(alpha=0.18)
    err_ax, focal_ax = fig.add_subplot(grid[1, :2]), fig.add_subplot(grid[1, 2:])
    for i, (group, color) in enumerate(zip(groups, colors)):
        data = [r for r in rows if r["nominal_focal_mm"] == group["nominal_focal_mm"]]
        indexes = np.arange(i * 32 + 1, (i + 1) * 32 + 1)
        err_ax.scatter(indexes, [r["native_error_px"] for r in data], color=color, s=18,
                       label=f"{group['nominal_focal_mm']} mm label")
    err_ax.axhline(1, color="#777777", linestyle="--", linewidth=1, label="1 px reference")
    err_ax.set(xlim=(0, 129), ylim=(0, 1.12), xlabel="Actual sample index", ylabel="Euclidean pixel error (px)",
               title="Native projection vs independently detected PNG center")
    err_ax.grid(alpha=0.18)
    err_ax.text(0.03, 0.12, f"RMS {native_stats['rms']:.4f} px\nP95 {native_stats['p95']:.4f} · max {native_stats['max']:.4f} px", transform=err_ax.transAxes, fontsize=11)
    err_ax.legend(loc="lower right", fontsize=8, ncol=2, frameon=False)
    focal_labels = [g["nominal_focal_mm"] for g in groups]
    focal_ax.plot(focal_labels, [g["fitted_fx_px"] for g in groups], "o", color="#172e46", markersize=8, label="Fitted fx")
    focal_ax.plot(focal_labels, [g["fitted_fy_px"] for g in groups], "x", color="#d56b34", markersize=9, markeredgewidth=1.8, label="Fitted fy")
    for g in groups:
        focal_ax.annotate(f"{g['fitted_fx_px']:.2f} / {g['fitted_fy_px']:.2f}",
                          (g["nominal_focal_mm"], g["fitted_fx_px"]), xytext=(0, 15), textcoords="offset points", ha="center", fontsize=9)
    focal_ax.set(xlim=(14, 54), ylim=(630, 2490), xticks=focal_labels, xlabel="Requested nominal focal label (mm)",
                 ylabel="Focal length fitted from real pixels (px)", title="Pixel focal fit · fx / fy")
    focal_ax.grid(alpha=0.18)
    focal_ax.legend(loc="lower right", frameon=False)
    fig.suptitle("128 real Story Mode observations · independent review · DRAFT", fontsize=19, x=0.055, ha="left", y=0.985)
    fig.text(0.055, 0.933, "Coverage: observed PNG centers only; repeated positions overplot without jitter. 2 camera tilts × 2 depths × 8 sites per focal group.", fontsize=10, color="#42566a")
    fig.subplots_adjust(top=0.875, bottom=0.15, left=0.06, right=0.97)
    fig.text(0.055, 0.065, f"Centered-principal-point reprojection RMS: {centered_stats['rms']:.4f} px. 18 mm leave-one-out maximum: {groups[0]['leave_one_out_error_px']['max']:.4f} px.", fontsize=10)
    fig.text(0.055, 0.035, "Story measurements only. Editor equivalence unverified; metric scale unapproved; delivery ineligible; non-trainable. Nominal labels do not measure physical filmback.", fontsize=9, color="#845d26")
    plot = OUT / "coverage-error-focal.png"
    fig.savefig(plot, dpi=180)
    plt.close(fig)

    # Recheck all bound source/input bytes after pixel analysis and plotting.
    for record in list(checked_files.values()): bound_read(record)
    report = {"schema": "dm-gta-story-projection-independent-review/1",
              "created_at": datetime.now(timezone.utc).isoformat(), "review_status": "PASS",
              "approval_status": "draft", "scope": "Story Mode measurements only",
              "editor_equivalence_verified": False, "metric_scale_approved": False,
              "delivery_eligible": False, "non_trainable": True, "approval_granted": False,
              "input_analysis": analysis_record, "review_source": binding(__file__),
              "fitting_reference_source": binding(CAPTURE / "gta_capture/calibration.py"),
              "input_file_bindings_verified": len(checked_files), "inputs_unchanged_after_review": True,
              "input_files": list(checked_files.values()), "count": len(rows), "per_focal": groups,
              "native_to_png_error_px": native_stats, "centered_reprojection_error_px": centered_stats,
              "before_after_max_observed_motion": max_motion, "all_transport_and_camera_checks_passed": True,
              "math_reproduction": "Independent inverse-quaternion vector rotation and closed-form covariance/centered fits; frozen PNG-only detector rerun unchanged.",
              "pixel_coordinate_convention": "Image-edge origin: native UV*[width,height]; detected column+0.5,row+0.5. No shift or image warp applied.",
              "limits": ["Actual pixels reproduce the stated numerical criteria, but no approval or Editor equivalence follows.",
                         "The same frozen PNG detector was rerun; this is not validation by a second detector algorithm.",
                         "The largest 18 mm leave-one-out residual is 1.01047 px; do not claim every cross-validation error is below 1 px.",
                         "Eight screen sites repeat across two depths and two tilts; points do not densely cover the full image or outer 20% borders.",
                         "Nominal-label equivalent filmback is model-conditioned, not an independently measured physical sensor dimension.",
                         "GTA world units are not independently approved real metres; no metric-scale approval.",
                         "Static native observations bracket each PNG; this is not an Editor writer generation fence.",
                         "No source capture, approval, gate or original analysis file was modified."],
              "figure": binding(plot), "observations": rows}
    md = f"""# Story 实测投影独立复核（草稿）

128/128 张真实 PNG、观测、请求与采集回执的哈希绑定通过；4 个名义焦距组（18/24/35/50 mm）各 32 点。逐张复跑原有仅接收 PNG 的检测器，独立使用四元数逆旋转和闭式最小二乘重算，数值与原分析一致。检测器复跑不等于第二种独立检测算法验证。

| 指标 | 实测复核值 |
|---|---:|
| Native 投影到 PNG 中心 RMS | {native_stats['rms']:.6f} px |
| Native 到 PNG P95 / 最大误差 | {native_stats['p95']:.6f} / {native_stats['max']:.6f} px |
| 中心固定的重投影 RMS / 最大误差 | {centered_stats['rms']:.6f} / {centered_stats['max']:.6f} px |
| 18 mm 留一法最大误差 | {groups[0]['leave_one_out_error_px']['max']:.6f} px |

全部采样的前后相机姿态、FOV 和 native UV 稳定，request/ready/release/ack 与 transport generation 一致。{len(checked_files)} 个独立输入文件的字节与 SHA-256 在计算前后核对不变。

像素约定为图像边缘原点：native UV 乘 1920/1080，与像素中心 column+0.5、row+0.5 比较；未额外平移或变形图像。图中只包含实际像素观测及其拟合统计，重复点直接重叠。每组仅 8 个屏幕位置 × 2 个深度 × 2 个倾角，横纵跨度约 60%，不是满画幅密集采样。

数值采样条件通过。18 mm 留一法有最大 1.01047 px 的残差，不能声称全部交叉验证误差都小于 1 px。图中的焦距是像素拟合量，名义 mm 标签和推导的等效 filmback 不构成物理传感器测量。

**范围仍为 Story 模式、draft、non_trainable。Editor 等价性未验证，实米尺度未批准，交付不具备资格；本复核不授予任何审批。** 原生相机观测包围静态 PNG 采样，不是 Editor generation fence。

原分析 SHA-256：`{analysis_record['sha256']}`。完整输入、图像和复核源码的 SHA-256 见 `review.json`。
"""
    with (OUT / "review.md").open("x", encoding="utf-8") as handle: handle.write(md)
    report["short_report"] = binding(OUT / "review.md")
    json_write(OUT / "review.json", report)
    print(json.dumps({"review_status": report["review_status"], "verified_inputs": len(checked_files),
                      "native_rms_px": native_stats["rms"], "centered_rms_px": centered_stats["rms"],
                      "output": str(OUT), "review_sha256": binding(OUT / "review.json")["sha256"]}), flush=True)


if __name__ == "__main__":
    main()
