"""depth 内容面规则（docs/83 §5，方向检测 docs/88）：
平铺/格式/量化/NaN/量程/方向 + 跨模态反投影自检。

在此之前 QC 只做**文件名对账**（`checks_json.check_depth_zip`），从不打开 EXR——
于是「深度整体错一帧」「深度类型取成欧氏距离」「内参缩放错」这类致命错全部漏检，
而 68 号 §7.2 对它们的处置是「只要发现一条没对齐，全部打回」。

采样纪律（docs/83「no silent caps」）：格式/量化/NaN/量程按**抽样帧**逐帧全像素检，
抽了哪些帧写进 evidence；反投影按抽样帧对 + 抽样像素。任何被跳过的覆盖面都要说出来。
"""

import math
import zipfile
from pathlib import Path
from typing import List, Optional

from . import exr
from .config import QCConfig
from .mathutil import median
from .report import FAIL, PASS, SKIP, RuleResult

DEPTH_RULE_FLAT = "depth.flat"
DEPTH_RULE_FORMAT = "depth.format"
DEPTH_RULE_QUANT = "depth.quantize"
DEPTH_RULE_NAN = "depth.nan"
DEPTH_RULE_RANGE = "depth.range"
DEPTH_RULE_REPROJ = "depth.reproject"
DEPTH_RULE_ALIGN = "depth.align"
DEPTH_RULE_ORIENT = "depth.orientation"


# ---------------------------------------------------------------- 平铺 / 命名

def check_flat(depth_zip: Optional[Path]) -> RuleResult:
    """68 号 §6.4：「所有文件保存在**单个平铺文件夹**内」。

    既有的 `check_depth_zip` 用 `Path(n).stem` 取成员名，`sub/depth_0.exr` 与
    `depth_0.png` 都能蒙混过关——这条补上目录与扩展名两道。
    """
    if depth_zip is None:
        return RuleResult(DEPTH_RULE_FLAT, SKIP, "depth.zip", "depth.zip 缺失")
    try:
        with zipfile.ZipFile(depth_zip) as zf:
            infos = zf.infolist()
    except (zipfile.BadZipFile, OSError) as e:
        return RuleResult(DEPTH_RULE_FLAT, FAIL, "depth.zip", f"无法读取：{e}")

    nested, dirs, badext = [], [], []
    for zi in infos:
        name = zi.filename
        if zi.is_dir() or name.endswith("/"):
            dirs.append(name)
            continue
        if "/" in name or "\\" in name:
            nested.append(name)
        if not name.lower().endswith(".exr"):
            badext.append(name)

    problems = []
    if dirs:
        problems.append(f"含目录条目 {len(dirs)} 个（如 {dirs[0]!r}）")
    if nested:
        problems.append(f"含嵌套路径 {len(nested)} 个（如 {nested[0]!r}）")
    if badext:
        problems.append(f"扩展名非 .exr {len(badext)} 个（如 {badext[0]!r}）")
    if problems:
        return RuleResult(DEPTH_RULE_FLAT, FAIL, "depth.zip", "；".join(problems))
    return RuleResult(DEPTH_RULE_FLAT, PASS, "depth.zip",
                      f"{len(infos)} 个成员均为平铺 .exr")


# ---------------------------------------------------------------- 抽样与解码

def _sample_indices(n: int, k: int) -> List[int]:
    """首尾必取，中间等距。返回升序去重下标。"""
    if n <= 0:
        return []
    if n <= k:
        return list(range(n))
    idx = {0, n - 1}
    for i in range(k):
        idx.add(round(i * (n - 1) / max(1, k - 1)))
    return sorted(idx)


def _load_depth(zf, name):
    try:
        return exr.read(zf.read(name)), None
    except (exr.ExrError, zipfile.BadZipFile, OSError, ValueError) as e:
        return None, f"{type(e).__name__}: {e}"


# ---------------------------------------------------------------- 量化合规

def _valid_quantized_value(v: float) -> bool:
    """值域判定：v 是否形如 `trunc(d*100)/100` 在该浮点格式下的表示。"""
    if v != v:
        return True
    if math.isinf(v) or v < 0.0:
        return False
    k = v * 100.0
    for cand in (math.floor(k), math.ceil(k)):
        if cand >= 0 and abs(cand / 100.0 - v) <= 1e-6 * max(1.0, abs(v)):
            return True
    return False


def _valid_quantized_half(bits: int) -> bool:
    """位精确判定：该 half 是否形如 `half(k/100)`（k 为非负整数）。

    必须在 `{floor, ceil}` 邻域里找 k，不能只试 `round(v*100)`——16 m 以上
    half 的舍入误差 ulp/2 已超过 0.005，单点判定会产生假阳性（docs/83 §5.2）。
    """
    v = exr.HALF_TO_FLOAT[bits]
    if v != v:                      # NaN：无效像素标记，合法
        return True
    if math.isinf(v) or v < 0.0:
        return False
    k = v * 100.0
    for cand in (math.floor(k), math.ceil(k)):
        if cand < 0:
            continue
        if _half_bits_of(cand / 100.0) == bits:
            return True
    return False


_HALF_BITS_CACHE = {}


def _half_bits_of(x: float) -> int:
    """float → half 位模式（与 ExrWriter.HalfFloat.FromFloat 同口径，NaN 归一 0x7E00）。"""
    hit = _HALF_BITS_CACHE.get(x)
    if hit is not None:
        return hit
    import struct as _s
    (b,) = _s.unpack("<I", _s.pack("<f", x))
    sign = (b >> 16) & 0x8000
    e = (b >> 23) & 0xFF
    m = b & 0x7FFFFF
    if e == 0xFF:
        out = 0x7E00 if m else (sign | 0x7C00)
    elif e == 0:
        out = sign
    else:
        ee = e - 127 + 15
        if ee >= 0x1F:
            out = sign | 0x7C00
        elif ee <= 0:
            if ee < -10:
                out = sign
            else:
                m |= 0x800000
                sh = 14 - ee
                hm = m >> sh
                rem = m & ((1 << sh) - 1)
                half = 1 << (sh - 1)
                if rem > half or (rem == half and (hm & 1)):
                    hm += 1
                out = sign | hm
        else:
            hm = m >> 13
            rem = m & 0x1FFF
            if rem > 0x1000 or (rem == 0x1000 and (hm & 1)):
                hm += 1
                if hm == 0x400:
                    hm = 0
                    ee += 1
                    if ee >= 0x1F:
                        return _cache(x, sign | 0x7C00)
            out = sign | (ee << 10) | hm
    return _cache(x, out)


def _cache(x, v):
    _HALF_BITS_CACHE[x] = v
    return v


# ---------------------------------------------------------------- 内容组

def check_depth_content(depth_zip: Optional[Path], frames15, sysinfo,
                        cfg: QCConfig) -> List[RuleResult]:
    """格式 / 量化 / NaN / 量程 + 反投影。frames15 用于取位姿内参与 depth_name 顺序。"""
    out: List[RuleResult] = []
    if depth_zip is None:
        for r in (DEPTH_RULE_FORMAT, DEPTH_RULE_QUANT, DEPTH_RULE_NAN,
                  DEPTH_RULE_RANGE, DEPTH_RULE_ORIENT, DEPTH_RULE_REPROJ,
                  DEPTH_RULE_ALIGN):
            out.append(RuleResult(r, SKIP, "depth.zip", "depth.zip 缺失"))
        return out

    try:
        zf = zipfile.ZipFile(depth_zip)
    except (zipfile.BadZipFile, OSError) as e:
        for r in (DEPTH_RULE_FORMAT, DEPTH_RULE_QUANT, DEPTH_RULE_NAN,
                  DEPTH_RULE_RANGE, DEPTH_RULE_ORIENT, DEPTH_RULE_REPROJ,
                  DEPTH_RULE_ALIGN):
            out.append(RuleResult(r, FAIL, "depth.zip", f"无法读取：{e}"))
        return out

    with zf:
        members = {Path(n).stem: n for n in zf.namelist() if not n.endswith("/")}
        names = [f.get("depth_name") for f in (frames15 or [])
                 if isinstance(f, dict) and f.get("depth_name")]
        if not names:
            names = sorted(members)
        order = [n for n in names if n in members]
        if not order:
            for r in (DEPTH_RULE_FORMAT, DEPTH_RULE_QUANT, DEPTH_RULE_NAN,
                      DEPTH_RULE_RANGE, DEPTH_RULE_ORIENT, DEPTH_RULE_REPROJ,
                      DEPTH_RULE_ALIGN):
                out.append(RuleResult(r, FAIL, "depth.zip",
                                      "zip 内没有任何与 json depth_name 对应的成员"))
            return out

        picks = _sample_indices(len(order), cfg.depth_sample_frames)
        sampled = [order[i] for i in picks]
        cover = f"抽样 {len(sampled)}/{len(order)} 帧（{sampled[0]}…{sampled[-1]}）"

        images = {}
        errs = []
        for n in sampled:
            img, err = _load_depth(zf, members[n])
            if err:
                errs.append(f"{n}: {err}")
            else:
                images[n] = img
        if errs:
            out.append(RuleResult(DEPTH_RULE_FORMAT, FAIL, "depth.zip",
                                  f"{len(errs)} 帧解码失败：{'; '.join(errs[:3])}"))
            for r in (DEPTH_RULE_QUANT, DEPTH_RULE_NAN, DEPTH_RULE_RANGE,
                      DEPTH_RULE_ORIENT, DEPTH_RULE_REPROJ, DEPTH_RULE_ALIGN):
                out.append(RuleResult(r, SKIP, "depth.zip", "存在解码失败帧，内容组跳过"))
            return out

        out.append(_check_format(images, cover, cfg))
        out.append(_check_quantize(images, cover))
        out.append(_check_nan(images, cover, cfg))
        out.append(_check_range(images, cover, cfg))
        out.append(_check_orientation(images, frames15, cover, cfg))
        out.append(_check_reproject(zf, members, order, frames15, sysinfo, cfg))
    out.append(check_depth_alignment(depth_zip, frames15, sysinfo, cfg))
    return out


def _check_format(images, cover, cfg: QCConfig) -> RuleResult:
    bad = []
    for name, img in images.items():
        if len(img.channels) != 1:
            bad.append(f"{name}: {len(img.channels)} 个通道 {img.channel_names}（要求恰 1 个）")
            continue
        cname, ptype = img.channels[0]
        if cname not in cfg.depth_channel_names:
            bad.append(f"{name}: 通道名 {cname!r}（要求 {list(cfg.depth_channel_names)}）")
        if ptype != exr.PIXELTYPE_HALF:
            bad.append(f"{name}: 像素类型 {exr.PIXELTYPE_NAMES.get(ptype, ptype)}（要求 HALF）")
        if (img.width, img.height) != (cfg.depth_width, cfg.depth_height):
            bad.append(f"{name}: {img.width}x{img.height}"
                       f"（要求 {cfg.depth_width}x{cfg.depth_height}）")
    if bad:
        return RuleResult(DEPTH_RULE_FORMAT, FAIL, "depth.zip", "; ".join(bad[:4]))
    any_img = next(iter(images.values()))
    return RuleResult(DEPTH_RULE_FORMAT, PASS, "depth.zip",
                      f"单通道 {any_img.channels[0][0]} HALF {any_img.width}x{any_img.height}"
                      f"，{cover}")


def _check_quantize(images, cover) -> RuleResult:
    """按**不同取值**校验，而不是逐像素——一帧至多 65536 种，代价与图像大小无关。"""
    offenders = []
    total_distinct = 0
    for name, img in images.items():
        cname, ptype = img.channels[0]
        distinct = set(img.bits[cname])
        total_distinct += len(distinct)
        if ptype == exr.PIXELTYPE_HALF:
            bad = [b for b in distinct if not _valid_quantized_half(b)]
            fmt = lambda b: f"0x{b:04X}={exr.HALF_TO_FLOAT[b]:.6g}"
        else:
            # 非 HALF 已由 depth.format 判 FAIL；这里仍按值域校验，
            # 免得格式与数值两条同时坏时只报一条。
            bad = [v for v in distinct if not _valid_quantized_value(float(v))]
            fmt = lambda v: f"{float(v):.6g}"
        if bad:
            samples = ", ".join(fmt(b) for b in sorted(bad)[:3])
            offenders.append(f"{name}: {len(bad)}/{len(distinct)} 种取值非 cm 截断（{samples}）")
    if offenders:
        return RuleResult(DEPTH_RULE_QUANT, FAIL, "depth.zip", "; ".join(offenders[:3]))
    return RuleResult(DEPTH_RULE_QUANT, PASS, "depth.zip",
                      f"共 {total_distinct} 种取值全部形如 trunc(d*100)/100 或 NaN，{cover}")


def _nan_entries(img):
    """该图里代表 NaN 的**原始条目**集合（HALF 为位模式，FLOAT 为浮点值）。"""
    cname, ptype = img.channels[0]
    distinct = set(img.bits[cname])
    if ptype == exr.PIXELTYPE_HALF:
        return {b for b in distinct if exr.half_is_nan(b)}
    return {v for v in distinct if v != v}


def _check_nan(images, cover, cfg: QCConfig) -> RuleResult:
    fracs = []
    for name, img in images.items():
        bits = img.bits[img.channels[0][0]]
        n = len(bits)
        nanset = _nan_entries(img)
        if nanset:
            # float('nan') != float('nan')，NaN 不能用集合成员判断
            cnt = sum(1 for b in bits if (b != b) or (b in nanset))
            fracs.append((name, cnt / n))
        else:
            fracs.append((name, 0.0))
    worst_hi = max(fracs, key=lambda t: t[1])
    if worst_hi[1] > cfg.depth_nan_frac_max:
        return RuleResult(DEPTH_RULE_NAN, FAIL, "depth.zip",
                          f"{worst_hi[0]} 的 NaN 占比 {worst_hi[1]:.1%} "
                          f"> {cfg.depth_nan_frac_max:.0%}（近乎全无效）")
    lo = min(f for _, f in fracs)
    hi = max(f for _, f in fracs)
    return RuleResult(DEPTH_RULE_NAN, PASS, "depth.zip",
                      f"NaN 占比 {lo:.1%}–{hi:.1%}，{cover}")


def _check_range(images, cover, cfg: QCConfig) -> RuleResult:
    bad = []
    gmin, gmax = float("inf"), 0.0
    for name, img in images.items():
        cname, ptype = img.channels[0]
        bits = img.bits[cname]
        vmin, vmax = float("inf"), 0.0
        for b in set(bits):
            v = exr.HALF_TO_FLOAT[b] if ptype == exr.PIXELTYPE_HALF else float(b)
            if v != v:
                continue
            if math.isinf(v):
                bad.append(f"{name}: 含 Inf")
                continue
            if v <= 0.0:
                bad.append(f"{name}: 含非正深度 {v:g}")
                continue
            vmin = min(vmin, v)
            vmax = max(vmax, v)
        if vmax > cfg.depth_far_max:
            bad.append(f"{name}: 最大深度 {vmax:g} m > 上限 {cfg.depth_far_max:g} m")
        gmin, gmax = min(gmin, vmin), max(gmax, vmax)
    if bad:
        return RuleResult(DEPTH_RULE_RANGE, FAIL, "depth.zip", "; ".join(sorted(set(bad))[:4]))
    if gmin == float("inf"):
        return RuleResult(DEPTH_RULE_RANGE, FAIL, "depth.zip", "抽样帧内没有任何有效深度")
    return RuleResult(DEPTH_RULE_RANGE, PASS, "depth.zip",
                      f"有效深度 {gmin:.2f}–{gmax:.2f} m，{cover}")


# ---------------------------------------------------------------- 方向（上下翻转）

def _band_stats(vals, width, y0, y1):
    """行带 [y0, y1) 的 (NaN 数, 有效值列表)。"""
    nan = 0
    valid = []
    for y in range(y0, y1):
        for v in vals[y * width:(y + 1) * width]:
            if v != v:
                nan += 1
            else:
                valid.append(v)
    return nan, valid


def _check_orientation(images, frames15, cover, cfg: QCConfig) -> RuleResult:
    """上下翻转检测（docs/88）：depth.reproject / depth.align 对垂直镜像有盲区——
    纯 yaw + 水平移动下，镜像与运动可交换，两帧同镜像的反投影残差不变化，
    sfm-d1-20260805 的 900 帧全翻转就是这么在 58 条 PASS 下漏过去的。

    这里用**内容先验**补上，两路独立证据：
    1. 天空/无效 NaN 应聚在图像**顶**行带——横滚≈0 时天空永远在图像上方，
       与俯仰无关；远平面截断的 NaN 贴地平线（中带），已被带设计排除；
    2. 顶带有效深度中位应**大于**底带——室外地面世界里，仰角越高的光线
       走得越远（近地在画面下方，远景/天际在上方）。

    翻转把两路证据同时反号，所以任一路反号即可判 FAIL。两路都不可判时 SKIP
    并说明（不是通过，是测不了）。横滚超限的帧剔除——横滚旋转图像的竖直轴，
    行带统计失去物理意义（本产线轨迹不滚转，真滚转的数据本身就该另审）。
    """
    by_name = {}
    for f in frames15 or []:
        if isinstance(f, dict) and f.get("depth_name"):
            by_name[f["depth_name"]] = f

    nan_top = nan_bot = 0
    votes_ok = votes_flip = votes_tie = 0
    rolled = poseless = 0
    used = 0
    for name, img in images.items():
        quat = (by_name.get(name) or {}).get("camera_rotation_quaternion")
        if quat is None:
            poseless += 1                 # 位姿缺失：按零横滚检（产线轨迹无滚转）
        else:
            R = _quat_matrix(quat)
            if R is None or abs(R[1][0]) > cfg.orient_max_roll_sin:
                rolled += 1               # R[1][0] = 相机 right 的世界 y 分量 ≈ sin(roll)
                continue
        used += 1
        vals = img.floats()
        bh = max(1, int(img.height * cfg.orient_band_frac))
        n_t, v_t = _band_stats(vals, img.width, 0, bh)
        n_b, v_b = _band_stats(vals, img.width, img.height - bh, img.height)
        nan_top += n_t
        nan_bot += n_b
        if len(v_t) >= cfg.orient_band_valid_min and len(v_b) >= cfg.orient_band_valid_min:
            mt, mb = median(v_t), median(v_b)
            if mt >= mb * cfg.orient_median_ratio:
                votes_ok += 1
            elif mb >= mt * cfg.orient_median_ratio:
                votes_flip += 1
            else:
                votes_tie += 1

    gates = []
    if rolled:
        gates.append(f"横滚超限剔除 {rolled} 帧")
    if poseless:
        gates.append(f"无位姿按零横滚检 {poseless} 帧")
    gate_note = ("；" + "、".join(gates)) if gates else ""

    nan_total = nan_top + nan_bot
    nan_ev = None
    if nan_total >= cfg.orient_min_nan_px:
        frac = nan_top / nan_total
        nan_ev = ("flip" if frac <= cfg.orient_nan_top_flip
                  else "ok" if frac >= cfg.orient_nan_top_min else "ambiguous")
    # 弃权票只削弱证据、不制造矛盾：正/反票才参与多数判定（全弃权→该路不判，
    # 面墙/平坦场景走 SKIP 而不是被误判成「方向不可信」）。
    med_ev = None
    decid = votes_ok + votes_flip
    if decid >= cfg.orient_median_frames_min:
        if votes_flip >= decid * cfg.orient_median_maj:
            med_ev = "flip"
        elif votes_ok >= decid * cfg.orient_median_maj:
            med_ev = "ok"
        else:
            med_ev = "ambiguous"

    nan_txt = (f"顶带 NaN 占比 {nan_top}/{nan_total}={nan_top / nan_total:.0%}"
               if nan_total else "顶/底带无 NaN")
    med_txt = f"带中位票 正确 {votes_ok}/翻转 {votes_flip}/弃权 {votes_tie}"
    detail = f"{nan_txt}；{med_txt}（{used} 帧参检，{cover}{gate_note}）"

    if "flip" in (nan_ev, med_ev):
        return RuleResult(DEPTH_RULE_ORIENT, FAIL, "depth.zip",
                          detail + " —— 天空/远景证据在图像**底**侧，depth 相对 video "
                                   "上下翻转（EXR 行序可离线翻转修复，见 docs/88）")
    if "ambiguous" in (nan_ev, med_ev) and "ok" not in (nan_ev, med_ev):
        return RuleResult(DEPTH_RULE_ORIENT, FAIL, "depth.zip",
                          detail + " —— 方向证据不判优（天空 NaN 顶底各半在物理上不该出现）"
                                   "，方向不可信，需人工目检")
    if nan_ev is None and med_ev is None:
        return RuleResult(DEPTH_RULE_ORIENT, SKIP, "depth.zip",
                          detail + f" —— NaN 证据不足 {cfg.orient_min_nan_px} px 且带中位可判帧"
                                   f"不足 {cfg.orient_median_frames_min}，本规则对该 clip "
                                   "无判别力（不是通过，是测不了）")
    return RuleResult(DEPTH_RULE_ORIENT, PASS, "depth.zip", detail)


# ---------------------------------------------------------------- 跨模态反投影

def _quat_matrix(q):
    """[x,y,z,w] → 3x3 旋转矩阵（列主：R[i][j]，v_world = R · v_cam）。"""
    x, y, z, w = q
    n = math.sqrt(x * x + y * y + z * z + w * w)
    if n == 0:
        return None
    x, y, z, w = x / n, y / n, z / n, w / n
    return (
        (1 - 2 * (y * y + z * z), 2 * (x * y - w * z), 2 * (x * z + w * y)),
        (2 * (x * y + w * z), 1 - 2 * (x * x + z * z), 2 * (y * z - w * x)),
        (2 * (x * z - w * y), 2 * (y * z + w * x), 1 - 2 * (x * x + y * y)),
    )


def _mat_mul_vec(m, v):
    return (m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2])


def _mat_t_mul_vec(m, v):
    return (m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
            m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
            m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2])


def _check_reproject(zf, members, order, frames15, sysinfo, cfg: QCConfig) -> RuleResult:
    """把 t 帧的深度反投影到世界、再投到 t+1 个 depth 帧，比共视像素的相对残差中位数。

    一条规则同时钉死：坐标系/四元数顺规、内参（fx vs fy、720/1080 缩放）、
    ±1 帧错位、深度类型（欧氏距离 vs 沿光轴 Z）、reversed-Z 符号、位姿与深度不同源。

    **只在相机确实动了的帧对上才有意义**——静止时反投影退化为恒等，什么都证明不了。
    找不到有位移/转动的帧对就 SKIP 并说明（例如 Stay-完全静止 轨迹）。
    """
    if not frames15:
        return RuleResult(DEPTH_RULE_REPROJ, SKIP, "depth.zip", "15fps json 不可用")

    by_name = {}
    for f in frames15:
        if isinstance(f, dict) and f.get("depth_name"):
            by_name[f["depth_name"]] = f

    # 深度内参 = 视频内参 × (depth_w / video_w)（docs/75 问题 10 已固化口径）
    scale = None
    if isinstance(sysinfo, dict) and sysinfo.get("width"):
        try:
            scale = cfg.depth_width / float(sysinfo["width"])
        except (TypeError, ZeroDivisionError, ValueError):
            scale = None
    if scale is None:
        return RuleResult(DEPTH_RULE_REPROJ, SKIP, "depth.zip",
                          "systeminfo.width 不可用，无法定 depth 内参缩放")

    # 挑一对「相机真的动了」的相邻 depth 帧
    pair = None
    for i in range(len(order) - 1):
        a, b = by_name.get(order[i]), by_name.get(order[i + 1])
        if not a or not b:
            continue
        pa, pb = a.get("camera_position"), b.get("camera_position")
        qa, qb = a.get("camera_rotation_quaternion"), b.get("camera_rotation_quaternion")
        if not (pa and pb and qa and qb):
            continue
        d = math.sqrt(sum((pb[k] - pa[k]) ** 2 for k in range(3)))
        dq = 1.0 - abs(sum(qa[k] * qb[k] for k in range(4)))
        if d >= cfg.reproject_min_baseline_m or dq >= cfg.reproject_min_quat_delta:
            pair = (order[i], order[i + 1], a, b)
            break
    if pair is None:
        return RuleResult(DEPTH_RULE_REPROJ, SKIP, "depth.zip",
                          "找不到相机有位移/转动的相邻 depth 帧对（静止轨迹下该检查无意义）")

    na, nb, fa, fb = pair
    ia, ea = _load_depth(zf, members[na])
    ib, eb = _load_depth(zf, members[nb])
    if ea or eb:
        return RuleResult(DEPTH_RULE_REPROJ, FAIL, "depth.zip", f"帧对解码失败：{ea or eb}")

    ka = fa.get("camera_intrinsics") or {}
    fx = float(ka.get("fx", 0)) * scale
    fy = float(ka.get("fy", 0)) * scale
    cx = float(ka.get("cx", 0)) * scale
    cy = float(ka.get("cy", 0)) * scale
    if fx <= 0 or fy <= 0:
        return RuleResult(DEPTH_RULE_REPROJ, SKIP, "depth.zip", "内参不可用")

    Ra = _quat_matrix(fa["camera_rotation_quaternion"])
    Rb = _quat_matrix(fb["camera_rotation_quaternion"])
    if Ra is None or Rb is None:
        return RuleResult(DEPTH_RULE_REPROJ, FAIL, "depth.zip", "四元数模长为 0")
    Ca = fa["camera_position"]
    Cb = fb["camera_position"]

    stat = _residual(ia, ib, Ra, Ca, Rb, Cb, fx, fy, cx, cy, cfg)
    if stat is None or stat[1] < cfg.reproject_min_samples:
        n = 0 if stat is None else stat[1]
        return RuleResult(DEPTH_RULE_REPROJ, SKIP, "depth.zip",
                          f"{na}→{nb} 共视样本仅 {n} 个，不足 "
                          f"{cfg.reproject_min_samples}，跳过")
    med, nsamp, inside_frac = stat
    detail = (f"{na}→{nb} 相对残差中位数 {med:.4f}"
              f"（样本 {nsamp}，视锥内 {inside_frac:.0%}）")
    if med > cfg.reproject_rel_median_max:
        return RuleResult(DEPTH_RULE_REPROJ, FAIL, "depth.zip",
                          detail + f" > 阈值 {cfg.reproject_rel_median_max:.4f}"
                                   "——depth 与位姿/内参不自洽（坐标系顺规、内参缩放、"
                                   "深度类型、reversed-Z 符号，逐项排查）")
    return RuleResult(DEPTH_RULE_REPROJ, PASS, "depth.zip",
                      detail + f" ≤ 阈值 {cfg.reproject_rel_median_max:.4f}")


def _residual(ia, ib, Ra, Ca, Rb, Cb, fx, fy, cx, cy, cfg):
    """把 ia 的深度按位姿 a→b 反投影再投影，返回 (相对残差中位数, 样本数, 视锥内占比)。"""
    W, H = ia.width, ia.height
    step = max(1, int(math.sqrt(W * H / max(1, cfg.reproject_points))))
    rels = []
    inside = considered = 0
    for y in range(step // 2, H, step):
        for x in range(step // 2, W, step):
            d = ia.at(x, y)
            if d is None or d != d or d <= 0:
                continue
            considered += 1
            # 像素 → 相机系（Unity：+z 前、+y 上，图像 v 轴朝下）
            xc = (x + 0.5 - cx) * d / fx
            yc = -(y + 0.5 - cy) * d / fy
            pw = _mat_mul_vec(Ra, (xc, yc, d))
            pw = (pw[0] + Ca[0], pw[1] + Ca[1], pw[2] + Ca[2])
            rel = (pw[0] - Cb[0], pw[1] - Cb[1], pw[2] - Cb[2])
            pc = _mat_t_mul_vec(Rb, rel)
            if pc[2] <= 0:
                continue
            xi = int(cx + fx * pc[0] / pc[2])
            yi = int(cy - fy * pc[1] / pc[2])
            if not (0 <= xi < W and 0 <= yi < H):
                continue
            inside += 1
            dref = ib.at(xi, yi)
            if dref is None or dref != dref or dref <= 0:
                continue
            rels.append(abs(pc[2] - dref) / dref)
    if not rels:
        return None
    return median(rels), len(rels), (inside / considered if considered else 0.0)



def _rel_pose(fa, fb):
    """两帧的相对位姿：(在 a 相机系下的平移, a→b 的相对旋转矩阵)。

    反投影残差**只**取决于这个量——绝对位姿整体平移/旋转不改变结果。
    """
    Ra = _quat_matrix(fa["camera_rotation_quaternion"])
    Rb = _quat_matrix(fb["camera_rotation_quaternion"])
    if Ra is None or Rb is None:
        raise ValueError("四元数模长为 0")
    Ca, Cb = fa["camera_position"], fb["camera_position"]
    t = _mat_t_mul_vec(Ra, (Cb[0] - Ca[0], Cb[1] - Ca[1], Cb[2] - Ca[2]))
    # R_rel = Ra^T · Rb
    Rrel = tuple(tuple(sum(Ra[k][i] * Rb[k][j] for k in range(3)) for j in range(3))
                 for i in range(3))
    return t, Rrel


def _rel_pose_delta(p, q):
    """两个相对位姿的差异：平移欧氏差 + 旋转夹角（弧度）。用作可判别性度量。"""
    (t1, R1), (t2, R2) = p, q
    dt = math.sqrt(sum((t1[i] - t2[i]) ** 2 for i in range(3)))
    # trace(R1^T R2) → 夹角
    tr = sum(sum(R1[k][i] * R2[k][i] for k in range(3)) for i in range(3))
    ang = math.acos(max(-1.0, min(1.0, (tr - 1.0) / 2.0)))
    return dt + ang


def check_depth_alignment(depth_zip: Optional[Path], frames15, sysinfo,
                          cfg: QCConfig) -> RuleResult:
    """帧对齐：**无量纲**判据，不依赖任何阈值定标。

    对同一对深度图，用位姿偏移 k ∈ {-1, 0, +1} 各算一次反投影残差。
    深度与 json 对齐时 k=0 必须是唯一最小值；深度整体滞后/超前一帧时最小值会挪到 ±1。

    为什么不用绝对阈值：一帧错位造成的残差正比于「每深度帧位移 / 场景深度」——
    3 m/s、15fps、20 m 深度只有约 1%，落在任何为量化噪声留余量的阈值之下。
    实测确认过：初版 2% 绝对阈值对注入的 ±1 帧错位判 PASS（残差 1.09%）。
    argmin 判据没有这个盲区，因为它比较的是同一场景下三种假设的相对优劣。
    """
    if depth_zip is None:
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip", "depth.zip 缺失")
    if not frames15:
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip", "15fps json 不可用")

    scale = None
    if isinstance(sysinfo, dict) and sysinfo.get("width"):
        try:
            scale = cfg.depth_width / float(sysinfo["width"])
        except (TypeError, ZeroDivisionError, ValueError):
            scale = None
    if scale is None:
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip", "systeminfo.width 不可用")

    seq = [f for f in frames15 if isinstance(f, dict) and f.get("depth_name")]
    if len(seq) < 3:
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip",
                          f"仅 {len(seq)} 个 depth 帧，做不了 ±1 偏移比较（需 ≥3）")

    try:
        zf = zipfile.ZipFile(depth_zip)
    except (zipfile.BadZipFile, OSError) as e:
        return RuleResult(DEPTH_RULE_ALIGN, FAIL, "depth.zip", f"无法读取：{e}")

    with zf:
        members = {Path(n).stem: n for n in zf.namelist() if not n.endswith("/")}
        # 挑**可判别**的帧对，而不是随便一个动了的。
        # 反投影只依赖两帧的相对位姿；匀速直线运动下 pose_a→pose_b 与
        # pose_{a+1}→pose_{b+1} 完全相同，整体错一帧在几何上**不可观测**——
        # 此时三个偏移的残差会一模一样。必须选相对位姿随偏移变化最大的那一对。
        idx, best_disc = None, 0.0
        for i in range(1, len(seq) - 2):
            a, b = seq[i], seq[i + 1]
            if a["depth_name"] not in members or b["depth_name"] not in members:
                continue
            try:
                base = _rel_pose(a, b)
                disc = max(_rel_pose_delta(base, _rel_pose(seq[i + k], seq[i + 1 + k]))
                           for k in (-1, 1))
            except (KeyError, TypeError, ValueError):
                continue
            if disc > best_disc:
                idx, best_disc = i, disc
        if idx is None:
            return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip",
                              "没有位姿齐备的相邻 depth 帧对")
        if best_disc < cfg.align_min_discriminability:
            return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip",
                              f"全片相对位姿随 ±1 偏移的变化仅 {best_disc:.4f}"
                              f"（< {cfg.align_min_discriminability}）——"
                              "运动近似匀速直线/静止，**整体错一帧在几何上不可观测**，"
                              "本规则对该 clip 无判别力（不是通过，是测不了）")

        ia, ea = _load_depth(zf, members[seq[idx]["depth_name"]])
        ib, eb = _load_depth(zf, members[seq[idx + 1]["depth_name"]])
        if ea or eb:
            return RuleResult(DEPTH_RULE_ALIGN, FAIL, "depth.zip", f"帧对解码失败：{ea or eb}")

        ka = seq[idx].get("camera_intrinsics") or {}
        fx, fy = float(ka.get("fx", 0)) * scale, float(ka.get("fy", 0)) * scale
        cx, cy = float(ka.get("cx", 0)) * scale, float(ka.get("cy", 0)) * scale
        if fx <= 0 or fy <= 0:
            return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip", "内参不可用")

        scores = {}
        for k in (-1, 0, 1):
            fa, fb = seq[idx + k], seq[idx + 1 + k]
            Ra, Rb = (_quat_matrix(fa["camera_rotation_quaternion"]),
                      _quat_matrix(fb["camera_rotation_quaternion"]))
            if Ra is None or Rb is None:
                continue
            st = _residual(ia, ib, Ra, fa["camera_position"], Rb, fb["camera_position"],
                           fx, fy, cx, cy, cfg)
            if st and st[1] >= cfg.reproject_min_samples:
                scores[k] = st[0]

    if 0 not in scores or len(scores) < 2:
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip",
                          f"可比较的偏移仅 {sorted(scores)}，样本不足")

    best = min(scores, key=scores.get)
    detail = "；".join(f"偏移{k:+d} 残差 {scores[k]:.4f}" for k in sorted(scores))
    if best != 0:
        return RuleResult(DEPTH_RULE_ALIGN, FAIL, "depth.zip",
                          f"最小残差出现在偏移 {best:+d} 而非 0 —— "
                          f"depth 相对 json **整体错位 {best:+d} 帧**（{detail}）")
    others = [v for k, v in scores.items() if k != 0]
    margin = min(others) / scores[0] if scores[0] > 0 else float("inf")
    if margin < cfg.align_margin_min:
        # argmin 落在 0 但拉不开差距：慢速/近匀速 clip 下三个假设的残差本来就
        # 挤在一起，argmin 的位置是噪声——对齐与错位都会走到这里。这不是
        # 「发现了错位」（那是上面 best != 0 的分支），是可判别性预检的漏网
        # 形态：位姿差过了门槛，但换算到像素上的信号仍不够。同一认识状态、
        # 同一结论：测不了（docs/88 辩题二十三）。
        return RuleResult(DEPTH_RULE_ALIGN, SKIP, "depth.zip",
                          f"偏移 0 残差最小但优势仅 {margin:.2f}×（< "
                          f"{cfg.align_margin_min}）——残差分离不出 ±1 帧，"
                          f"本规则对该 clip 无判别力（不是通过，是测不了；{detail}）")
    return RuleResult(DEPTH_RULE_ALIGN, PASS, "depth.zip",
                      f"偏移 0 残差最小且优势 {margin:.1f}×（{detail}）")
