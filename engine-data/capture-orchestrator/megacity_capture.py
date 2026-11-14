#!/usr/bin/env python3
"""megacity_capture.py — Megacity Metro MC 批量采集编排器（docs/78 §5）。

状态机（每条 clip 一个目录，状态即目录内 marker 文件，断点续跑天然成立）：

    PLANNED    plan/ 就绪（trajgen 退出 0 且计划自检全绿）
    CAPTURED   Unity 退出 0 且 12 文件齐 + 构造自检通过
    QC_PASS / QC_FAIL-<rule>
    UPLOADED   rclone 校验和通过（--upload 显式开启才传）

用法：
    python3 megacity_capture.py batch  --count 5 --seed-base 2000 --runs-root <dir>
    python3 megacity_capture.py single --seed 2000 --runs-root <dir>
    python3 megacity_capture.py resume --runs-root <dir>
    python3 megacity_capture.py report --runs-root <dir>

导演模式（docs/78 §10 车辆导演；`--mode director`，各子命令通用）：
    python megacity_capture.py batch --mode director --count 5 --seed-base 13 \
        --runs-root <dir> --scene-file engine-data/capture-orchestrator/megacity-scene.json

    - 计划走 `python -m trajgen direct-clip` 子进程；账本 `--ledger <runs-root>/ledger.json`
      跨条传同一文件 ⇒ 画幅逐步配平 60/10/10/20（进程内新建 QuotaTracker 的话
      pick_aspect 恒见 clips=0、全部 16:9）。
    - 计划失败（direct-clip 内部 8 次重排耗尽）落 PLAN_FAIL，批循环 seed+1 顺移；
      marker 状态机与断点续跑语义与经典模式完全一致。
    - 采集环境照抄本机已验证单条调用：恒等摆位（yaw=0/snap_y=0）+
      DIOGENES_FFMPEG/CODEC/PRESET（NVENC 直出）；30min 看门狗；
      results.xml Passed 且 12 交付文件齐才落 CAPTURED。
    - QC 附 `--init-exempt-lanes 4`（cam4 全景观察位不进初帧同簇口径）。

依赖：与仓库同层的 engine-data/trajgen（配额账本复用其 QuotaTracker，不重写）与
engine-data/qc；Unity 6000.1.0f1 + Megacity 工程（含 DiogenesCapture 测试）。
零第三方依赖纪律与 qc/trajgen 相同。
"""

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

# ---------------------------------------------------------------- 环境常量

ENGINE_DATA = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ENGINE_DATA / "trajgen"))
sys.path.insert(0, str(ENGINE_DATA / "scenes"))

import registry  # noqa: E402  （scenes/registry.py：场景身份与许可的唯一事实源）

_WIN = sys.platform == "win32"
UNITY_BIN = os.environ.get(
    "DIOGENES_UNITY_BIN",
    r"C:\Program Files\Unity\Hub\Editor\6000.1.0f1\Editor\Unity.exe" if _WIN
    else "/Applications/Unity/Hub/Editor/6000.1.0f1/Unity.app/Contents/MacOS/Unity")
UNITY_PROJECT = os.environ.get(
    "DIOGENES_UNITY_PROJECT",
    r"D:\megacity-metro" if _WIN else os.path.expanduser("~/Documents/megacity-metro"))
UNITY_TEST_FILTER = "McClipCaptureTest"
#: 勘察表定稿的世界偏移（S1，probe-report.md：车流最密近原点格）
DEFAULT_WORLD_OFFSET = os.environ.get("DIOGENES_WORLD_OFFSET", "60,138,220")
#: 经典模式的暖机帧默认值（导演模式不显式给 --warmup 时沿用 Unity 侧默认）
DEFAULT_WARMUP = 120
#: 看门狗（docs/78 §5：30min kill + 重试 ≤2）
UNITY_TIMEOUT_S = int(os.environ.get("DIOGENES_UNITY_TIMEOUT_S", "1800"))
UNITY_MAX_RETRIES = 2
#: direct-clip 子进程看门狗（8 次内部重排的计划耗时远小于此；防呆不防长）
PLAN_TIMEOUT_S = int(os.environ.get("DIOGENES_PLAN_TIMEOUT_S", "1800"))
#: Unity 是排他资源：他人占用工程时轮询等待的间隔与上限（不强杀）
UNITY_WAIT_POLL_S = 30
UNITY_WAIT_MAX_S = int(os.environ.get("DIOGENES_UNITY_WAIT_S", "7200"))
#: 连续同规则 QC 失败停批阈值（规则性缺陷，不是坏运气）
SAME_RULE_STOP = 5

R2_REMOTE = "r2:diogenes-engine-delivery"
#: `2MultiCamera` 之上的前缀由场景登记的处置决定（scenes/registry.py），不再是常量：
#: internal 档进 internal-validation/ 做物理隔离，deliverable 档进交付面根。
#: 未给 --scene-id 时退回这个历史缺省（Megacity 线的既有行为，一字不改）。
LEGACY_PREFIX = "internal-validation"
MC_TOP = "2MultiCamera"

MARKERS = ("PLANNED", "CAPTURED", "QC_PASS", "UPLOADED")

#: 作废标记。QC 失败走的是「改名留档」（step_qc），那是逐条的、由规则判定的作废；
#: VOID 是**批级的人工判定**——数据本身没毛病，但因外部原因不得出仓，
#: 典型如 docs/106 的 warehouse-v1/v2：provenance 身份指错场景，而且三个字段
#: 自洽到能骗过许可闸。这种只能靠人下判断，所以要有一个人能落、机器认的标记。
VOID_MARKER = "VOID"


def resolve_scene_id(args) -> None:
    """把 --scene-id 解析成登记条目挂到 args.scene_entry（没给就是 None）。

    给了 --scene-id 就以清单为准：采集侧的身份/许可环境变量全部由清单注入，
    环境里同名变量一律被覆盖并记一行日志。理由见 docs/106——warehouse 三批都是
    「靠调用方记得手工导对环境变量」，v1/v2 就是没导对，把 Robotics-Warehouse
    的数据标成了 Factory Training。
    """
    args.scene_entry = None
    scene_id = getattr(args, "scene_id", None)
    if not scene_id:
        return
    try:
        args.scene_entry = registry.resolve(scene_id)
    except (registry.RegistryError, OSError, json.JSONDecodeError) as e:
        log("--scene-id 解析失败：%s" % e)
        sys.exit(2)
    p = args.scene_entry["provenance"]
    log("场景登记 %s：scene_asset_id=%s license=%s 处置=%s"
        % (args.scene_entry["name"], p["scene_asset_id"],
           p["license_marker"], p["delivery"]))


def resolve_scene_file(args) -> None:
    """scene-file 模式（SceneProbeExporter 产物，docs/78 G'2）：

    - 计划坐标即真实几何的局部坐标 ⇒ 摆位必须恒等：yaw=0、不做走廊高度吸附
      （旋转/吸附会破坏计划与占据快照的对应关系），step_capture 据此传
      DIOGENES_YAW=0 / DIOGENES_SNAP_Y=0；
    - 世界偏移从 scene json 的 world_offset 元数据读取（局部原点的世界坐标），
      用户显式给了 --world-offset 时以用户为准。
    """
    if not getattr(args, "scene_file", None):
        return
    try:
        meta = json.loads(Path(args.scene_file).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        log("scene-file 不可读：%s (%s)" % (args.scene_file, e))
        sys.exit(2)
    off = meta.get("world_offset")
    if off is not None and args.world_offset == DEFAULT_WORLD_OFFSET:
        try:
            if not isinstance(off, list) or len(off) != 3:
                raise ValueError("world_offset 必须是 3 元数组")
            values = [float(x) for x in off]
            if not all(math.isfinite(x) for x in values):
                raise ValueError("world_offset 必须是有限数")
        except (TypeError, ValueError) as e:
            log("scene-file world_offset 无效：%s (%s)" % (off, e))
            sys.exit(2)
        args.world_offset = ",".join(str(round(x, 3)) for x in values)
        log("scene-file world_offset → %s（恒等摆位：yaw=0 / snap_y=0）" % args.world_offset)


# ---------------------------------------------------------------- 基础设施

def log(msg: str) -> None:
    print("[%s] %s" % (datetime.now().strftime("%H:%M:%S"), msg), flush=True)


def marker_path(clip_dir: Path, name: str) -> Path:
    return clip_dir / (".state-" + name)


def set_marker(clip_dir: Path, name: str, payload: str = "") -> None:
    # utf-8 显式：Windows 缺省 locale 编码（cp936）遇 U+FFFD 等字符会直接抛
    marker_path(clip_dir, name).write_text(payload + "\n", encoding="utf-8")


def has_marker(clip_dir: Path, name: str) -> bool:
    return marker_path(clip_dir, name).exists()


def planned_aspect(clip_dir: Path) -> str:
    """从 PLANNED marker 载荷（"… aspect=16:9 …"）读画幅，两种模式通用。"""
    try:
        txt = marker_path(clip_dir, "PLANNED").read_text(encoding="utf-8",
                                                         errors="replace")
    except OSError:
        return "-"
    m = re.search(r"aspect=(\S+)", txt)
    return m.group(1) if m else "-"


def qc_fail_marker(clip_dir: Path):
    for p in clip_dir.glob(".state-QC_FAIL-*"):
        return p.name[len(".state-QC_FAIL-"):]
    return None


def clear_qc_markers(clip_dir: Path) -> None:
    for p in clip_dir.glob(".state-QC_*"):
        p.unlink()


# ---------------------------------------------------------------- 配额账本

def ledger_path(runs_root: Path) -> Path:
    return runs_root / "ledger.json"


def load_quota(runs_root: Path):
    """restore trajgen QuotaTracker（同一实现；序列化已上收进 QuotaTracker）。"""
    from trajgen.planner import QuotaTracker
    return QuotaTracker.load(ledger_path(runs_root))


def save_quota(runs_root: Path, q) -> None:
    q.save(ledger_path(runs_root))


# ---------------------------------------------------------------- Unity 排他

def unity_project_holder() -> str:
    """返回占用 UNITY_PROJECT 的其它 Unity 进程描述；空串＝空闲。

    以进程命令行里的 -projectPath 为准（UnityLockfile 崩溃后可能残留，
    单看文件会假阳性死等）。非 Windows 退化为 lockfile 存在性。
    """
    if _WIN:
        try:
            out = subprocess.run(
                ["powershell", "-NoProfile", "-Command",
                 "Get-CimInstance Win32_Process -Filter \"Name='Unity.exe'\" | "
                 "ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }"],
                capture_output=True, timeout=60).stdout.decode("utf-8", "replace")
        except (OSError, subprocess.TimeoutExpired):
            return ""
        needle = str(UNITY_PROJECT).lower().replace("/", "\\").rstrip("\\")
        for line in out.splitlines():
            if needle in line.lower().replace("/", "\\"):
                return line.strip()[:160]
        return ""
    lock = Path(UNITY_PROJECT) / "Temp" / "UnityLockfile"
    return str(lock) if lock.exists() else ""


def wait_unity_free() -> bool:
    """轮询等待 Megacity 工程空闲（排他资源纪律：等待，不强杀他人进程）。"""
    t0 = time.time()
    while True:
        holder = unity_project_holder()
        if not holder:
            return True
        if time.time() - t0 > UNITY_WAIT_MAX_S:
            log("等待 Unity 空闲超时（%ds），先不采集：%s" % (UNITY_WAIT_MAX_S, holder))
            return False
        log("Unity 工程被占用（%s），%ds 后重试" % (holder, UNITY_WAIT_POLL_S))
        time.sleep(UNITY_WAIT_POLL_S)


# ---------------------------------------------------------------- 步骤：PLAN

def step_plan(clip_dir: Path, seed: int, args, quota) -> bool:
    from trajgen.export import write_clip
    from trajgen.planner import generate
    from trajgen.scene import Scene, synthetic

    plan_dir = clip_dir / "plan"
    if has_marker(clip_dir, "PLANNED"):
        return True
    t0 = time.time()
    scene = (Scene.load(args.scene_file) if args.scene_file
             else synthetic(args.scene, args.scene_seed))
    plan = generate(scene, seed=seed, quota=quota)
    if not plan.ok:
        set_marker(clip_dir, "PLAN_FAIL", "; ".join(plan.reasons))
        log("seed %d PLAN_FAIL: %s" % (seed, "; ".join(plan.reasons)))
        return False
    write_clip(plan, plan_dir)
    set_marker(clip_dir, "PLANNED", "%.1fs aspect=%s" % (time.time() - t0, plan.aspect))
    log("seed %d PLANNED (%.1fs, aspect %s)" % (seed, time.time() - t0, plan.aspect))
    return True


def step_plan_director(clip_dir: Path, seed: int, args, runs_root: Path) -> bool:
    """导演模式计划：`python -m trajgen direct-clip` 子进程 + 持久账本。

    - 账本：`--ledger <runs-root>/ledger.json` 跨条传同一文件，picked 画幅
      按 60/10/10/20 缺口配平（direct-clip 成功才 note_clip 落账）；
    - 失败（direct-clip 内部 8 次重排耗尽抛 RuntimeError → 退出码 1）落
      PLAN_FAIL marker，批循环自然 seed+1 顺移——与经典模式语义一致；
    - 断点续跑：PLANNED marker 在场直接跳过（不重复记账）。
    """
    plan_dir = clip_dir / "plan"
    if has_marker(clip_dir, "PLANNED"):
        return True
    t0 = time.time()
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ENGINE_DATA / "trajgen") + (
        os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    env["PYTHONIOENCODING"] = "utf-8"
    cmd = [sys.executable, "-m", "trajgen", "direct-clip", str(plan_dir),
           "--seed", str(seed),
           "--scene-file", str(args.scene_file),
           "--duration", str(args.duration),
           "--extras", str(args.extras),
           "--covis-hz", str(args.covis_hz),
           "--ledger", str(ledger_path(runs_root))]
    log("seed %d direct-clip: %s" % (seed, " ".join(cmd)))
    try:
        proc = subprocess.run(cmd, env=env, capture_output=True,
                              timeout=PLAN_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        set_marker(clip_dir, "PLAN_FAIL", "direct-clip timeout %ds" % PLAN_TIMEOUT_S)
        log("seed %d PLAN_FAIL: direct-clip 超时 %ds" % (seed, PLAN_TIMEOUT_S))
        return False
    out = (proc.stdout.decode("utf-8", "replace")
           + proc.stderr.decode("utf-8", "replace"))
    (clip_dir / "plan-cli.log").write_text(out, encoding="utf-8")
    if proc.returncode != 0:
        tail = " ".join(out.split())[-300:]
        set_marker(clip_dir, "PLAN_FAIL", tail)
        log("seed %d PLAN_FAIL (direct-clip rc=%d): %s"
            % (seed, proc.returncode, tail[-160:]))
        return False
    aspect = "?"
    try:
        aspect = json.loads((plan_dir / "director_report.json")
                            .read_text(encoding="utf-8")).get("aspect", "?")
    except (OSError, json.JSONDecodeError):
        pass
    dt = time.time() - t0
    set_marker(clip_dir, "PLANNED", "%.1fs aspect=%s mode=director" % (dt, aspect))
    log("seed %d PLANNED (%.1fs, aspect %s, director)" % (seed, dt, aspect))
    return True


# ---------------------------------------------------------------- 步骤：CAPTURE

DELIVERY_FILES = (
    ["video_%d.mp4" % n for n in range(1, 5)]
    + ["action_camera_%d.json" % n for n in range(1, 5)]
    + ["systeminfo_%d.json" % n for n in range(1, 5)]
)


def capture_ok(capture_dir: Path) -> bool:
    return all((capture_dir / f).is_file() for f in DELIVERY_FILES)


def sync_capture_assets() -> str:
    """把 Samples~/Megacity 同步进 Unity 工程副本，返回一行摘要。

    `Assets/DiogenesCapture` 是 Samples~ 的**安装副本**，改完仓库不会自动生效。
    批量前必须核：跑的是不是当前代码。两个已实测踩过的坑：

    1. 忘同步 —— 日志照打、QC 照绿，跑的却是上一版 C#（本机 2026-08-06 实测：
       cam4 剔除载机层的改动没进工程，采集日志仍打 `cam4=[1,2,3]`）。
    2. **同步了但 mtime 没变** —— `shutil.copy2`/`Copy-Item` 保留源时间戳，
       比 Unity 上次编译还旧时会复用旧程序集，跑出「已修好的代码抛旧版异常」
       的假结果。所以逐文件写完**显式把 mtime 打到当前时刻**。

    QC 完全看不见这类偏差（它只读位姿与视频，不核 C# 行为），只能在这里挡。
    """
    src = ENGINE_DATA / "enginecapture" / "Samples~" / "Megacity"
    dst = Path(UNITY_PROJECT) / "Assets" / "DiogenesCapture"
    if not src.is_dir():
        return "skip: 源 %s 不存在" % src
    want = {p.relative_to(src): p for p in src.rglob("*") if p.is_file()}
    changed = 0
    for rel, sp in want.items():
        dp = dst / rel
        if dp.is_file() and dp.read_bytes() == sp.read_bytes():
            continue
        dp.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(sp, dp)          # 不用 copy2：绝不继承源 mtime
        os.utime(dp, None)               # 显式打当前时刻，逼 Unity 重编译
        changed += 1
    stale = 0
    for dp in list(dst.rglob("*")) if dst.is_dir() else []:
        if not dp.is_file():
            continue
        rel = dp.relative_to(dst)
        if rel in want:
            continue
        # .meta 是 Unity 自己生成的（仓库一个都不跟踪）：只要它伺候的源文件还在，
        # 就必须留着——删了会换 GUID，触发无谓重导入。只清真正的孤儿 meta。
        if rel.suffix == ".meta" and rel.with_suffix("") in want:
            continue
        dp.unlink()                      # 仓库里已不存在的文件（重命名/删除的残留）
        stale += 1
    return "assets: %d 文件同步, %d 陈旧文件清除, 共 %d 文件" % (
        changed, stale, len(want))


def step_capture(clip_dir: Path, seed: int, args) -> bool:
    if has_marker(clip_dir, "CAPTURED"):
        return True
    plan_dir = clip_dir / "plan"
    out_root = clip_dir / "capture"
    capture_dir = out_root / "clip"

    for attempt in range(UNITY_MAX_RETRIES + 1):
        if capture_dir.exists():
            shutil.rmtree(capture_dir)
        env = dict(os.environ)
        env.update({
            "DIOGENES_PLAN_DIR": str(plan_dir),
            "DIOGENES_OUT_DIR": str(out_root),
            "DIOGENES_CLIP_NAME": "clip",
            "DIOGENES_WORLD_OFFSET": args.world_offset,
        })
        # 身份与许可从场景登记注入，覆盖环境里的同名变量——采集层只如实转写，
        # 不再自己决定许可（docs/70 R1 / docs/106）。**在模式分支之前**：
        # 两种模式都要如实转写登记，director 也不例外。
        entry = getattr(args, "scene_entry", None)
        if entry is not None:
            for k, v in registry.capture_env(entry).items():
                if env.get(k) not in (None, v):
                    log("seed %d env %s 被场景登记覆盖：%r → %r"
                        % (seed, k, env[k], v))
                env[k] = v
        if getattr(args, "mode", "classic") == "director":
            # 照抄本机端到端验证过的单条调用（docs/91 取证 + NVENC 实测）：
            # 恒等摆位 + ffmpeg 全路径 + 编码器/preset；WARMUP 不显式给时
            # 沿用 Unity 侧默认（验证调用未设它）。
            env.update({"DIOGENES_YAW": "0", "DIOGENES_SNAP_Y": "0",
                        "DIOGENES_FFMPEG": args.ffmpeg,
                        "DIOGENES_CODEC": args.codec,
                        "DIOGENES_PRESET": args.preset})
            if args.warmup is not None:
                env["DIOGENES_WARMUP"] = str(args.warmup)
        else:
            env["DIOGENES_WARMUP"] = str(args.warmup if args.warmup is not None
                                         else DEFAULT_WARMUP)
            if args.scene_file:
                # 真实占据快照：恒等摆位（见 resolve_scene_file）
                env.update({"DIOGENES_YAW": "0", "DIOGENES_SNAP_Y": "0"})
        if args.scene_file:
            # 采集侧也吃同一张占据图：NPC 防穿模用它，地面高度取 world_offset.y
            # （Megacity 版测试类不读这两个 env，传了无害）。**两种模式都要**：
            # director 强制 --scene-file，classic 可选，但只要给了就该转写过去。
            env["DIOGENES_SCENE_FILE"] = str(Path(args.scene_file).resolve())
            env.setdefault("DIOGENES_GROUND_Y", args.world_offset.split(",")[1])
        if not wait_unity_free():
            # 不落 CAPTURE_FAIL：没轮到而已，resume 可续
            return False
        # 必须在拿到 Unity 排他之后再同步：抢锁期间别的进程可能正在编译
        log("seed %d %s" % (seed, sync_capture_assets()))
        unity_log = clip_dir / ("unity-%d.log" % attempt)
        results = clip_dir / ("unity-results-%d.xml" % attempt)
        cmd = [UNITY_BIN, "-batchmode",
               "-projectPath", UNITY_PROJECT,
               "-runTests", "-testPlatform", "PlayMode",
               "-testFilter", UNITY_TEST_FILTER,
               "-testResults", str(results),
               "-logFile", str(unity_log)]
        log("seed %d capture attempt %d: %s" % (seed, attempt, " ".join(cmd)))
        t0 = time.time()
        try:
            proc = subprocess.run(cmd, env=env, timeout=UNITY_TIMEOUT_S,
                                  capture_output=True)
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            rc = -9
            log("seed %d capture TIMEOUT (%ds watchdog), killed" % (seed, UNITY_TIMEOUT_S))
        dt = time.time() - t0

        tail = ""
        if unity_log.exists():
            lines = unity_log.read_text(errors="replace").splitlines()
            tail = "\n".join(lines[-50:])
            (clip_dir / "unity-tail.log").write_text(tail, encoding="utf-8")
        # CAPTURED 判据 = results.xml Passed 且 12 交付文件齐。不再苛求 rc==0：
        # Windows 上 Unity 清场阶段偶发非零退出码，测试结果与产物才是事实源；
        # 看门狗击杀（rc=-9）时 results.xml 不会写出 Passed，不会假阳性。
        passed = results.exists() and 'result="Passed"' in results.read_text(
            encoding="utf-8", errors="replace")[:2000]
        if passed and capture_ok(capture_dir):
            set_marker(clip_dir, "CAPTURED",
                       "%.0fs attempt=%d rc=%s" % (dt, attempt, rc))
            log("seed %d CAPTURED (%.0fs, rc=%s)" % (seed, dt, rc))
            return True
        log("seed %d capture attempt %d failed rc=%s passed=%s files=%s (%.0fs)"
            % (seed, attempt, rc, passed, capture_ok(capture_dir), dt))

    set_marker(clip_dir, "CAPTURE_FAIL", "unity retries exhausted")
    return False


# ---------------------------------------------------------------- 步骤：QC

def step_qc(clip_dir: Path, seed: int, args) -> (bool, str):
    if has_marker(clip_dir, "QC_PASS"):
        return True, ""
    rule = qc_fail_marker(clip_dir)
    if rule:
        return False, rule
    capture_dir = clip_dir / "capture" / "clip"
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ENGINE_DATA / "qc")
    env["PYTHONIOENCODING"] = "utf-8"
    # QC 的 video 组走 shutil.which("ffprobe")：PATH 上没有就整组判 UNVERIFIED
    # 而不是 FAIL——不把 ffmpeg 目录传下去，批量会把「没验过视频」记成 QC_PASS。
    ff = os.environ.get("DIOGENES_FFMPEG") or shutil.which("ffmpeg")
    if ff:
        env["PATH"] = str(Path(ff).parent) + os.pathsep + env.get("PATH", "")
    cmd = [sys.executable, "-m", "engine_qc", "check-clip", "--mode", "mc", "--json"]
    if getattr(args, "mode", "classic") == "director":
        # cam4 同路径异速率后初帧与前三路同簇（实测 6.9°/18.2m，门 15°/30m），
        # 豁免已撤销；统览圆时代的 --init-exempt-lanes 4 不再传。
        pass
    cmd.append(str(capture_dir))
    log("seed %d qc: %s" % (seed, " ".join(cmd)))
    proc = subprocess.run(cmd, env=env, capture_output=True, timeout=600)
    stdout = proc.stdout.decode("utf-8", "replace")
    stderr = proc.stderr.decode("utf-8", "replace")
    (clip_dir / "qc-report.json").write_text(stdout, encoding="utf-8")
    try:
        rep = json.loads(stdout)
        # 只看 status=="FAIL" 会把 UNVERIFIED（规则根本没跑成）当通过。
        # 交付判据是「每条规则都验过且没挂」，缺一不可。
        fails = [r for r in rep.get("results", []) if r.get("status") == "FAIL"]
        unver = [r for r in rep.get("results", []) if r.get("status") == "UNVERIFIED"]
        if unver:
            fails = fails + [{"rule": "qc.unverified",
                              "evidence": "%d 条规则未验证：%s"
                              % (len(unver),
                                 ", ".join(sorted({u.get("rule", "?")
                                                   for u in unver}))[:300])}]
        verdict = str(rep.get("verdict", "")).upper()
        if not fails and verdict and verdict != "PASS":
            fails = [{"rule": "qc.verdict", "evidence": "报告判定 %s" % verdict}]
    except json.JSONDecodeError:
        fails = [{"rule": "qc.crashed", "evidence": stderr[-500:]}]
    if not fails:
        set_marker(clip_dir, "QC_PASS")
        log("seed %d QC_PASS" % seed)
        return True, ""
    rule = fails[0]["rule"]
    set_marker(clip_dir, "QC_FAIL-" + rule,
               "; ".join("%(rule)s: %(evidence)s" % f for f in fails[:5]))
    # 作废纪律：不修数据，目录改名留档
    rejected = capture_dir.with_name("clip.rejected-" + rule.replace("/", "_"))
    if capture_dir.exists() and not rejected.exists():
        capture_dir.rename(rejected)
    log("seed %d QC_FAIL first_rule=%s (%d fails)" % (seed, rule, len(fails)))
    return False, rule


# ---------------------------------------------------------------- 步骤：UPLOAD

def license_gate(clip_dir: Path) -> tuple:
    """许可闸：读 clip 自己写下的 provenance，在场景登记的白名单里反查处置。

    返回 `(entry, "")` 放行，或 `(None, 拒绝理由)`。三道判据缺一不可：

    1. **标记在白名单里**——白名单就是 registry.marker_index() 的键集，即
       「已核验来源与许可的场景」全集。未登记的标记一律拒传（fail closed）。
    2. **scene_asset_id 与标记同属一条登记**。只看标记挡不住 docs/106 那类事故：
       warehouse-v1/v2 的标记「合法」（Asset Store EULA 是白名单里的真标记），
       错的是它与实际场景对不上。两个字段交叉验证才挡得住。
    3. **给了 --scene-id 时还要与本批声明的场景一致**，防止工程指错、批次串号。
    """
    prov = clip_dir / "capture" / "clip" / "provenance.json"
    try:
        doc = json.loads(prov.read_text())
    except (OSError, json.JSONDecodeError) as e:
        return None, "provenance.json 不可读（%s）" % e
    marker = doc.get("license", "")
    scene_asset_id = doc.get("scene_asset_id", "")
    if not marker:
        return None, "provenance 无 license 标记"
    try:
        index = registry.marker_index()
    except (registry.RegistryError, OSError, json.JSONDecodeError) as e:
        return None, "场景登记不可用（%s）" % e
    entry = index.get(marker)
    if entry is None:
        return None, ("license 标记 %r 不在白名单里（已登记：%s）"
                      % (marker, sorted(index)))
    expect = entry["provenance"]["scene_asset_id"]
    if scene_asset_id != expect:
        return None, ("scene_asset_id %r 与标记 %r 所属场景 %r 不符——"
                      "provenance 自相矛盾，按错标处理"
                      % (scene_asset_id, marker, expect))
    return entry, ""


def upload_dest(clip_dir: Path, seed: int, args, entry=None) -> str:
    """MC 交付路径（delivery/README.md 的 MC 规范）：

        [<前缀>/]2MultiCamera/<gameProcessName>/<MMDD>/<时间_场景拼音_序号>/

    前缀、场景拼音、game 段全部取自场景登记（entry）——internal 档进
    `internal-validation/` 做物理隔离，deliverable 档进交付面根。组命名照需求方样例
    `2026_03_11_xingqiuchongqi_01`，序号取 seed%100（演练桶内按 seed 唯一；正式交付
    切 TOS 时按交付表重排）。entry 为 None 时退回 Megacity 线的历史缺省。
    """
    if entry is not None:
        p = entry["provenance"]
        prefix, game, slug = p["upload_prefix"], p["game_process_name"], p["group_slug"]
    else:
        prefix, slug = LEGACY_PREFIX, "megacity"
        try:
            game = json.loads((clip_dir / "capture" / "clip" / "systeminfo_1.json")
                              .read_text()).get("gameProcessName", "MegacityMetro")
        except (OSError, json.JSONDecodeError):
            game = "MegacityMetro"
    now = datetime.now()
    group = getattr(args, "upload_group", None) or \
        "%s_%s_%02d" % (now.strftime("%Y_%m_%d"), slug, seed % 100)
    head = "/".join(x for x in (R2_REMOTE, prefix, MC_TOP) if x)
    return "%s/%s/%s/%s" % (head, game, now.strftime("%m%d"), group)


def step_upload(clip_dir: Path, seed: int, args) -> bool:
    if not args.upload:
        return False
    if has_marker(clip_dir, "UPLOADED"):
        return True
    capture_dir = clip_dir / "capture" / "clip"
    # 人工作废优先于一切自动判据：闸看的是 provenance 自洽性，而作废的理由
    # 往往正是「自洽但整体是错的」，闸按定义看不出来（docs/106 §3.1）。
    if has_marker(clip_dir, VOID_MARKER):
        why = marker_path(clip_dir, VOID_MARKER).read_text().strip()
        log("seed %d upload REFUSED: 批次已作废（%s）" % (seed, why))
        return False
    # 许可红线（docs/78 R'5 + docs/70 R1）：标记必须在场且与场景登记自洽，
    # 前缀由登记的处置决定，不由调用方决定。
    entry, why = license_gate(clip_dir)
    if entry is None:
        log("seed %d upload REFUSED: %s" % (seed, why))
        return False
    declared = getattr(args, "scene_entry", None)
    if declared is not None and declared["name"] != entry["name"]:
        log("seed %d upload REFUSED: clip 自称 %s，本批声明的是 %s"
            % (seed, entry["name"], declared["name"]))
        return False
    dest = upload_dest(clip_dir, seed, args, entry)
    # 上传面 = 12 交付文件 + plan/covis/dyn_metrics/provenance 侧车（docs/78 §4.2 打包口径）；
    # audio_mix.wav 是 mux 前的引擎混音中间产物，不出仓
    cmd = ["rclone", "copy", "--checksum", "--exclude", "audio_mix.wav",
           str(capture_dir), dest]
    log("seed %d upload: %s" % (seed, " ".join(cmd)))
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=3600)
    if proc.returncode != 0:
        log("seed %d upload failed: %s" % (seed, proc.stderr[-300:]))
        return False
    # 远端校验（rclone check：逐文件 size+hash 比对），过了才落 UPLOADED marker
    chk = ["rclone", "check", "--exclude", "audio_mix.wav", str(capture_dir), dest]
    proc = subprocess.run(chk, capture_output=True, text=True, timeout=1800)
    if proc.returncode != 0:
        log("seed %d upload check FAILED: %s" % (seed, proc.stderr[-300:]))
        return False
    set_marker(clip_dir, "UPLOADED", dest)
    log("seed %d UPLOADED → %s" % (seed, dest))
    return True


# ---------------------------------------------------------------- 批次驱动

def run_one(runs_root: Path, seed: int, args, quota) -> dict:
    clip_dir = runs_root / ("clip-%06d" % seed)
    clip_dir.mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    rec = {"seed": seed, "dir": clip_dir.name}

    def done(state, rule=None):
        rec["state"] = state
        if rule:
            rec["qc_rule"] = rule
        rec["dt"] = round(time.time() - t0, 1)
        return rec

    if args.mode == "director":
        # 账本由 direct-clip 子进程直接读写（--ledger），进程内 quota 不参与
        if not step_plan_director(clip_dir, seed, args, runs_root):
            return done("PLAN_FAIL")
    else:
        if not step_plan(clip_dir, seed, args, quota):
            return done("PLAN_FAIL")
        save_quota(runs_root, quota)
    rec["aspect"] = planned_aspect(clip_dir)

    if not step_capture(clip_dir, seed, args):
        return done("CAPTURE_FAIL")

    ok, rule = step_qc(clip_dir, seed, args)
    if not ok:
        return done("QC_FAIL", rule)

    if args.upload and step_upload(clip_dir, seed, args):
        return done("UPLOADED")
    return done("QC_PASS")


def cmd_batch(args) -> int:
    runs_root = Path(args.runs_root)
    runs_root.mkdir(parents=True, exist_ok=True)
    quota = load_quota(runs_root)
    records = []
    passes = 0
    seed = args.seed_base
    attempts = 0
    same_rule_streak = ("", 0)
    max_attempts = args.count * 3

    while passes < args.count and attempts < max_attempts:
        rec = run_one(runs_root, seed, args, quota)
        records.append(rec)
        attempts += 1
        if rec["state"] in ("QC_PASS", "UPLOADED"):
            passes += 1
        elif rec["state"] == "QC_FAIL":
            rule = rec.get("qc_rule", "?")
            streak_rule, streak_n = same_rule_streak
            same_rule_streak = (rule, streak_n + 1 if rule == streak_rule else 1)
            if same_rule_streak[1] >= SAME_RULE_STOP:
                log("停批：连续 %d 个种子在同一规则 %s 上失败（规则性缺陷）"
                    % (SAME_RULE_STOP, rule))
                break
        seed += 1  # QC 失败 → seed+1 重录；成功也顺排下一个种子

    if args.mode == "director":
        quota = load_quota(runs_root)  # 子进程更新过账本，重读再进报表
    write_report(runs_root, records, quota, args)
    return 0 if passes >= args.count else 1


def cmd_single(args) -> int:
    runs_root = Path(args.runs_root)
    runs_root.mkdir(parents=True, exist_ok=True)
    quota = load_quota(runs_root)
    rec = run_one(runs_root, args.seed, args, quota)
    print(json.dumps(rec, ensure_ascii=False, indent=2))
    return 0 if rec["state"] in ("QC_PASS", "UPLOADED") else 1


def cmd_resume(args) -> int:
    """扫描 runs-root 里未完成的 clip 目录接着跑（marker 决定断点）。"""
    runs_root = Path(args.runs_root)
    quota = load_quota(runs_root)
    records = []
    for clip_dir in sorted(runs_root.glob("clip-*")):
        seed = int(clip_dir.name.split("-")[1])
        if has_marker(clip_dir, "QC_PASS") or qc_fail_marker(clip_dir) \
                or has_marker(clip_dir, "PLAN_FAIL") or has_marker(clip_dir, "CAPTURE_FAIL"):
            continue
        records.append(run_one(runs_root, seed, args, quota))
    if args.mode == "director":
        quota = load_quota(runs_root)
    write_report(runs_root, records, quota, args)
    return 0


def cmd_void(args) -> int:
    """把整批标成作废：逐 clip 落 VOID marker，批根写 VOID.md 说明。

    **不删不改数据**——沿用「不修数据、留档」的作废纪律，只是把「这批不得出仓」
    这条人的判断变成机器认得的状态。要撤销就删 `.state-VOID` 与 `VOID.md`。
    """
    runs_root = Path(args.runs_root)
    if not runs_root.is_dir():
        log("runs-root 不存在：%s" % runs_root)
        return 2
    clips = sorted(runs_root.glob("clip-*"))
    if not clips:
        log("runs-root 下没有 clip 目录：%s" % runs_root)
        return 2
    for clip_dir in clips:
        set_marker(clip_dir, VOID_MARKER, args.reason)
    stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    (runs_root / "VOID.md").write_text(
        "# 本批已作废，不得上传\n\n"
        "- 作废时间：%s\n- 覆盖 clip：%d 个\n- 理由：%s\n\n"
        "数据原样保留（作废纪律：不修数据、留档）。编排器 `step_upload` 见到\n"
        "`.state-VOID` 即拒传，优先于许可闸——因为作废理由常常正是「provenance\n"
        "自洽但整体是错的」，那种错闸按定义看不出来。\n\n"
        "撤销：删掉各 clip 的 `.state-VOID` 与本文件。\n" % (stamp, len(clips), args.reason),
        encoding="utf-8")
    log("已作废 %d 条 → %s" % (len(clips), runs_root / "VOID.md"))
    return 0


def cmd_report(args) -> int:
    runs_root = Path(args.runs_root)
    records = []
    for clip_dir in sorted(runs_root.glob("clip-*")):
        seed = int(clip_dir.name.split("-")[1])
        state = "PENDING"
        rule = None
        for name in ("UPLOADED", "QC_PASS", "CAPTURE_FAIL", "PLAN_FAIL"):
            if has_marker(clip_dir, name):
                state = name
                break
        else:
            rule = qc_fail_marker(clip_dir)
            if rule:
                state = "QC_FAIL"
            elif has_marker(clip_dir, "CAPTURED"):
                state = "CAPTURED"
            elif has_marker(clip_dir, "PLANNED"):
                state = "PLANNED"
        rec = {"seed": seed, "dir": clip_dir.name, "state": state,
               "aspect": planned_aspect(clip_dir)}
        if rule:
            rec["qc_rule"] = rule
        records.append(rec)
    write_report(runs_root, records, load_quota(runs_root), args)
    return 0


def write_report(runs_root: Path, records, quota, args) -> None:
    runs_root.mkdir(parents=True, exist_ok=True)
    total = len(records)
    passes = sum(1 for r in records if r["state"] in ("QC_PASS", "UPLOADED"))
    by_state = {}
    for r in records:
        by_state[r["state"]] = by_state.get(r["state"], 0) + 1
    report = {
        "generated": datetime.now().isoformat(timespec="seconds"),
        "runs_root": str(runs_root),
        "mode": getattr(args, "mode", "classic"),
        "world_offset": args.world_offset,
        "clips": records,
        "attempts": total,
        "qc_pass": passes,
        "success_rate": round(passes / total, 4) if total else 0.0,
        "by_state": by_state,
        "quota": quota.report(),
    }
    (runs_root / "batch-report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")

    lines = ["# Megacity MC 批量采集报表", "",
             "生成时间: %s" % report["generated"],
             "模式: %s" % report["mode"],
             "世界偏移: %s" % args.world_offset,
             "尝试 %d 条 / QC 通过 %d 条 / 成功率 %.0f%%"
             % (total, passes, report["success_rate"] * 100), "",
             "| seed | 状态 | 画幅 | QC 首个失败规则 | 用时 s |",
             "|---|---|---|---|---|"]
    for r in records:
        lines.append("| %s | %s | %s | %s | %s |" % (
            r["seed"], r["state"], r.get("aspect", "-"),
            r.get("qc_rule", "-"), r.get("dt", "-")))
    lines += ["", "## 配额账本（trajgen QuotaTracker 快照）", "",
              "```json", json.dumps(report["quota"], ensure_ascii=False, indent=2),
              "```"]
    (runs_root / "batch-report.md").write_text("\n".join(lines) + "\n",
                                               encoding="utf-8")
    log("报表 → %s" % (runs_root / "batch-report.md"))


# ---------------------------------------------------------------- CLI

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        prog="megacity_capture",
        description="Megacity Metro MC 批量采集编排器（docs/78 §5 状态机）")
    sub = ap.add_subparsers(dest="command", required=True)

    def common(p):
        p.add_argument("--runs-root", required=True, help="批工作目录（状态即目录）")
        p.add_argument("--scene-id",
                       help="scenes/manifest.json 的条目名（如 robotics-warehouse）："
                            "采集身份与许可标记按登记注入，上传前缀按登记的处置选。"
                            "不给则沿用历史行为（环境变量自带 + internal-validation/ 前缀）")
        p.add_argument("--mode", choices=("classic", "director"), default="classic",
                       help="classic=固定机位库（进程内 generate）；director=车辆导演"
                            "（direct-clip 子进程 + 持久账本画幅配平，需 --scene-file）")
        p.add_argument("--scene", default="open_plaza",
                       help="trajgen 合成场景（S1 世界偏移放置法）")
        p.add_argument("--scene-file", help="真实占据快照 json（SceneProbeExporter 产物）")
        p.add_argument("--scene-seed", type=int, default=1)
        p.add_argument("--world-offset", default=DEFAULT_WORLD_OFFSET,
                       help='Unity 世界偏移 "x,y,z"（勘察表推荐值）')
        p.add_argument("--warmup", type=int, default=None,
                       help="暖机帧；经典模式缺省 %d，导演模式缺省沿用 Unity 侧默认"
                            % DEFAULT_WARMUP)
        p.add_argument("--duration", type=float, default=60.0,
                       help="（director）单条时长 s（60=交付口径）")
        p.add_argument("--extras", type=int, default=2,
                       help="（director）氛围受控车数量")
        p.add_argument("--covis-hz", type=float, default=2.0,
                       help="（director）共视度量采样率")
        p.add_argument("--ffmpeg",
                       help="（director）ffmpeg 全路径（DIOGENES_FFMPEG）；缺省从 PATH 解析")
        p.add_argument("--codec", default="h264_nvenc",
                       help="（director）编码器（DIOGENES_CODEC，本机 NVENC 实测可用）")
        p.add_argument("--preset", default="p4",
                       help="（director）编码 preset（DIOGENES_PRESET）")
        p.add_argument("--upload", action="store_true",
                       help="QC_PASS 后 rclone 上传 R2（前缀由场景登记的处置决定）")
        p.add_argument("--upload-group",
                       help="MC 组名（<YYYY_MM_DD_场景拼音_序号>），缺省按日期+seed 生成")

    p = sub.add_parser("batch", help="批量采集直至 count 条 QC_PASS")
    p.add_argument("--count", type=int, default=5)
    p.add_argument("--seed-base", type=int, default=2000)
    common(p)

    p = sub.add_parser("single", help="单条（调试）")
    p.add_argument("--seed", type=int, required=True)
    common(p)

    p = sub.add_parser("resume", help="断点续跑 runs-root 中未完成的 clip")
    common(p)

    p = sub.add_parser("report", help="仅重算报表")
    common(p)

    # void 不吃 common()：它只碰 marker，给它 --scene-id/--world-offset 之类
    # 反而会让人以为它会重跑什么。
    p = sub.add_parser("void", help="把整批标成作废（不得上传），数据原样保留")
    p.add_argument("--runs-root", required=True)
    p.add_argument("--reason", required=True, help="作废理由，写进 marker 与 VOID.md")

    args = ap.parse_args(argv)
    if args.command == "void":
        return cmd_void(args)
    resolve_scene_id(args)
    # void 子命令没有 --mode，所以这道校验必须在它 return 之后、且用 getattr 取值。
    if getattr(args, "mode", "classic") == "director" and args.command in ("batch", "single", "resume"):
        if not args.scene_file:
            ap.error("--mode director 需要 --scene-file（direct-clip 只认真实占据快照）")
        if not args.ffmpeg:
            args.ffmpeg = shutil.which("ffmpeg") or ""
        if not args.ffmpeg or not Path(args.ffmpeg).is_file():
            ap.error("--mode director 需要 ffmpeg（--ffmpeg 或 PATH 里可解析）")
        log("director ffmpeg = %s" % args.ffmpeg)
    resolve_scene_file(args)
    return {"batch": cmd_batch, "single": cmd_single,
            "resume": cmd_resume, "report": cmd_report}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
