"""形态 b 生成器（docs/69 §8.3 俯视伴飞）：自检逐字复刻 QC 三条判据 + 构造余量。

QC 侧独立复核见 `engine-data/qc/tests/test_cam4_topdown.py`。这里盯生成侧那一半：
- `metrics.cam4_topdown_check` 与 QC `_check_cam4_topdown` 判据**同向**——用 QC 测试
  同款的正/反例数值（垂直缓漂 41.6m 过、45° 斜视/6.6m 低空/50m 脱队挂）；
- 生成的 clip 不仅过线、且带构造余量（#427 教训：贴线过的自检等于没自检）；
- 粉色脉冲窗按 QC `plan.pink_honesty` 的原始算法（窗起帧 yaw 基底）逐窗复算；
- 初始帧豁免只豁免 lane 4，且缺省 timewarp 的 plan.json 输出逐字不变。
"""

import math

import pytest

from trajgen import cam4 as cam4mod
from trajgen import metrics as met
from trajgen.export import frame_records
from trajgen.mathutil import quat_forward, quat_from_yaw_elev_roll, vnorm, vsub
from trajgen.planner import FPS, QuotaTracker, generate
from trajgen.primitives import PINK
from trajgen.scene import synthetic

DT = 1.0 / FPS
N = 1800


def _gen(seed, mode="topdown"):
    return generate(synthetic("open_plaza", 1), seed=seed, quota=QuotaTracker(),
                    cam4_mode=mode)


@pytest.fixture(scope="module")
def plan():
    """夹具钉死 seed=1（与 test_plan_export 同纪律：不搜种子，过不了就该红；
    选 1 不选 0 的跨平台缘由见那边 `_plan` 的 docstring——两形态共用前三路，
    seed 1 在 macOS/glibc 上逐位同结果、一次过闸）。"""
    p = _gen(1)
    assert p.ok, "seed=1 topdown 未过自检：%s" % p.reasons
    return p


# ---------------------------------------------------------------- 判据同向（QC 同款数值）

def _straight_guide(y=1.6):
    return [(i * DT * 1.0, y, 0.0) for i in range(N)]


def _others():
    g = _straight_guide()
    return [g, [(0.0, 1.7, 2.0)] * N, [(0.0, 1.8, -2.0)] * N]


def _cam4(y=41.6, z0=0.0, elev=-90.0):
    pos = [(5.0 + 0.5 * i * DT, y, z0 + 0.1 * i * DT) for i in range(N)]
    quats = [quat_from_yaw_elev_roll(0.0, elev, 0.0)] * N
    return pos, quats


def test_qc_valid_example_passes():
    """QC 正例同款：guide 上空 41.6m 斜向缓漂 + 垂直向下 → 三条全过。"""
    pos, quats = _cam4()
    chk = met.cam4_topdown_check(pos, quats, _others(), _straight_guide(), DT)
    assert chk.problems() == [], chk.to_dict()
    assert chk.pitch_max_deg < 1e-6
    assert chk.alt_min_m > 35.0


def test_qc_counterexamples_fail_the_same_way():
    """QC 反例同款数值，逐条同向：斜视 45° / 低空 6.6m / 脱队 z=50m。"""
    others, guide = _others(), _straight_guide()

    pos, quats = _cam4(elev=-45.0)
    probs = met.cam4_topdown_check(pos, quats, others, guide, DT).problems()
    assert any(p.startswith("topdown_pitch") for p in probs), probs

    pos, quats = _cam4(y=6.6)
    probs = met.cam4_topdown_check(pos, quats, others, guide, DT).problems()
    assert any(p.startswith("topdown_alt") for p in probs), probs

    pos, quats = _cam4(z0=50.0)
    probs = met.cam4_topdown_check(pos, quats, others, guide, DT).problems()
    assert any(p.startswith("topdown_route") for p in probs), probs


def test_route_polyline_convention_is_point_sampled():
    """折线口径逐字：0.2s 抽样**点**的最小距离，不是点到线段。

    guide 沿 +x 走 6 m/s 时抽样点间距 1.2m；停在两点正中间的相机到「线段」
    距离为 0、到「点」距离 0.6m。生成侧必须给出 QC 那个 0.6——两侧算法在
    这里分叉的话，贴线通过的自检在 QC 侧就是另一个数（#427 形态）。
    """
    guide = [(i * DT * 6.0, 1.6, 0.0) for i in range(N)]
    pos = [(3.6 + 0.6, 41.6, 0.0)] * N          # 抽样点在 x=3.6/4.8，正中 4.2
    quats = [quat_from_yaw_elev_roll(0.0, -90.0, 0.0)] * N
    chk = met.cam4_topdown_check(pos, quats, _others(), guide, DT)
    assert abs(chk.route_max_m - 0.6) < 1e-6


# ---------------------------------------------------------------- 生成的 clip

def test_topdown_plan_declares_mode(plan):
    d = plan.cam4_plan.to_dict()
    assert plan.cam4_plan.mode == "topdown"
    assert d["mode"] == "topdown"
    assert d["guide_cam"] in (1, 2, 3)


def test_timewarp_plan_dict_stays_verbatim():
    """缺省形态 plan.json 不写 mode 键——历史 clip 输出逐字不变（README 承诺）。"""
    d = cam4mod.Cam4Plan(1, 0.5, 2.0, 30.0, 2.0).to_dict()
    assert "mode" not in d
    assert cam4mod.Cam4Plan.mode == "timewarp"


def test_topdown_passes_with_construction_margin(plan):
    """三条判据不仅过线、且带构造余量——贴线过的自检等于没自检。"""
    chk = plan.cam4_topdown
    assert chk is not None and chk.problems() == []
    assert chk.pitch_over_frac == 0.0
    assert chk.pitch_max_deg <= 12.5          # 设计上界 9+3，门 15
    assert chk.alt_under_frac == 0.0
    assert chk.alt_min_m >= 10.5              # 确定性抬底 10.6−取整
    assert chk.route_p95_m <= 10.0            # 走廊偏移设计 ~2m，门 20
    assert chk.route_max_m <= 20.0            # 门 40


def test_selfcheck_number_equals_export_side(plan):
    """自检拿计划内存位姿算，QC 拿导出 json 算——对**导出精度**的位姿重算一遍，
    两个数必须在取整噪声内一致（两侧口径一致纪律的端到端形式）。"""
    gi = plan.cam4_plan.guide_cam - 1
    recs4 = frame_records(plan, 3)
    recs_g = frame_records(plan, gi)
    pos4 = [tuple(r["camera_position"]) for r in recs4]
    quats4 = [tuple(r["camera_rotation_quaternion"]) for r in recs4]
    pos_g = [tuple(r["camera_position"]) for r in recs_g]
    others = [[tuple(r["camera_position"]) for r in frame_records(plan, k)]
              for k in range(3)]
    chk = met.cam4_topdown_check(pos4, quats4, others, pos_g, DT)
    assert chk.problems() == []
    assert abs(chk.pitch_max_deg - plan.cam4_topdown.pitch_max_deg) < 0.01
    assert abs(chk.alt_min_m - plan.cam4_topdown.alt_min_m) < 0.001
    assert abs(chk.route_p95_m - plan.cam4_topdown.route_p95_m) < 0.001


def test_pink_windows_cover_all_six_and_are_honest(plan):
    """cam4 俯视位的 6 类粉色平移：覆盖齐全（plan.pink_coverage 对每一路判），
    且按 QC plan.pink_honesty 的原始算法逐窗复算（窗起帧 yaw 基底、净位移
    ≥0.8m、≥窗内路径长 50%）。"""
    tr = plan.tracks[3]
    assert {m.cls for m in tr.moves} == set(PINK)
    recs = frame_records(plan, 3)
    pos = [tuple(r["camera_position"]) for r in recs]
    for m in tr.moves:
        i0 = int(round(m.t0 * FPS))
        i1 = min(int(round(m.t1 * FPS)), len(pos) - 1)
        fx, _, fz = quat_forward(tuple(recs[i0]["camera_rotation_quaternion"]))
        yaw = math.atan2(fx, fz)
        fwd = (math.sin(yaw), 0.0, math.cos(yaw))
        right = (math.cos(yaw), 0.0, -math.sin(yaw))
        axis = {"translate_forward": fwd,
                "translate_back": (-fwd[0], 0.0, -fwd[2]),
                "translate_right": right,
                "translate_left": (-right[0], 0.0, -right[2]),
                "translate_up": (0.0, 1.0, 0.0),
                "translate_down": (0.0, -1.0, 0.0)}[m.cls]
        d = vsub(pos[i1], pos[i0])
        disp = d[0] * axis[0] + d[1] * axis[1] + d[2] * axis[2]
        path = sum(vnorm(vsub(b, a)) for a, b in zip(pos[i0:i1 + 1], pos[i0 + 1:i1 + 1]))
        assert disp >= 0.8, (m.cls, disp)
        assert disp >= 0.5 * path, (m.cls, disp, path)


def test_init_gate_exempts_lane4_only(plan):
    """俯视位首帧垂直朝向对四路口径必超 15°；自检按 QC 豁免语义只判前三路。"""
    pose_map = {t.cam_id: (t.positions, t.quats) for t in plan.tracks}
    ang_all4, _ = met.initial_frame(pose_map)
    assert ang_all4 > 15.0, "cam4 首帧垂直向下，四路口径不该 ≤15°"
    assert plan.init_angle <= 15.0
    ang3, _ = met.initial_frame({k: v for k, v in pose_map.items() if k != 4})
    assert abs(plan.init_angle - ang3) < 1e-9


def test_topdown_is_deterministic():
    a, b = _gen(1), _gen(1)
    assert a.cam4_plan.to_dict() == b.cam4_plan.to_dict()
    assert a.tracks[3].positions[:20] == b.tracks[3].positions[:20]


def test_topdown_single_seed_pass_rate():
    """topdown 单种子过率闸（与 test_single_seed_pass_rate 同纪律）。"""
    ok = [_gen(s).ok for s in range(2, 6)]
    assert sum(ok) >= 3, "4 个种子只过 %d 条：%s" % (sum(ok), ok)
