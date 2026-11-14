#!/usr/bin/env python3
"""会话验收——上传完成后对单个 session 出验收报告（docs/150）.

管线位置: 客户端上传 → dataminer-api 落 R2 → **本脚本（cron 触发）** →
报告写回 session 前缀 `acceptance/dm-session-acceptance-1.json` → console
DataBrowser 读取展示（console 侧契约见 docs/150 §4）。

单 session 验收内容:
  1. 完整性: 每 clip 六件套齐全（video/frames/events/audio/meta/manifest）,
     文件名内嵌 sha256 与 manifest 一致（内容寻址使核对免费, 不重下正文）
  2. 游戏归属: meta.target.game_id（docs/149 铁律, 缺 target 记 _missing_target）
  3. QC: ml/diogenes_wm/halo_qc.policy_for_game(game_id) 逐判据评定
  4. 小时数: Σn_frames ÷ fps（doc 44 强制口径）+ fps 同质性
  5. 健康位聚合: duplicate_frames / queue_drops / input_overflow / 空 events

用法:
  # 单 session（在线, 需 RCLONE_CONFIG_R2_* 凭据; 车道默认两条源车道全扫）
  ./accept.py --session <uuid>
  # 扫描未验收的已完成 session（cron 模式; 完成的启发式=最后写入静默 ≥15min）
  ./accept.py --scan --quiet-minutes 15 --limit 40
  # 写回 R2（需写凭据; 默认只落本地 --out-dir）
  ./accept.py --session <uuid> --upload
  # 离线（测试）: --listing tp.txt --payload-dir <本地按相对路径的对象目录>

退出码: 0=全部通过 · 1=有 session 验收失败 · 2=致命错误

2026-09-06 的三处修改（issue #887 / docs/167）:
  1. **多车道**。摄取面 08-29 从 `raw/v1/non-trainable` 切到 `raw/v2`（issue #855）,
     写死 v1 意味着 v2 的 391 个 session 永远不会被验收。清单路径改成桶内完整 key。
  2. **批量拉取**。原来每个完整 clip 要 4 次 `rclone cat`（meta/manifest/frames/events）,
     6 万+ clip × 4 就是 45 分钟 timeout 的病根（cron 从未跑完过一轮）。改成整批
     `rclone copy --files-from` 一次拉齐再从本地读, 与 weekly_report 同一路子。
  3. **每轮限额 + 最旧优先**（--limit）。跑不完的留给下一轮, 保证每轮都有净进展;
     原来每轮都从头扫、每轮都被杀, 积压只增不减。
"""
from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "ml"))
from diogenes_wm.halo_qc import evaluate_session, policy_for_game  # noqa: E402
from diogenes_wm.game_registry import GOVERNANCE  # noqa: E402

SCHEMA = "dm-session-acceptance/1"
REPORT_REL = "acceptance/dm-session-acceptance-1.json"
# audio 是可选件: fallout4 depth 产线无音频(doc 50), 缺 audio 不算不完整,
# 报告以 audio_present_clips 计数披露
REQUIRED = ("video", "frames", "events", "meta", "manifest")
SIX = REQUIRED + ("audio",)
OBJ = re.compile(
    r"(?P<prefix>.*sessions/(?P<session>[^/]+))/runs/(?P<run>[^/]+)/clips/"
    r"(?P<clip>[^/]+)/(?P<kind>video|frames|events|audio|meta|manifest)"
    r"-(?P<sha>[0-9a-f]{64})\.(?P<ext>mp4|jsonl\.zst|pcm\.zst|json)$")
RCLONE = ["rclone", "--fast-list", "--contimeout", "15s", "--timeout", "300s"]
MISSING = "_missing_target"
#: 两条**源**车道（与 tools/weekly-report/weekly_report.py 的 SOURCE_LANES 同口径）。
#: `raw/v1/attested-trainable` 是 r2-attested-reclass 的派生镜像, 不在此列——
#: 它的 session 已经在 non-trainable 里验收过一次了。
SOURCE_LANES = (
    "raw/v1/non-trainable/dataminer-api-production",
    "raw/v2/dataminer-api-production",
)


def die(msg: str) -> None:
    print(f"FATAL: {msg}", file=sys.stderr)
    raise SystemExit(2)


def bucket() -> str:
    if not os.environ.get("RCLONE_CONFIG_R2_ACCESS_KEY_ID"):
        die("缺少 RCLONE_CONFIG_R2_* 环境变量（见 tools/r2-attested-reclass/README）")
    return os.environ.get("DM_R2_BUCKET", "diogenes-model-training")


# --------------------------------------------------------------------------- #
# IO 后端: 在线 rclone / 离线目录, 同一接口
# --------------------------------------------------------------------------- #
class Online:
    """清单与读写一律用**桶内完整 key**（含车道前缀）——多车道下这是唯一无歧义的形状。"""

    def __init__(self, lanes: tuple[str, ...] = SOURCE_LANES):
        self.lanes = lanes
        self._cache: Path | None = None

    def listing(self) -> list[tuple[str, str]]:
        rows: list[tuple[str, str]] = []
        for lane in self.lanes:
            # --use-server-modtime: 没有它 rclone 为了 `t` 会**逐对象发 HEAD**,
            # 46 万对象跑不完（同 weekly_report 的那条命门注释）。
            out = subprocess.run(
                RCLONE + ["lsf", "-R", "--files-only", "--use-server-modtime",
                          "--format", "tp", f"r2:{bucket()}/{lane}"],
                capture_output=True, text=True, env={**os.environ, "TZ": "UTC"})
            if out.returncode != 0:
                die(f"rclone lsf {lane} 失败：{out.stderr.strip()[-500:]}")
            n = 0
            for ln in out.stdout.splitlines():
                if ";" in ln:
                    ts, path = ln.split(";", 1)
                    rows.append((ts, f"{lane}/{path}"))
                    n += 1
            print(f"[listing] {lane}: {n} 对象", flush=True)
        return rows

    def prefetch(self, rels: list[str]) -> None:
        """整批拉到本地临时目录, 之后 read() 走本地。**这是替掉逐对象 cat 的那一刀。**"""
        if self._cache is not None:
            shutil.rmtree(self._cache, ignore_errors=True)
        self._cache = None
        if not rels:
            return
        cache = Path(tempfile.mkdtemp(prefix="dm-accept-"))
        t0 = time.monotonic()
        with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
            f.write("\n".join(sorted(set(rels))))
            lst = f.name
        r = subprocess.run(
            RCLONE + ["copy", f"r2:{bucket()}", str(cache), "--files-from", lst,
                      "--transfers", "64", "--checkers", "64", "--no-traverse"],
            capture_output=True, text=True)
        os.unlink(lst)
        if r.returncode != 0:
            # 预取失败不致命: read() 会退回逐对象 cat（慢, 但仍然出得了报告）。
            print(f"⚠ 批量预取失败, 退回逐对象读: {r.stderr.strip()[-300:]}",
                  file=sys.stderr)
            shutil.rmtree(cache, ignore_errors=True)
            return
        self._cache = cache
        print(f"[prefetch] {len(set(rels))} 个对象 / {time.monotonic()-t0:.0f}s",
              flush=True)

    def read(self, rel: str) -> bytes | None:
        if self._cache is not None:
            local = self._cache / rel
            if local.exists():
                return local.read_bytes()
        out = subprocess.run(RCLONE + ["cat", f"r2:{bucket()}/{rel}"],
                             capture_output=True)
        return out.stdout if out.returncode == 0 else None

    def write(self, rel: str, data: bytes) -> bool:
        p = subprocess.run(RCLONE + ["rcat", f"r2:{bucket()}/{rel}"],
                           input=data, capture_output=True)
        if p.returncode != 0:
            print(f"⚠ 写回失败 {rel}: {p.stderr.decode()[-300:]}", file=sys.stderr)
        return p.returncode == 0

    def cleanup(self) -> None:
        if self._cache is not None:
            shutil.rmtree(self._cache, ignore_errors=True)
            self._cache = None


class Offline:
    """--listing + --payload-dir。写回落到 payload-dir（测试可断言）。"""

    def __init__(self, listing_file: str, payload_dir: str | None):
        self._rows = []
        for ln in Path(listing_file).read_text().splitlines():
            if not ln.strip():
                continue
            ts, _, path = ln.partition(";")
            self._rows.append((ts if path else "", path or ts))
        self.root = Path(payload_dir) if payload_dir else None

    def listing(self):
        return self._rows

    def prefetch(self, rels: list[str]) -> None:
        return None                      # 离线后端本来就是本地读

    def cleanup(self) -> None:
        return None

    def read(self, rel: str) -> bytes | None:
        if self.root is None:
            return None
        try:
            return (self.root / rel).read_bytes()
        except OSError:
            return None

    def write(self, rel: str, data: bytes) -> bool:
        if self.root is None:
            return False
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        return True


def _unzstd(blob: bytes) -> bytes:
    import zstandard
    return zstandard.ZstdDecompressor().stream_reader(io.BytesIO(blob)).read()


# --------------------------------------------------------------------------- #
# 单 session 验收
# --------------------------------------------------------------------------- #
def accept_session(backend, session: str, rows: list[dict]) -> dict:
    """rows: 该 session 的对象清单 [{rel, kind, sha, run, clip}]。"""
    clips: dict[tuple, dict] = defaultdict(dict)
    for r in rows:
        clips[(r["run"], r["clip"])][r["kind"]] = r

    findings: list[str] = []
    complete: list[tuple] = []
    audio_present = 0
    for key, kinds in sorted(clips.items()):
        if "audio" in kinds:
            audio_present += 1
        missing = [k for k in REQUIRED if k not in kinds]
        if missing:
            findings.append(f"clip {key[1]} 缺 {missing}")
        else:
            complete.append(key)

    game_counts: dict[str, int] = defaultdict(int)
    frames_rows_by_clip: dict[str, list] = {}
    hours = 0.0
    fps_seen: dict[str, int] = defaultdict(int)
    health = {"duplicate_frames": 0, "queue_drops": 0, "input_overflow": 0,
              "empty_events_clips": 0, "meta_unreadable": 0,
              "manifest_mismatch": 0}

    for key in complete:
        kinds = clips[key]
        meta_raw = backend.read(kinds["meta"]["rel"])
        try:
            meta = json.loads(meta_raw)
        except (TypeError, json.JSONDecodeError):
            health["meta_unreadable"] += 1
            game_counts[MISSING] += 1
            continue
        # manifest ↔ 文件名 sha 三角核对（内容寻址, 无需重下正文）
        man_raw = backend.read(kinds["manifest"]["rel"])
        try:
            man = json.loads(man_raw)
        except (TypeError, json.JSONDecodeError):
            man = None
        if isinstance(man, dict):
            for kind in (k for k in ("video", "frames", "events", "audio", "meta")
                         if k in kinds):
                logical = {"video": "video.mp4", "frames": "frames.jsonl.zst",
                           "events": "events.jsonl.zst", "audio": "audio.pcm.zst",
                           "meta": "meta.json"}[kind]
                declared = str(man.get(logical, "")).removeprefix("sha256:")
                if declared and declared != kinds[kind]["sha"]:
                    health["manifest_mismatch"] += 1
                    findings.append(f"clip {key[1]} {logical} manifest/文件名 sha 不符")
        gid = (meta.get("target") or {}).get("game_id") or MISSING
        game_counts[gid] += 1
        n, num, den = meta.get("n_frames", 0), meta.get("fps_num"), meta.get("fps_den", 1)
        if num:
            hours += float(n) / (float(num) / float(den or 1)) / 3600.0
            fps_seen[f"{num}/{den or 1}"] += 1
        h = meta.get("health") or {}
        health["duplicate_frames"] += int(h.get("duplicate_frames", 0))
        health["queue_drops"] += int(h.get("queue_drops", 0))
        health["input_overflow"] += int(h.get("input_overflow", 0))
        # frames 供 QC（events 9 字节空帧 = 操作空窗, 只计数）
        fr_raw = backend.read(kinds["frames"]["rel"])
        if fr_raw:
            try:
                lines = _unzstd(fr_raw).decode().splitlines()
                frames_rows_by_clip[key[1]] = [json.loads(ln) for ln in lines if ln]
            except Exception:
                findings.append(f"clip {key[1]} frames 解压/解析失败")
        ev_raw = backend.read(kinds["events"]["rel"])
        if ev_raw is not None and len(ev_raw) <= 16:
            health["empty_events_clips"] += 1

    game_id = max(game_counts, key=game_counts.get) if game_counts else MISSING
    mixed = len([g for g in game_counts if g != MISSING]) > 1
    if mixed:
        findings.append(f"session 内混游戏: {dict(game_counts)}")

    qc_block: dict = {"ran": False, "reason": "no game_id or unregistered"}
    if game_id in GOVERNANCE and frames_rows_by_clip:
        try:
            policy = policy_for_game(game_id, "floor")
            qc = evaluate_session(frames_rows_by_clip.values(),
                                  session_id=session, tier=policy)
            qc_block = {"ran": True, "tier": policy.tier, "passed": qc.passed,
                        "failed_checks": sorted(qc.failed_checks),
                        "checks": [c.to_dict() for c in qc.checks]}
        except Exception as e:  # QC 失败不吞掉验收其余部分
            qc_block = {"ran": False, "reason": f"qc error: {e}"}

    passed = (not findings and qc_block.get("passed", False)
              and health["manifest_mismatch"] == 0)
    return {
        "schema": SCHEMA,
        "session": session,
        "generated_by": "tools/session-acceptance/accept.py",
        "clips_total": len(clips),
        "clips_complete": len(complete),
        "audio_present_clips": audio_present,
        "game_id": game_id,
        "games_seen": dict(sorted(game_counts.items())),
        "mixed_session": mixed,
        "hours": round(hours, 4),
        "fps_seen": dict(fps_seen),
        "health": health,
        "qc": qc_block,
        "findings": findings,
        "passed": passed,
    }


def select_targets(last_ts: dict[str, str], reported: set[str],
                   quiet_minutes: int, limit: int,
                   now_epoch: float | None = None) -> tuple[list[str], int]:
    """scan 模式的选片: 静默够久且未验收的 session, **最旧优先**, 每轮封顶 limit。

    返回 (本轮目标, 积压总数)。封顶是 issue #887 的止血: 原来每轮都从积压头部
    重新啃、每轮都在 45:00 被杀, 一轮净进展为 0, 积压只增不减。
    """
    now = time.time() if now_epoch is None else now_epoch
    ready: list[tuple[str, str]] = []
    for sid, ts in last_ts.items():
        if sid in reported:
            continue
        try:
            age_min = (now - time.mktime(time.strptime(
                ts[:19], "%Y-%m-%d %H:%M:%S"))) / 60.0
        except ValueError:
            age_min = quiet_minutes      # 无时间戳清单(离线): 视为已完成
        if age_min >= quiet_minutes:
            ready.append((ts, sid))
    ready.sort()                          # (最后写入时间, sid) 升序 = 最旧优先
    targets = [sid for _, sid in ready]
    return (targets[:limit] if limit else targets), len(ready)


#: accept_session 每个完整 clip 会读的四件（正文 video/audio 从不下载）。
READ_KINDS = ("meta", "manifest", "frames", "events")


def rels_to_prefetch(rows: list[dict]) -> list[str]:
    """一个 session 里 accept_session 将要读的全部对象 key。"""
    by_clip: dict[tuple, dict] = defaultdict(dict)
    for r in rows:
        by_clip[(r["run"], r["clip"])][r["kind"]] = r
    out = []
    for kinds in by_clip.values():
        if any(k not in kinds for k in REQUIRED):
            continue                     # 不完整 clip 只记 finding, 不读正文
        out += [kinds[k]["rel"] for k in READ_KINDS if k in kinds]
    return out


# --------------------------------------------------------------------------- #
def parse_rows(listing: list[tuple[str, str]]):
    by_session: dict[str, list[dict]] = defaultdict(list)
    last_ts: dict[str, str] = {}
    reported: set[str] = set()
    for ts, path in listing:
        path = path.strip()
        if path.endswith(REPORT_REL):
            m = re.search(r"sessions/([^/]+)/", path)
            if m:
                reported.add(m.group(1))
            continue
        m = OBJ.search(path)
        if not m:
            continue
        sid = m.group("session")
        by_session[sid].append({"rel": path, "kind": m.group("kind"),
                                "sha": m.group("sha"), "run": m.group("run"),
                                "clip": m.group("clip"),
                                "prefix": m.group("prefix")})
        if ts:
            last_ts[sid] = max(last_ts.get(sid, ""), ts)
    return by_session, last_ts, reported


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lanes", default=",".join(SOURCE_LANES),
                    help="逗号分隔的源车道前缀（默认两条; 别加 attested-trainable, "
                         "那是派生镜像）")
    ap.add_argument("--session", help="只验收这个 session")
    ap.add_argument("--scan", action="store_true",
                    help="扫描全部「已完成且未验收」的 session（cron 模式）")
    ap.add_argument("--quiet-minutes", type=int, default=15,
                    help="scan 模式的完成启发式: 最后写入静默 ≥N 分钟")
    ap.add_argument("--limit", type=int, default=40,
                    help="scan 模式每轮最多验收几个 session（最旧优先）。跑不完留给"
                         "下一轮——每轮都从头啃、每轮都被 timeout 杀, 积压只增不减"
                         "（issue #887）。0 = 不限")
    ap.add_argument("--upload", action="store_true",
                    help="报告写回 R2 session 前缀（需写凭据; 默认只落 --out-dir）")
    ap.add_argument("--out-dir", default="acceptance-reports")
    ap.add_argument("--listing", help="离线模式: rclone lsf --format tp 清单")
    ap.add_argument("--payload-dir", help="离线模式: 对象本地根目录")
    a = ap.parse_args()

    lanes = tuple(x.strip() for x in a.lanes.split(",") if x.strip())
    backend = (Offline(a.listing, a.payload_dir) if a.listing
               else Online(lanes))
    by_session, last_ts, reported = parse_rows(backend.listing())

    if a.session:
        targets = [a.session] if a.session in by_session else []
        if not targets:
            die(f"session {a.session} 在 {'/'.join(lanes)} 下无对象")
    elif a.scan:
        targets, backlog = select_targets(
            last_ts, reported, a.quiet_minutes, a.limit)
        print(f"待验收 {backlog} 个 session（已报告 {len(reported)} 跳过）；"
              f"本轮取最旧 {len(targets)} 个"
              + (f"，剩 {backlog - len(targets)} 个留给下一轮" if backlog > len(targets)
                 else ""))
    else:
        die("给 --session <uuid> 或 --scan 其一")

    out_root = Path(a.out_dir)
    out_root.mkdir(parents=True, exist_ok=True)
    # 一次把本轮要读的对象全拉下来, 而不是每 clip 4 次 cat（issue #887 病根）。
    backend.prefetch([rel for sid in targets
                      for rel in rels_to_prefetch(by_session[sid])])
    failed = 0
    for sid in targets:
        report = accept_session(backend, sid, by_session[sid])
        text = json.dumps(report, indent=1, ensure_ascii=False) + "\n"
        (out_root / f"{sid}.json").write_text(text)
        state = "PASS" if report["passed"] else "FAIL"
        print(f"[{state}] {sid} game={report['game_id']} "
              f"clips={report['clips_complete']}/{report['clips_total']} "
              f"hours={report['hours']} findings={len(report['findings'])}")
        if not report["passed"]:
            failed += 1
        if a.upload:
            rel = f"{by_session[sid][0]['prefix']}/{REPORT_REL}"
            backend.write(rel, text.encode())
    backend.cleanup()
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
