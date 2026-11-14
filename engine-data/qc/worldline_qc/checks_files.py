"""打包面：文件齐全、命名布局、manifest 哈希、systeminfo。

需求出处：交付说明《Multi-camera 12 路相机数据交付说明》§一/§三/§四/§五，新需求 §5.2、§5.3、§7.1。
"""

from __future__ import annotations

import hashlib
from pathlib import Path
from typing import List, Optional

from .loaders import Group, load_json
from .model import FAIL, MANUAL, PASS, SKIP, WARN, RuleResult

SYSINFO_KEYS = ("gameProcessName", "x", "y", "width", "height", "recordDpi")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_packaging(g: Group) -> List[RuleResult]:
    out: List[RuleResult] = []
    missing = []
    for lf in g.lanes:
        for name in lf.missing():
            missing.append(f"{lf.info.take}/{name}_{lf.info.lane}")
    n_ok = sum(1 for lf in g.lanes if not lf.missing())
    if missing:
        out.append(RuleResult("pack.files", FAIL, "group",
                              f"12 路 × 4 文件应齐全，缺 {len(missing)} 个：{'、'.join(missing[:12])}"
                              + ("…" if len(missing) > 12 else ""), "交付说明 v1.1 §一 / 需求§5.2"))
    else:
        out.append(RuleResult("pack.files", PASS, "group", f"12 路 × 4 文件齐全（{n_ok * 4} 个）", "交付说明 v1.1 §一 / 需求§5.2"))

    # 包内多余文件：交付说明只认 48 个文件（内部 README/manifest 容忍），其余会干扰需方按编号收件
    expected = set()
    for lf in g.lanes:
        for _, p in lf.files():
            if p is not None:
                expected.add(p.resolve())
    tolerated = {"README.md", "manifest.json"}
    extra = sorted(str(p.relative_to(g.root)) for p in g.root.rglob("*")
                   if p.is_file() and p.resolve() not in expected and p.name not in tolerated and not p.name.startswith("."))
    if extra:
        out.append(RuleResult("pack.extra_files", WARN, "group",
                              f"包内有 {len(extra)} 个 48 件之外的文件：{'、'.join(extra[:8])}{'…' if len(extra) > 8 else ''}"
                              "（正式交付只放 video_i/systeminfo_i/action_camera_i/multicam_sync_i 与元信息表；预览片等内部产物不要随包上传）", "交付说明§一/§四"))
    else:
        out.append(RuleResult("pack.extra_files", PASS, "group", "包内无 48 件之外的文件", "交付说明§一/§四"))

    # 世界线覆盖：A 3 路、B 1 路、C 4 路、D 4 路
    counts = {wl: len(g.lanes_of(wl)) for wl in "ABCD"}
    want = {"A": 3, "B": 1, "C": 4, "D": 4}
    if counts == want:
        out.append(RuleResult("pack.worldlines", PASS, "group", "世界线 A/B/C/D 路数 3/1/4/4", "需求§5.2"))
    else:
        out.append(RuleResult("pack.worldlines", FAIL, "group",
                              f"世界线路数 {counts}，应为 {want}", "需求§5.2"))

    # 布局：需方要求平铺 1～12，不按世界线建子目录
    if g.layout == "flat":
        out.append(RuleResult("pack.layout", PASS, "group", "平铺布局 video_1..12（与交付说明一致）", "交付说明§三/§四"))
    else:
        out.append(RuleResult("pack.layout", FAIL, "group",
                              "按「遍」分子目录（AB/C/D 各 1–4 路），交付说明要求同一采集组内平铺存放、编号 1～12"
                              "（A=1–3，B=4，C=5–8，D=9–12），不再按世界线建子目录；正式交付前须重排。", "交付说明§三/§五"))

    # 文件名带下划线（交付说明§四）——布局发现已按带下划线的名字找文件，这里只补一条显式结论
    bad = [p.name for p in g.root.rglob("*") if p.is_file() and p.suffix in (".mp4", ".json")
           and p.name not in ("manifest.json",) and "_" not in p.name]
    if bad:
        out.append(RuleResult("pack.naming", FAIL, "group", f"文件名缺下划线：{'、'.join(bad[:8])}", "交付说明§四"))
    else:
        out.append(RuleResult("pack.naming", PASS, "group", "文件名格式 video_i / systeminfo_i / action_camera_i / multicam_sync_i", "交付说明§四"))

    # 采集组文件夹命名 YYYY_MM_DD_场景名称_标注序号
    import re
    if re.match(r"^\d{4}_\d{2}_\d{2}_.+_\d{3}$", g.root.name):
        out.append(RuleResult("pack.group_name", PASS, "group", f"采集组文件夹名 {g.root.name}", "交付说明§二/§三"))
    else:
        out.append(RuleResult("pack.group_name", WARN, "group",
                              f"采集组文件夹名 {g.root.name} 不符合 YYYY_MM_DD_场景名称_标注序号（内部演示可接受，正式交付须改名）",
                              "交付说明§二/§三"))

    # 元信息表 / 交付表不在包内
    xlsx = list(g.root.glob("*.xlsx")) + list(g.root.glob("*.csv"))
    if xlsx:
        out.append(RuleResult("pack.metainfo", MANUAL, "group", f"包内含表格 {', '.join(p.name for p in xlsx)}，字段完整性需人工对模板", "需求§5.5/§5.6"))
    else:
        out.append(RuleResult("pack.metainfo", WARN, "group",
                              "包内没有元信息表与交付表（正式交付须随 TOS 上传元信息表，并在交付表登记 48 个文件的 TOS 地址、"
                              "世界线/相机类型/受控人物/事件是否改变等字段）", "需求§5.5/§5.6 / 交付表模板"))
    return out


def check_manifest(g: Group) -> List[RuleResult]:
    out: List[RuleResult] = []
    if g.manifest is None:
        out.append(RuleResult("pack.manifest", WARN, "group", "无 manifest.json（内部校验件，需方不要求）", "内部"))
        return out
    files = g.manifest.get("files") if isinstance(g.manifest, dict) else None
    if not isinstance(files, dict):
        out.append(RuleResult("pack.manifest", FAIL, "group", "manifest.json 缺 files 表", "内部"))
        return out
    bad, checked, absent = [], 0, []
    for rel, meta in files.items():
        p = g.root / rel
        if not p.is_file():
            absent.append(rel)
            continue
        size = p.stat().st_size
        if size != meta.get("bytes"):
            bad.append(f"{rel} 字节数 {size}≠{meta.get('bytes')}")
            continue
        if sha256_file(p) != meta.get("sha256"):
            bad.append(f"{rel} SHA-256 不符")
        checked += 1
    if bad or absent:
        out.append(RuleResult("pack.manifest", FAIL, "group",
                              f"manifest 核验：{checked} 个通过；不符 {len(bad)}：{'；'.join(bad[:5])}"
                              + (f"；缺文件 {len(absent)}：{'、'.join(absent[:5])}" if absent else ""), "内部"))
    else:
        out.append(RuleResult("pack.manifest", PASS, "group", f"manifest 内 {checked} 个文件 SHA-256 与字节数全部一致", "内部"))
    if g.manifest.get("delivery_eligible") is False or g.manifest.get("internal") is True:
        out.append(RuleResult("pack.internal_flag", WARN, "group",
                              "manifest 标记 internal=true / delivery_eligible=false：本包为内部演示件，不能作为正式交付", "内部"))
    return out


def check_systeminfo(g: Group, video_dims: dict) -> List[RuleResult]:
    """video_dims: {lane_key: (w, h)}，来自 ffprobe；没有时只查字段与 0,0。"""
    out: List[RuleResult] = []
    for lf in g.lanes:
        key = lf.info.key
        if lf.systeminfo is None:
            out.append(RuleResult("sysinfo.schema", SKIP, key, "systeminfo 缺失（见 pack.files）", "需求§5.3"))
            continue
        obj, err = load_json(lf.systeminfo)
        if err or not isinstance(obj, dict):
            out.append(RuleResult("sysinfo.schema", FAIL, key, f"systeminfo 不是 JSON 对象：{err}", "需求§5.3"))
            continue
        missing = [k for k in SYSINFO_KEYS if k not in obj]
        extra = [k for k in obj if k not in SYSINFO_KEYS]
        if missing or extra:
            out.append(RuleResult("sysinfo.schema", FAIL, key,
                                  f"字段应恰为 {list(SYSINFO_KEYS)}；缺 {missing} 多 {extra}", "需求§5.3"))
        else:
            out.append(RuleResult("sysinfo.schema", PASS, key,
                                  f"gameProcessName={obj['gameProcessName']}，{obj['width']}×{obj['height']}，recordDpi={obj['recordDpi']}",
                                  "需求§5.3"))
        if obj.get("x") == 0 and obj.get("y") == 0:
            out.append(RuleResult("sysinfo.origin", PASS, key, "窗口左上角 (0,0)，全屏录制", "需求§3.2/§5.3"))
        else:
            out.append(RuleResult("sysinfo.origin", FAIL, key, f"窗口左上角 ({obj.get('x')},{obj.get('y')})，应为全屏 (0,0)", "需求§3.2/§5.3"))
        w, h = obj.get("width"), obj.get("height")
        dims = video_dims.get(key)
        if dims is None:
            out.append(RuleResult("sysinfo.resolution", SKIP, key, "无视频探测结果，无法与 systeminfo 比对", "需求§3.2"))
        elif (w, h) == dims:
            out.append(RuleResult("sysinfo.resolution", PASS, key, f"systeminfo {w}×{h} 与视频实际一致", "需求§3.2/§5.3"))
        else:
            out.append(RuleResult("sysinfo.resolution", FAIL, key, f"systeminfo {w}×{h} ≠ 视频 {dims[0]}×{dims[1]}", "需求§3.2/§5.3"))
    return out
