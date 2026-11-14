"""经典管线的粉色诚实性（QC plan.pink_honesty 的生成侧收口）。

三层各测各的：
1. `metrics.pink_honesty_check` 与 QC `check_pink_honesty` 判据同向（同款数值）；
2. `pink_honesty.enforce` 修正场：把坏声称修诚实、修正自身守 jerk 预算、
   相邻窗偏移携带不泄漏；
3. 端到端：生成的 clip 四路全部粉色声称诚实（QC #597 上线后这里一直是
   「自检全绿 QC 打回」的 #427 形态——修复前每条 clip 稳定 5-11 条 FAIL）。
"""

import math

import pytest

from trajgen import metrics as met
from trajgen import pink_honesty as ph
from trajgen.mathutil import quat_from_yaw_elev_roll, vnorm, vsub
from trajgen.planner import FPS, QuotaTracker, generate
from trajgen.primitives import PINK, Move
from trajgen.scene import synthetic

DT = 1.0 / FPS
Q_NORTH = quat_from_yaw_elev_roll(0.0, 0.0, 0.0)     # forward = +z


def _line(n, v, start=(0.0, 1.6, 0.0)):
    """匀速直线：v 为 (vx,vy,vz)。"""
    return [(start[0] + v[0] * i * DT, start[1] + v[1] * i * DT,
             start[2] + v[2] * i * DT) for i in range(n)]


@pytest.fixture(scope="module")
def plan():
    """夹具钉死 seed=1（与 test_plan_export 同纪律；seed 0 在 glibc 上不过闸的
    跨平台缘由见那边 `_plan` 的 docstring）。"""
    p = generate(synthetic("open_plaza", 1), seed=1, quota=QuotaTracker())
    assert p.ok, "seed=1 未过自检：%s" % p.reasons
    return p


# ---------------------------------------------------------------- 判据同向

def test_check_matches_qc_contract():
    n = 300
    quats = [Q_NORTH] * n
    mv = [Move("translate_forward", 1.0, 5.0, 0)]

    # 正例：沿 +z 直走 0.5 m/s → 4s 窗净位移 2m、占比 100%
    rows = met.pink_honesty_check(_line(n, (0.0, 0.0, 0.5)), quats, mv, FPS)
    assert rows[0].ok and abs(rows[0].disp_m - 2.0) < 1e-6

    # 反例 1：位移不足（0.15 m/s × 4s = 0.6 < 0.8）
    rows = met.pink_honesty_check(_line(n, (0.0, 0.0, 0.15)), quats, mv, FPS)
    assert not rows[0].ok

    # 反例 2：方向错位（走 +x 而声称 forward=+z：净轴向 0）
    rows = met.pink_honesty_check(_line(n, (0.5, 0.0, 0.0)), quats, mv, FPS)
    assert not rows[0].ok and abs(rows[0].disp_m) < 1e-6

    # 基底跟窗首帧 yaw 走：把相机转 90°（forward=+x），走 +x 就诚实了
    quats_east = [quat_from_yaw_elev_roll(90.0, 0.0, 0.0)] * n
    rows = met.pink_honesty_check(_line(n, (0.5, 0.0, 0.0)), quats_east, mv, FPS)
    assert rows[0].ok


# ---------------------------------------------------------------- 修正场

def _jerk_max(delta, dt=DT):
    worst = 0.0
    for j in range(3, len(delta)):
        jv = tuple(delta[j][a] - 3 * delta[j - 1][a] + 3 * delta[j - 2][a]
                   - delta[j - 3][a] for a in range(3))
        worst = max(worst, vnorm(jv) / dt ** 3)
    return worst


def test_enforce_repairs_short_window_within_jerk_budget():
    n = 900   # 30s
    quats = [Q_NORTH] * n
    pos = _line(n, (0.0, 0.0, 0.02))        # 近乎不动：声称的平移没发生
    mv = [Move("translate_forward", 8.0, 11.0, 0)]
    out = ph.enforce(pos, quats, mv, FPS, ground_y=0.0)
    rows = met.pink_honesty_check(out, quats, mv, FPS)
    assert rows[0].ok, rows[0]
    delta = [vsub(b, a) for a, b in zip(pos, out)]
    assert _jerk_max(delta) <= ph.JERK_BUDGET * 1.05
    # 窗后衰减：修正在下一段落收回（末端回到原轨迹）
    assert vnorm(delta[-1]) < 0.05


def test_enforce_carries_offset_across_adjacent_windows():
    """相邻窗之间没有衰减空间：偏移携带（常量偏移不改判据），两窗都得诚实，
    且位置序列无跳变（速度阶跃有界）。"""
    n = 1200
    quats = [Q_NORTH] * n
    pos = _line(n, (0.0, 0.0, 0.02))
    mv = [Move("translate_forward", 8.0, 10.5, 0),
          Move("translate_right", 10.5, 13.0, 1)]
    out = ph.enforce(pos, quats, mv, FPS, ground_y=0.0)
    rows = met.pink_honesty_check(out, quats, mv, FPS)
    assert all(r.ok for r in rows), rows
    vmax = max(vnorm(vsub(b, a)) / DT for a, b in zip(out, out[1:]))
    assert vmax < 3.0


def test_enforce_gives_up_on_hopeless_claim():
    """声称与运动严重相反（jerk 预算内救不回）：不硬掰，原样返回交给自检打回。"""
    n = 900
    quats = [Q_NORTH] * n
    pos = _line(n, (0.0, 0.0, -1.5))        # 全速反向
    mv = [Move("translate_forward", 8.0, 10.5, 0)]
    out = ph.enforce(pos, quats, mv, FPS, ground_y=-99.0)
    rows = met.pink_honesty_check(out, quats, mv, FPS)
    assert not rows[0].ok
    drift = max(vnorm(vsub(b, a)) for a, b in zip(pos, out))
    assert drift < 0.7      # 放弃的窗只有（缩过幅的）横向抑制，不产生大改形


def test_prelift_keeps_dive_above_eye_floor():
    """低空 translate_down：窗前预抬升让俯冲有得冲，且不破视高地板。"""
    n = 900
    quats = [Q_NORTH] * n
    pos = _line(n, (0.0, 0.0, 0.3), start=(0.0, 0.75, 0.0))   # 离地仅 0.75m
    mv = [Move("translate_down", 10.0, 13.0, 0)]
    out = ph.enforce(pos, quats, mv, FPS, ground_y=0.0)
    rows = met.pink_honesty_check(out, quats, mv, FPS)
    assert rows[0].ok, rows[0]
    assert min(p[1] for p in out) >= 0.5 - 1e-9


# ---------------------------------------------------------------- 端到端

def test_all_lanes_pink_claims_honest(plan):
    for tr in plan.tracks:
        rows = met.pink_honesty_check(tr.positions, tr.quats, tr.moves, FPS)
        assert {m.cls for m in tr.moves if m.cls in PINK} == set(PINK), \
            "cam%d 粉色覆盖缺项" % tr.cam_id
        bad = [r for r in rows if not r.ok]
        assert not bad, "cam%d 粉色声称不诚实：%s" % (tr.cam_id, bad)


def test_cam4_rho_contract_survives_pink_overlay(plan):
    """保速换向不得惊动变速契约：QC 同式的 ρ 反算三条照旧全过。"""
    chk = plan.cam4_check
    assert chk is not None
    assert chk.problems(plan.cam4_plan.t_sw, plan.cam4_plan.delta) == []


def test_translate_basis_is_lookat_yaw():
    """原语基底 = 对锚 look-at 的 yaw（声称口径），不是上一 move 末位 yaw。

    回归钉：基底若回退到 start.yaw，上一 move 是 pan 时实测轴错位可达 90°+
    （修复前 144 窗有 36 窗挂在这）。窗首帧导出 yaw 就是 look-at 方向，
    两处必须同源。
    """
    import random
    from trajgen.anchors import make_anchor, make_subject
    from trajgen.primitives import Ctx, Pose, build
    from trajgen.scene import synthetic as syn

    scene = syn("open_plaza", 1)
    rng = random.Random(7)
    subjects = [make_subject(scene, rng, 60.0, (0.0, 1.6, 0.0))]
    anchor = make_anchor(scene, rng, 60.0, subjects)
    ctx = Ctx(scene, anchor, subjects, None, rng, 60.0)
    # start 的 yaw 故意拧到背对锚点
    p0 = (anchor.at(0.0)[0] + 6.0, 1.6, anchor.at(0.0)[2])
    m = Move("translate_forward", 0.0, 3.0, 0, params={"extent": 2.0})
    run = build(m, ctx, Pose(p0, 137.0, 0.0, 0.0))
    a, b = run(0.0, 0.0), run(0.5, 1.5)
    d = vsub(b.p, a.p)
    yaw0 = math.radians(a.yaw)          # 窗首帧朝向 = look-at
    fwd = (math.sin(yaw0), 0.0, math.cos(yaw0))
    cos = (d[0] * fwd[0] + d[2] * fwd[2]) / max(1e-9, vnorm(d))
    assert cos > 0.98, "translate_forward 未沿窗首帧 yaw 基底运动（cos=%.3f）" % cos
