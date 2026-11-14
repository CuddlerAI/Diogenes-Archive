# R0 标定 runner:在真实交付包上标定 E-geo 噪声地板与 E1a-pose 估计器召回。
#
# **这是一次性标定 runner,不是可复用工具**:它读的是当时会话工作目录里的三个
# 交付包与一份外部 `convert_depth`,那些路径不在仓库里。留档是为了让 43 号文
# §5 的读数可追溯到产生它的代码,不是为了让人原样再跑一遍。
#
# 用法(要重跑必须自己给根目录):
#     DIOGENES_GEO_CALIB_ROOT=<含 wm-depth/ 与 fo4/ 的工作根> python3 geo_calibrate.py
#
# 产出 evidence/geo-calibration-report.json(本目录已随 PR 归档一份)。
import json, math, subprocess, sys, os
import numpy as np

S = os.environ.get("DIOGENES_GEO_CALIB_ROOT")
if not S:
    raise SystemExit(
        "geo_calibrate 需要 DIOGENES_GEO_CALIB_ROOT——它依赖的交付包与 convert_depth\n"
        "不在仓库里。归档读数见 evidence/geo-calibration-report.json。"
    )
sys.path.insert(0, f"{S}/wm-depth/ml")
sys.path.insert(0, f"{S}/fo4/work")
from diogenes_wm.eval.geo_probes import Pinhole, RelPose, e_geo, estimate_pose, reproject, yaw_of, _bilinear
from convert_depth import read_exr_half

PKGS = {
    "fo4_120s_01": dict(
        video=f"{S}/fo4/work/package/2026_08_06_FO4Commonwealth_120s_01/video.mp4",
        depth=f"{S}/fo4/work/delivery-120/depth",
        action=f"{S}/fo4/work/delivery-120/action_camera_30fps.json",
        fx1080=698.2612, n_frames=3600,
        bands=[(0.2, 5.0), (5.0, 20.0), (20.0, 100.0), (100.0, 5000.0)],
    ),
    "ets2_180s_01": dict(
        video=f"{S}/ets2v/work/video-3min.mp4",
        depth=f"{S}/ets2v/work/delivery-3min/depth",
        action=f"{S}/ets2v/work/delivery-3min/action_camera_30fps.json",
        fx1080=1345.87036, n_frames=5400,
        bands=[(0.2, 1.0), (1.0, 5.0), (5.0, 12.0), (12.0, 22.0)],
    ),
}

def frames_gray_720(video, n):
    """全部帧的 720p 灰度(uint8),流式读入。"""
    p = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-i", video, "-vf", "scale=1280:720", "-vsync", "0",
         "-f", "rawvideo", "-pix_fmt", "gray", "-"], stdout=subprocess.PIPE)
    buf = p.stdout.read(1280 * 720 * n)
    p.stdout.close(); p.wait()
    got = len(buf) // (1280 * 720)
    return np.frombuffer(buf[: got * 1280 * 720], dtype=np.uint8).reshape(got, 720, 1280)

def masked_e_geo(gt, gk, d, intr, rel, band, sign):
    uk, vk, valid = reproject(d, intr, rel, sign)
    sampled, inb = _bilinear(gk.astype(np.float64), uk, vk)
    m = valid & inb & (d >= band[0]) & (d < band[1])
    if m.sum() < 500: return None
    photo = float(np.mean(np.abs(sampled[m] - gt.astype(np.float64)[m])))
    base = float(np.mean(np.abs(gk.astype(np.float64)[m] - gt.astype(np.float64)[m])))
    return photo, base, float(m.mean())

def run_pkg(name, cfg):
    print(f"== {name}", flush=True)
    ac = json.load(open(cfg["action"]))
    pos = np.array([r["camera_position"] for r in ac])
    quat = np.array([r["camera_rotation_quaternion"] for r in ac])
    intr = Pinhole(cfg["fx1080"] * 2 / 3, cfg["fx1080"] * 2 / 3, 640.0, 360.0)
    frames = frames_gray_720(cfg["video"], cfg["n_frames"])
    n = len(frames)
    print("frames:", n, flush=True)

    def depth_of(t):
        return read_exr_half(f'{cfg["depth"]}/depth_{t}.exr').astype(np.float64)

    # 1) 约定自检:前 20 对上四种组合(v_sign × R 转置)取优
    combos = {}
    for sign in (-1.0, 1.0):
        for transpose in (False, True):
            imps = []
            for t in range(0, 40, 2):
                rel = RelPose.between(pos[t], quat[t], pos[t+2], quat[t+2])
                if transpose:
                    rel = RelPose(rel.rotation.T, -(rel.rotation.T @ rel.translation))
                d = depth_of(t)
                r = e_geo(frames[t], frames[t+2], d, intr, rel, sign)
                if np.isfinite(r.improvement): imps.append(r.improvement)
            combos[(sign, transpose)] = float(np.median(imps)) if imps else 0.0
    best = max(combos, key=combos.get)
    sign, transpose = best
    print("约定自检:", {str(k): round(v,3) for k,v in combos.items()}, "→ 选", best, flush=True)

    def rel_of(t, k=2):
        rel = RelPose.between(pos[t], quat[t], pos[t+k], quat[t+k])
        if transpose:
            rel = RelPose(rel.rotation.T, -(rel.rotation.T @ rel.translation))
        return rel

    # 2) E-geo 全量(每 2 对取 1)+ 分带
    agg = {"overall": []}
    for b in cfg["bands"]: agg[f"{b[0]}-{b[1]}m"] = []
    for t in range(0, n - 2, 4):
        d = depth_of(t)
        rel = rel_of(t)
        r = e_geo(frames[t], frames[t+2], d, intr, rel, sign)
        if np.isfinite(r.improvement): agg["overall"].append(r.improvement)
        for b in cfg["bands"]:
            mb = masked_e_geo(frames[t], frames[t+2], d, intr, rel, b, sign)
            if mb: agg[f"{b[0]}-{b[1]}m"].append(mb[1]/mb[0] if mb[0]>0 else np.inf)
    stats = {k: dict(n=len(v), p50=round(float(np.median(v)),3), p10=round(float(np.percentile(v,10)),3))
             for k,v in agg.items() if v}
    print("E-geo 改善比:", json.dumps(stats, ensure_ascii=False), flush=True)

    # 3) E1a-pose 召回:每 40 帧取一对,恒等初始化
    est_yaw, true_yaw, est_fwd, true_fwd = [], [], [], []
    for t in range(0, n - 2, 40):
        d = depth_of(t)
        true_rel = rel_of(t)
        est = estimate_pose(frames[t], frames[t+2], d, intr,
                            RelPose(np.eye(3), np.zeros(3)), iters=15, stride=4, v_up_sign=sign)
        est_yaw.append(yaw_of(est)); true_yaw.append(yaw_of(true_rel))
        est_fwd.append(est.translation[2]); true_fwd.append(true_rel.translation[2])
    def corr(a, b):
        a, b = np.array(a), np.array(b)
        if a.std() < 1e-9 or b.std() < 1e-9: return None
        return round(float(np.corrcoef(a, b)[0,1]), 3)
    pose_stats = dict(
        n=len(est_yaw),
        r_yaw=corr(est_yaw, true_yaw),
        r_fwd=corr(est_fwd, true_fwd),
        yaw_sign_agree=round(float(np.mean(np.sign(est_yaw)==np.sign(true_yaw))),3),
        yaw_mae_deg=round(float(np.mean(np.abs(np.degrees(np.array(est_yaw)-np.array(true_yaw))))),4),
    )
    print("E1a-pose 召回:", json.dumps(pose_stats), flush=True)
    return dict(convention=dict(v_sign=sign, rot_transposed=transpose,
                                probe=({str(k): round(v,3) for k,v in combos.items()})),
                e_geo=stats, pose=pose_stats)

out = {}
for name, cfg in PKGS.items():
    out[name] = run_pkg(name, cfg)
json.dump(out, open(f"{S}/wm-depth/geo-calibration-report.json", "w"), indent=1, ensure_ascii=False)
print("DONE")
