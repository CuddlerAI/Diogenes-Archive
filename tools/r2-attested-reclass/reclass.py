#!/usr/bin/env python3
"""把 R2 第一方存量语料复制进具结车道 raw/v1/attested-trainable/ 并落 WAIVER.json。

治理背景见 model_research/research/44 §3（裁决）与 DataMiner/docs/93。要点：
  - 不改写任何封存对象与 DB 行——non-trainable 车道保持逐字节原样；
  - 六件套内容寻址（meta-<sha256>.json 文件名即内容哈希），原地改写必然破坏完整性校验，
    所以「改标识」的唯一诚实实现是换车道：服务端逐字节复制 + 车道根 WAIVER 承载具结；
  - 范围仅第一方前缀；外部开源镜像（diogenes-external-datasets、HF COD 镜像）许可证禁训，
    永远不进本车道。

game_filter（research/50 §4.3 P1 / §6 R1，docs/141 §5）：
  - 生产摄取前缀已多游戏混杂，而 FO4/CP2077 未过权利闸——整前缀复制会造成无人裁决的
    权利越界。带 game_filter 的来源先列举全量对象、逐 session 读一份 meta 的
    catalog.game.id 建 session→game 映射（cache 到本地，重跑幂等），只复制白名单内
    session 的对象；game 读不出的 session 一律排除并计入报告。
  - 对账口径：src 侧 = 白名单清单读数；dst 前缀里可能有历史复制遗留的越界对象
    （research/50 §6 R2/R3），dst ≥ src 属预期。verify 以「白名单内逐对象存在且
    size 一致」为 PASS，并单独报告 dst 侧白名单外对象（只报告不删除，处置另行裁决）。

用法（凭据配置见同目录 README）：
  reclass.py plan               # 只读：枚举来源与目标的对象数/字节数，不动任何数据
  reclass.py execute            # 服务端复制 + 校验 + 写 WAIVER（幂等，可中断重跑）
  reclass.py verify             # 只读：复核 src/dst 读数是否一致
  可选 --source NAME 限定单个来源；--bucket 覆盖默认桶名。
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import subprocess
import sys
import tempfile

LANE_ROOT = "raw/v1/attested-trainable"
WAIVER_KEY = f"{LANE_ROOT}/WAIVER.json"
ATTESTED_AT = "2026-08-10"
DECISION_DOC = "model_research/research/44-halo-100h-playability-eval-and-stock-attestation.md"

# session→game 映射 cache 与越界对象报告的落盘目录（映射由内容寻址对象决定，不可变）。
CACHE_DIR = os.path.expanduser("~/.cache/r2-attested-reclass")

# 第一方存量来源。dst 镜像 src 的目录结构，训练面只从 attested-trainable 车道取数。
SOURCES = [
    {
        "name": "halo-production-ingest",
        "game": "halo-infinite-steam-release-test",
        "src": "raw/v1/non-trainable/dataminer-api-production/sessions",
        "dst": f"{LANE_ROOT}/dataminer-api-production/sessions",
        # 该前缀已多游戏混杂（halo/fallout4/cyberpunk2077/w2/wuchang/cod），FO4 与
        # CP2077 未过权利闸（docs/141 §5）——只放行白名单内 game 的 session。
        "game_filter": ["halo-infinite-steam-release-test"],
        "restrictions": [
            "datasheet_must_disclose_attested_lane",
            "known_label_defects_docs_92_111",  # combat.aim 坏标签 / 无 view_scale / 空帧段
        ],
    },
    {
        # 与上一条同一条产线, 只是摄取车道换了: 2026-08-29 摄取面从 v1 切到
        # raw/v2（issue #855, sessions.schema_version = dm-session-ingest-2）。
        # 不加这条, **切换后新采的 Halo 永远进不了 attested 车道**——旧的还在,
        # 新的静默地不见, 与 docs/167 记的那类失效同形。
        # game_filter / restrictions 与上一条**逐字相同**: 这里扩的是车道,
        # 不是权利面。
        "name": "halo-production-ingest-v2",
        "game": "halo-infinite-steam-release-test",
        "src": "raw/v2/dataminer-api-production/sessions",
        "dst": f"{LANE_ROOT}/dataminer-api-production/sessions",
        "game_filter": ["halo-infinite-steam-release-test"],
        "restrictions": [
            "datasheet_must_disclose_attested_lane",
            "known_label_defects_docs_92_111",
        ],
    },
    {
        "name": "cod-manual-dev",
        "game": "call-of-duty-steam-release-test",
        "src": "raw/v1/non-trainable/manual/DESKTOP-3PG7OEU",
        "dst": f"{LANE_ROOT}/manual/DESKTOP-3PG7OEU",
        # COD 既有 owner 具结（2026-08-01，见 research/33 §13）原为 no_publish +
        # no_product_training；owner 2026-08-10 两次追加明示，两条均已解除
        # （research/44 §3.4）。仅保留裁决要求的 DATASHEET 披露纪律。
        # ⚠ 2026-08-13 起该前缀不再是单游戏：混入 21 个 FO4 depth-lab session
        # （research/50 §2.1），FO4 未过权利闸（docs/141 §5）——必须过滤。
        # manual 布局的 session 键在 non-trainable-sessions/ 之后，game 证据读
        # <sid>/session.json 的 catalog.game.id；设备级文件（journal.sqlite3 等
        # 客户端台账）不属于任何 session，恒放行（08-11 首次复制先例）。
        "game_filter": ["call-of-duty-steam-release-test"],
        "layout": "manual",
        "restrictions": [
            "datasheet_must_disclose_attested_lane",
        ],
    },
]

# 明确排除项，防止未来有人把本脚本当成通用「解禁器」。
EXCLUDED = {
    "diogenes-external-datasets": "外部开源镜像（D2E/CS:GO/GF-Minecraft），非第一方，许可证禁训",
    "hf:DiogenesLab/*": "HF 镜像（COD sample / mv-gameplay），method_validation_only",
    "raw-staging/": "暂存前缀，非封存语料",
}

RCLONE_FLAGS = [
    "--fast-list",
    "--contimeout", "15s",
    "--timeout", "300s",
    "--retries", "3",
    "--low-level-retries", "10",
]


def die(msg: str) -> None:
    print(f"FATAL: {msg}", file=sys.stderr)
    sys.exit(1)


def rclone(args: list[str], capture: bool = True) -> str:
    cmd = ["rclone", *args, *RCLONE_FLAGS]
    res = subprocess.run(cmd, capture_output=capture, text=True)
    if res.returncode != 0:
        err = (res.stderr or "").strip()[-2000:] if capture else ""
        die(f"rclone {' '.join(args[:2])} 失败 (exit {res.returncode})\n{err}")
    return res.stdout if capture else ""


def prefix_size(bucket: str, prefix: str) -> tuple[int, int]:
    """返回 (对象数, 字节数)。前缀不存在时 rclone size 返回 0/0，同样接受。"""
    out = rclone(["size", f"r2:{bucket}/{prefix}", "--json"])
    data = json.loads(out)
    return data["count"], data["bytes"]


def fmt(count: int, nbytes: int) -> str:
    return f"{count:>7} 对象 / {nbytes / 2**30:8.2f} GiB"


def select_sources(name: str | None) -> list[dict]:
    if name is None:
        return SOURCES
    picked = [s for s in SOURCES if s["name"] == name]
    if not picked:
        die(f"未知来源 {name!r}；可选：{', '.join(s['name'] for s in SOURCES)}")
    return picked


# ---------- game_filter：纯函数部分（不碰网络，tests/ 直接覆盖） ----------

def parse_lsf(text: str) -> list[tuple[str, int]]:
    """解析 `rclone lsf -R --files-only --format ps` 输出为 [(相对路径, 字节数)]。"""
    entries = []
    for line in text.splitlines():
        if not line.strip():
            continue
        path, sep, size = line.rpartition(";")
        if not sep or not path:
            die(f"无法解析 lsf 行：{line!r}")
        entries.append((path, int(size)))
    return entries


# manual 布局中不属于任何 session 的设备级文件（journal 台账等）的分组键。
DEVICE_LEVEL = "<device-level>"
MANUAL_SESSIONS_MARK = "non-trainable-sessions/"


def group_sessions(entries: list[tuple[str, int]], layout: str = "api") -> dict[str, list[tuple[str, int]]]:
    """api 布局按相对路径第一段（session uuid）分组；manual 布局按
    non-trainable-sessions/ 之后一段（客户端 session id）分组，标记之外的
    设备级文件归 DEVICE_LEVEL 组。"""
    groups: dict[str, list[tuple[str, int]]] = {}
    for path, size in entries:
        if layout == "manual":
            _, mark, rest = path.partition(MANUAL_SESSIONS_MARK)
            key = rest.split("/", 1)[0] if mark and "/" in rest else DEVICE_LEVEL
        else:
            key = path.split("/", 1)[0]
        groups.setdefault(key, []).append((path, size))
    return groups


def pick_meta(files: list[tuple[str, int]], layout: str = "api") -> str | None:
    """取该 session 的 game 证据文件：api 布局是 meta-*.json（排序取首，确定性；
    manifest- 不算）；manual 布局是 session 根的 session.json（catalog 块同形）。"""
    if layout == "manual":
        cands = sorted(p for p, _ in files if p.endswith("/session.json"))
        return cands[0] if cands else None
    metas = sorted(
        p for p, _ in files
        if p.rsplit("/", 1)[-1].startswith("meta-") and p.endswith(".json")
    )
    return metas[0] if metas else None


def extract_game_id(meta: dict) -> str | None:
    """从 meta.json 读 catalog.game.id；缺 catalog 块或形状不对一律返回 None。"""
    catalog = meta.get("catalog")
    if not isinstance(catalog, dict):
        return None
    game = catalog.get("game")
    if not isinstance(game, dict) or not isinstance(game.get("id"), str):
        return None
    return game["id"]


def totals(entries: list[tuple[str, int]]) -> tuple[int, int]:
    return len(entries), sum(size for _, size in entries)


def build_whitelist(
    groups: dict[str, list[tuple[str, int]]],
    session_games: dict[str, str | None],
    game_filter: list[str],
) -> tuple[list[tuple[str, int]], dict[str, list[int]], list[str]]:
    """返回 (白名单 entries, 排除统计 {game: [session 数, 对象数, 字节数]}, 读不出 game 的 session)。"""
    allowed = set(game_filter)
    whitelist: list[tuple[str, int]] = []
    excluded: dict[str, list[int]] = {}
    unresolved: list[str] = []
    for sid in sorted(groups):
        files = groups[sid]
        if sid == DEVICE_LEVEL:
            # 设备级客户端台账（journal 等），不属于任何 session，恒放行。
            whitelist.extend(sorted(files))
            continue
        game = session_games.get(sid)
        if game in allowed:
            whitelist.extend(sorted(files))
            continue
        if game is None:
            unresolved.append(sid)
        count, nbytes = totals(files)
        stat = excluded.setdefault(game or "<catalog.game.id 读不出>", [0, 0, 0])
        stat[0] += 1
        stat[1] += count
        stat[2] += nbytes
    return whitelist, excluded, unresolved


def diff_against_whitelist(
    whitelist: list[tuple[str, int]], dst_entries: list[tuple[str, int]]
) -> tuple[list[str], list[tuple[str, int]]]:
    """返回 (白名单内在 dst 缺失或 size 不一致的路径, dst 侧白名单外的越界 entries)。"""
    dst = dict(dst_entries)
    wl_paths = {p for p, _ in whitelist}
    bad = [p for p, size in whitelist if dst.get(p) != size]
    out_of_scope = [(p, size) for p, size in dst_entries if p not in wl_paths]
    return bad, out_of_scope


def build_waiver(results: list[dict], refreshed_at: str, generated_at: str) -> dict:
    return {
        "schema": "dm-attested-trainable-waiver-1",
        "attested_at": ATTESTED_AT,
        "refreshed_at": refreshed_at,  # 本次执行（读数刷新）的 UTC 日期；具结时点不变
        "authorizer": "owner (Diogenes)",
        "authorization_channel": "owner 会话指令（2026-08-10）",
        "decision_doc": DECISION_DOC,
        "attestation": (
            "本车道内全部语料为第一方采集（本项目雇佣的采集员在本项目客户端上产出），"
            "owner 具结其已获授权可用于模型训练。原 non-trainable 车道保持逐字节原样，"
            "本车道为服务端复制副本；采集时点的授权链状态以 clip_trainability 视图的"
            "如实记录为准，本具结不改写历史，仅授予前向使用资格。"
        ),
        "quality_disclaimer": (
            "治理解禁不等于质量合格：进训练前须按 DataMiner/docs/92、111 号审计过滤"
            "（mask combat.aim 通道、剔除判废 session、空帧段降权；无 view_scale 标定，"
            "camera 头只能使用 μ-law 计数箱口径）。"
        ),
        "excluded": EXCLUDED,
        "sources": results,
        "generated_at": generated_at,
    }


# ---------- game_filter：网络部分 ----------

def lsf_prefix(bucket: str, prefix: str) -> list[tuple[str, int]]:
    out = rclone(["lsf", "-R", "--files-only", "--format", "ps", f"r2:{bucket}/{prefix}"])
    return parse_lsf(out)


def resolve_session_games(bucket: str, s: dict, groups: dict[str, list[tuple[str, int]]]) -> dict[str, str | None]:
    """逐 session 读一份 meta 得 session→game 映射，cache 到本地 json（内容寻址，映射不可变）。"""
    os.makedirs(CACHE_DIR, exist_ok=True)
    cache_path = os.path.join(CACHE_DIR, f"session-game-{bucket}-{s['name']}.json")
    cache: dict[str, str | None] = {}
    if os.path.exists(cache_path):
        with open(cache_path) as f:
            cache = json.load(f)["sessions"]
    missing = [sid for sid in sorted(groups) if sid not in cache]
    if missing:
        print(f"  session→game：cache 命中 {len(groups) - len(missing)}，新读 {len(missing)} 份 meta …", flush=True)
    for sid in missing:
        meta_rel = pick_meta(groups[sid], s.get("layout", "api"))
        game = None
        if meta_rel is not None:
            try:
                meta = json.loads(rclone(["cat", f"r2:{bucket}/{s['src']}/{meta_rel}"]))
            except json.JSONDecodeError:
                meta = {}
            game = extract_game_id(meta)
        cache[sid] = game
        tmp = cache_path + ".tmp"  # 逐条落盘：中断重跑不重读
        with open(tmp, "w") as f:
            json.dump({"schema": "reclass-session-game-cache-1", "sessions": cache}, f, indent=1)
        os.replace(tmp, cache_path)
    return cache


def filtered_state(bucket: str, s: dict) -> dict:
    """只读：列举 src 全量、解析 session→game、算白名单。plan/execute/verify 共用。"""
    src_entries = lsf_prefix(bucket, s["src"])
    groups = group_sessions(src_entries, s.get("layout", "api"))
    games = resolve_session_games(bucket, s, groups)
    whitelist, excluded, unresolved = build_whitelist(groups, games, s["game_filter"])
    if not whitelist and groups:
        # 前缀下**有** session 却一个都没过白名单 = 过滤值与 meta 的 catalog.game.id
        # 对不上（打错字 / catalog 改名）。这条闸要拦的就是它, 保持致命。
        die(f"{s['name']} game_filter={s['game_filter']} 白名单为空——请核对过滤值与 meta 的 catalog.game.id")
    # 前缀整个是空的则是另一回事: 这条源车道**还没有数据**。新加一条车道时
    # 必然先经历这个状态（如 raw/v2 上目前一个 Halo session 都没有——Halo 08-12
    # 就停采了, 早于 08-29 的车道切换）。此时 die 会把整个工具拖死, 而"提前把
    # 车道挂上、等数据来"正是这次要修的那类失效的反面。
    return {
        "src_entries": src_entries,
        "games": games,
        "whitelist": whitelist,
        "excluded": excluded,
        "unresolved": unresolved,
    }


def print_filter_report(s: dict, st: dict) -> None:
    wc, wb = totals(st["whitelist"])
    print(f"  game_filter={s['game_filter']}")
    print(f"  白名单（src 对账口径）：{fmt(wc, wb)}")
    for game in sorted(st["excluded"]):
        ns, no, nb = st["excluded"][game]
        print(f"  排除 {game}: {ns} session / {fmt(no, nb)}")
    if st["unresolved"]:
        print(f"  其中 {len(st['unresolved'])} 个 session 读不出 catalog.game.id，一律排除：")
        for sid in st["unresolved"]:
            print(f"    - {sid}")


def report_out_of_scope(bucket: str, s: dict, out_of_scope: list[tuple[str, int]],
                        games: dict[str, str | None]) -> tuple[int, int]:
    """报告 dst 侧白名单外对象（research/50 §6 R2/R3 遗留）。只报告不删除，处置另行裁决。"""
    oc, ob = totals(out_of_scope)
    if not out_of_scope:
        print("  dst 越界对象：0（车道内无白名单外对象）")
        return oc, ob
    by_sess: dict[str, list[int]] = {}
    for path, size in out_of_scope:
        stat = by_sess.setdefault(path.split("/", 1)[0], [0, 0])
        stat[0] += 1
        stat[1] += size
    print(f"  dst 越界对象（白名单外、历史复制遗留；只报告不删除）：{fmt(oc, ob)}，{len(by_sess)} 个 session：")
    for sid in sorted(by_sess):
        no, nb = by_sess[sid]
        game = games.get(sid) or "?"
        print(f"    - {sid} (game={game}): {fmt(no, nb)}")
    os.makedirs(CACHE_DIR, exist_ok=True)
    report_path = os.path.join(CACHE_DIR, f"out-of-scope-{bucket}-{s['name']}.json")
    with open(report_path, "w") as f:
        json.dump({
            "schema": "reclass-out-of-scope-report-1",
            "bucket": bucket, "source": s["name"], "dst": s["dst"],
            "game_filter": s["game_filter"],
            "count": oc, "bytes": ob,
            "objects": [{"path": p, "size": size} for p, size in sorted(out_of_scope)],
            "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        }, f, ensure_ascii=False, indent=1)
    print(f"  越界对象全量清单已写入 {report_path}")
    return oc, ob


# ---------- 三个子命令 ----------

def cmd_plan(bucket: str, sources: list[dict]) -> None:
    print(f"桶：{bucket}（只读盘点，不改动任何数据）\n")
    for s in sources:
        print(f"[{s['name']}] game={s['game']}")
        if s.get("game_filter"):
            sc, sb = prefix_size(bucket, s["src"])
            print(f"  src {s['src']}\n      全前缀 {fmt(sc, sb)}（多游戏混杂，按白名单口径对账）")
            st = filtered_state(bucket, s)
            print_filter_report(s, st)
            dc, db = prefix_size(bucket, s["dst"])
            print(f"  dst {s['dst']}\n      {fmt(dc, db)}  （execute 后应 ≥ 白名单读数；差额为历史越界对象，verify 单独报告）")
        else:
            sc, sb = prefix_size(bucket, s["src"])
            dc, db = prefix_size(bucket, s["dst"])
            print(f"  src {s['src']}\n      {fmt(sc, sb)}")
            print(f"  dst {s['dst']}\n      {fmt(dc, db)}  （execute 后应与 src 一致）")
        print(f"  随行限制：{', '.join(s['restrictions'])}\n")
    print("明确排除（永不进入具结车道）：")
    for k, v in EXCLUDED.items():
        print(f"  - {k}: {v}")


def copy_source(bucket: str, s: dict) -> dict:
    if s.get("game_filter"):
        return copy_source_filtered(bucket, s)
    print(f"[{s['name']}] 服务端复制 {s['src']} -> {s['dst']} …", flush=True)
    # --size-only 是幂等性的关键：R2 服务端 CopyObject 产生新的 LastModified，而摄取
    # API 上传的源对象没有 rclone mtime 元数据，按 modtime 比对每次重试都会整树重拷
    # （实测一次 150 GiB 前缀被重拷 3 遍）。本语料文件名内嵌内容 SHA-256，
    # 「同名同尺寸」即同内容，size-only 判等是充分的。
    rclone(
        [
            "copy", f"r2:{bucket}/{s['src']}", f"r2:{bucket}/{s['dst']}",
            "--size-only", "--transfers", "24", "--checkers", "32",
            "--stats", "60s", "--stats-one-line", "--stats-log-level", "NOTICE",
        ],
        capture=False,
    )
    sc, sb = prefix_size(bucket, s["src"])
    dc, db = prefix_size(bucket, s["dst"])
    ok = (sc, sb) == (dc, db)
    print(f"[{s['name']}] src {fmt(sc, sb)} | dst {fmt(dc, db)} | {'一致 ✅' if ok else '不一致 ❌'}")
    if not ok:
        die(f"{s['name']} 复制后 src/dst 读数不一致——不写 WAIVER，请重跑 execute（幂等增量）")
    return {
        "name": s["name"], "game": s["game"], "src": s["src"], "dst": s["dst"],
        "objects": sc, "bytes": sb, "restrictions": s["restrictions"],
    }


def copy_source_filtered(bucket: str, s: dict) -> dict:
    print(f"[{s['name']}] 列举 src 全量并解析 session→game …", flush=True)
    st = filtered_state(bucket, s)
    if not st["whitelist"]:
        print(f"[{s['name']}] src 前缀 {s['src']} 下没有对象——跳过（该车道尚无数据）",
              flush=True)
        return {
            "name": s["name"], "game": s["game"], "src": s["src"], "dst": s["dst"],
            "objects": 0, "bytes": 0, "restrictions": s["restrictions"],
            "skipped": "source_prefix_empty",
        }
    print_filter_report(s, st)
    wc, wb = totals(st["whitelist"])
    print(f"[{s['name']}] 服务端复制白名单 {s['src']} -> {s['dst']} …", flush=True)
    fd, listfile = tempfile.mkstemp(suffix=".files-from.txt")
    try:
        with os.fdopen(fd, "w") as f:
            f.writelines(path + "\n" for path, _ in st["whitelist"])
        rclone(
            [
                "copy", f"r2:{bucket}/{s['src']}", f"r2:{bucket}/{s['dst']}",
                "--files-from", listfile,
                "--size-only", "--transfers", "24", "--checkers", "32",
                "--stats", "60s", "--stats-one-line", "--stats-log-level", "NOTICE",
            ],
            capture=False,
        )
    finally:
        os.unlink(listfile)
    dst_entries = lsf_prefix(bucket, s["dst"])
    dc, db = totals(dst_entries)
    bad, out_of_scope = diff_against_whitelist(st["whitelist"], dst_entries)
    print(f"[{s['name']}] src 白名单 {fmt(wc, wb)} | dst 前缀 {fmt(dc, db)}（dst ≥ 白名单属预期）")
    if bad:
        for path in bad[:10]:
            print(f"    缺失/size 不一致: {path}")
        die(f"{s['name']} 白名单内 {len(bad)} 个对象在 dst 缺失或 size 不一致——不写 WAIVER，请重跑 execute（幂等增量）")
    oc, ob = report_out_of_scope(bucket, s, out_of_scope, st["games"])
    print(f"[{s['name']}] 白名单内逐对象存在且 size 一致 ✅")
    return {
        "name": s["name"], "game": s["game"], "src": s["src"], "dst": s["dst"],
        "game_filter": s["game_filter"],
        "objects": wc, "bytes": wb,  # 白名单口径，不是 src 全前缀
        # execute 复制后即做逐对象核对，越界读数当场可得，直接回填（无需 verify 二次改写 WAIVER）
        "known_out_of_scope_in_lane": {"objects": oc, "bytes": ob},
        "restrictions": s["restrictions"],
    }


def write_waiver(bucket: str, results: list[dict]) -> None:
    now = datetime.datetime.now(datetime.timezone.utc)
    waiver = build_waiver(
        results,
        refreshed_at=now.date().isoformat(),
        generated_at=now.isoformat(timespec="seconds"),
    )
    body = json.dumps(waiver, ensure_ascii=False, indent=2)
    subprocess.run(
        ["rclone", "rcat", f"r2:{bucket}/{WAIVER_KEY}", *RCLONE_FLAGS],
        input=body, text=True, check=True,
    )
    print(f"WAIVER 已写入 r2:{bucket}/{WAIVER_KEY}")


def cmd_execute(bucket: str, sources: list[dict]) -> None:
    results = [copy_source(bucket, s) for s in sources]
    if len(sources) != len(SOURCES):
        print("（--source 限定运行：WAIVER 需在全部来源复制完成后统一生成，本次不写。）")
        return
    write_waiver(bucket, results)


def cmd_verify(bucket: str, sources: list[dict]) -> None:
    bad_sources = 0
    for s in sources:
        if s.get("game_filter"):
            st = filtered_state(bucket, s)
            print_filter_report(s, st)
            wc, wb = totals(st["whitelist"])
            dst_entries = lsf_prefix(bucket, s["dst"])
            dc, db = totals(dst_entries)
            bad, out_of_scope = diff_against_whitelist(st["whitelist"], dst_entries)
            print(f"[{s['name']}] src 白名单 {fmt(wc, wb)} | dst 前缀 {fmt(dc, db)}")
            report_out_of_scope(bucket, s, out_of_scope, st["games"])
            if bad:
                bad_sources += 1
                for path in bad[:10]:
                    print(f"    缺失/size 不一致: {path}")
                print(f"[{s['name']}] 白名单内 {len(bad)} 个对象缺失或 size 不一致 ❌")
            else:
                print(f"[{s['name']}] 白名单内逐对象存在且 size 一致 ✅")
        else:
            sc, sb = prefix_size(bucket, s["src"])
            dc, db = prefix_size(bucket, s["dst"])
            ok = (sc, sb) == (dc, db)
            bad_sources += 0 if ok else 1
            print(f"[{s['name']}] src {fmt(sc, sb)} | dst {fmt(dc, db)} | {'一致 ✅' if ok else '不一致 ❌'}")
    if bad_sources:
        die(f"{bad_sources} 个来源读数不一致（若 src 仍在持续摄取，属预期增量；重跑 execute 即可追平）")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", choices=["plan", "execute", "verify"], nargs="?", default="plan")
    ap.add_argument("--bucket", default=os.environ.get("R2_TRAINING_BUCKET", "diogenes-model-training"))
    ap.add_argument("--source", help="限定单个来源（见 SOURCES）")
    args = ap.parse_args()

    if not os.environ.get("RCLONE_CONFIG_R2_ACCESS_KEY_ID"):
        die("缺少 RCLONE_CONFIG_R2_* 环境变量；凭据配置见同目录 README")

    sources = select_sources(args.source)
    {"plan": cmd_plan, "execute": cmd_execute, "verify": cmd_verify}[args.command](args.bucket, sources)


if __name__ == "__main__":
    main()
