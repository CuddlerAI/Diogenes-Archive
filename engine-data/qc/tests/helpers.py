"""测试共用：sys.path 引导 + 合成 clip 生成器。"""

import json
import math
import sys
from pathlib import Path

QC_ROOT = Path(__file__).resolve().parents[1]  # engine-data/qc
if str(QC_ROOT) not in sys.path:
    sys.path.insert(0, str(QC_ROOT))

FIXTURES = QC_ROOT.parent / "fixtures"

SYSINFO = {"gameProcessName": "ds", "x": 0, "y": 0,
           "width": 1920, "height": 1080, "recordDpi": 1.0}


def yaw_quat(deg):
    r = math.radians(deg) / 2.0
    return [0.0, math.sin(r), 0.0, math.cos(r)]


def _time_str(i):
    return f"2026-08-03 12:{(i // 1800):02d}:{(i // 30) % 60:02d}.{int((i % 30) * 1000 / 30):03d}"


def mc_frame(i, pos, quat, vel, cam1=False):
    f = {
        "time": _time_str(i),
        "fps": 30.0,
        "frame": i,
        "camera_position": [float(x) for x in pos],
        "camera_rotation_quaternion": [float(x) for x in quat],
        "camera_Follow Offset": [0.0, 2.5, -5.0],
        "camera_intrinsics": {"fx": 1080.0, "fy": 1080.0, "cx": 960.0, "cy": 540.0},
        "camera_speed": [float(x) for x in vel],
    }
    if cam1:
        f.update({
            "camera_rotation_oula": [0.0, 0.0, 0.0],
            "player_position": [pos[0], pos[1] - 1.4, pos[2]],
            "player_rotation_oula": [0.0, 0.0, 0.0],
            "player_rotation_quaternion": [float(x) for x in quat],
            "player_speed": [float(x) for x in vel],
            "mouse_x": 0.5, "mouse_y": 0.5,
            "mouse_dx": 0.0, "mouse_dy": 0.0,
            "keyCode": [87],
            "metric_scale": 1.0,
        })
    return f


def gen_cam_frames(n, p0, vel, yaw0=0.0, yaw_rate=0.0, cam1=False, positions=None):
    frames = []
    for i in range(n):
        t = i / 30.0
        pos = positions[i] if positions is not None else [p0[j] + vel[j] * t for j in range(3)]
        frames.append(mc_frame(i, pos, yaw_quat(yaw0 + yaw_rate * t), vel, cam1=cam1))
    return frames


def valid_mc_cams(n=1800):
    """4 路合法 MC：静态朝向 + 慢速运动，方向/速率/角速率互相错开保证解耦。"""
    return {
        "1": gen_cam_frames(n, [0.0, 1.6, 0.0], [1.0, 0.0, 0.0], yaw_rate=0.0, cam1=True),
        "2": gen_cam_frames(n, [2.0, 1.6, 0.0], [0.0, 0.0, 1.0], yaw_rate=-8.0),
        "3": gen_cam_frames(n, [0.0, 1.6, 2.0], [-2.5, 0.0, 0.0], yaw_rate=20.0),
        "4": gen_cam_frames(n, [2.0, 1.6, 2.0], [0.0, 0.5, 0.0], yaw_rate=0.0),
    }


def coupled_mc_cams(n=1800):
    """反例：四路完全同步运动（同方向同速同角速率）→ 解耦闸门应 FAIL。"""
    cams = {}
    for idx, k in enumerate("1234"):
        cams[k] = gen_cam_frames(n, [idx * 2.0, 1.6, 0.0], [1.0, 0.0, 0.0],
                                 yaw_rate=0.0, cam1=(k == "1"))
    return cams


def cam4_reparam_positions(n, p0, guide_vel, rho1, rho2, t_sw, delta):
    """第四路：guide 直线路径的时间重参数化（五次 smoothstep 过渡，docs/71 §6）。"""
    dt = 1.0 / 30.0
    s = 0.0
    out = []
    for i in range(n):
        t = i * dt
        out.append([p0[j] + guide_vel[j] * s for j in range(3)])
        tau = max(0.0, min(1.0, (t - t_sw) / delta))
        s5 = 6 * tau ** 5 - 15 * tau ** 4 + 10 * tau ** 3
        s += (rho1 + (rho2 - rho1) * s5) * dt
    return out


STRICT_GATE = ("min(O(A→B),O(B→A)) ≥ 0.30，前三路**三对全部**成立才计时长"
               "（严读，需求「共同存在」）；第四路不参与")
WIDE_GATE = "min(O(A→B),O(B→A)) ≥ 0.30，前三路至少一对成立即计时长；第四路不参与"


def covis_doc(ratio=0.62, n=121, gate=STRICT_GATE, declared=None,
              any_pair=None, with_curves=True):
    """按当前 trajgen（PR #462 之后）的 `covis.json` 形状造一份。

    默认三对曲线一致，于是严读 = 宽读 = `ratio`，声明值与重算值自洽。
    造反例时把 `declared` 拧成别的数，或把 `gate` 换成 `WIDE_GATE`。
    """
    hit = int(round(ratio * n))
    curves = {p: [(0.62 if k < hit else 0.10) for k in range(n)]
              for p in ("1-2", "1-3", "2-3")}
    real = hit / n
    doc = {
        "covis_ratio": real if declared is None else declared,
        "covis_ratio_any_pair": real if any_pair is None else any_pair,
        "overlap_min": 0.30,
        "gate": gate,
        "pair_min_overlap_ts": [[round(k * 0.5, 3), (0.62 if k < hit else 0.10)]
                                for k in range(n)],
    }
    if with_curves:
        doc["pair_curves"] = curves
    return doc


def write_mc_clip(root: Path, cams, extra=None):
    root.mkdir(parents=True, exist_ok=True)
    for k, frames in cams.items():
        (root / f"action_camera_{k}.json").write_text(
            json.dumps(frames), encoding="utf-8")
        (root / f"systeminfo_{k}.json").write_text(
            json.dumps(SYSINFO), encoding="utf-8")
    for name, obj in (extra or {}).items():
        (root / name).write_text(json.dumps(obj), encoding="utf-8")
    return root


# ---------------------------------------------------------------- sfm

def sfm_frame(i, pos, quat, vel, fps15=False):
    f = {
        "time": _time_str(i),
        "fps": 30.0,
        "frame": i,
        "camera_position": [float(x) for x in pos],
        "camera_rotation_quaternion": [float(x) for x in quat],
        "camera_follow_offset": [0.0, 0.6, -0.07],
        "camera_intrinsics": {"fx": 960.0, "fy": 960.0, "cx": 960.0, "cy": 540.0},
        "camera_speed": [float(x) for x in vel],
        "player_position": [pos[0], pos[1] - 0.6, pos[2]],
        "player_rotation_quaternion": [float(x) for x in quat],
        "player_speed": [float(x) for x in vel],
        "metric_scale": 1.0,
        "mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0,
        "keyCode": [87],
    }
    if fps15:
        f["depth_name"] = f"depth_{i}"
    return f


def valid_sfm_files(n=1800):
    vel = [1.0, 0.0, 0.5]
    f30 = [sfm_frame(i, [vel[0] * i / 30.0, 1.6, vel[2] * i / 30.0], yaw_quat(10.0), vel)
           for i in range(n)]
    f15 = [sfm_frame(i, [vel[0] * i / 30.0, 1.6, vel[2] * i / 30.0], yaw_quat(10.0), vel,
                     fps15=True)
           for i in range(0, n, 2)]
    return f30, f15


def write_sfm_clip(root: Path, f30, f15):
    root.mkdir(parents=True, exist_ok=True)
    (root / "action_camera_30fps.json").write_text(json.dumps(f30), encoding="utf-8")
    (root / "action_camera_15fps.json").write_text(json.dumps(f15), encoding="utf-8")
    (root / "systeminfo.json").write_text(json.dumps(SYSINFO), encoding="utf-8")
    return root


# ---------------------------------------------------------------- 断言辅助

def results_by_rule(report, rule):
    return [r for r in report.results if r.rule == rule]


def statuses(report, rule):
    return {r.status for r in results_by_rule(report, rule)}


def status_for(report, rule, target):
    for r in report.results:
        if r.rule == rule and r.target == target:
            return r.status
    return None


def fail_rules(report):
    return sorted({r.rule for r in report.results if r.status == "FAIL"})
