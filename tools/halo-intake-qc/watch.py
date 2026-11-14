#!/usr/bin/env python3
"""halo-intake-qc 的告警层——把 qc.py 从「人手动跑的巡检」变成「会自己叫人的哨兵」。

动机（research/50 §5.1 实测）：2026-08-14 盘点时发现 prod 摄取最后一次入库停在
2026-08-12 16:10:06Z，**已停摆 44.8 小时而无人发现**；同时 QC 的动作熵与纯 W 两项
早已超线，也**从未触发过任何告警**——因为 qc.py 只把结果打到 stdout，没人跑就没人看。
本脚本加两类判据并接飞书通知：

  1. 停摆告警（本轮最该抓的）——最近一次 clip 入库距今 > --stall-hours（默认 24h，
     按**工作时间**计：北京周六/周日不计入，标注员周末不开工属正常节律）。
     采集停摆是「零新增数据」，分布 QC 反而会因为时间窗内没对象直接 FATAL 退出，
     形态上跟「跑挂了」无法区分，所以必须单独成一条判据、单独取证。
  2. 分布超线告警——116 §8.3 九项判据 + toggle-zoom 签名任一不过（判据逻辑完全复用
     qc.py，本文件不重复实现，只读它的退出码与报告行）。

与 qc.py 的边界：本文件**不含任何判据逻辑**，只做 freshness 探测 + 编排 + 通知。
qc.py（及其后续抽库重构 ml/diogenes_wm/halo_qc.py）保持 0=全过 / 1=有失败 / 2=致命
的退出码契约即可，本文件无需跟着改。

用法：
  ./watch.py                          # 完整巡检（freshness + 分布），有告警则通知
  ./watch.py --dry-run                # 不发通知，只打印将要发出的告警正文
  ./watch.py --stall-only             # 只查停摆（秒级，不下载 clip），适合高频 cron
  ./watch.py --listing tp.txt         # 用离线清单（`rclone lsf --format tp` 格式）跑
                                      # freshness，取证/回放用
  ./watch.py --json                   # 机器可读结果打到 stdout

退出码：0 = 无告警；1 = 有告警（无论通知是否发出去）；2 = 巡检自身失败（凭据/网络）。
通知是可选依赖：没有飞书凭据时降级为 stdout 打印，退出码不变——**告警绝不因为
通知通道不通而被吞掉**。
"""

from __future__ import annotations   # 供 3.9 系统 python 直接跑（`X | None` 注解）

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
QC = HERE / "qc.py"
#: 两条源车道（同 qc.py PREFIXES / accept.py SOURCE_LANES）。只盯 v1 的话,
#: 08-29 车道切换后停摆判据会把"新车道在跑"读成"老车道停了"——反过来也一样,
#: 老车道自然静默会被误报成停摆。两条一起看才是"摄取有没有在进"。
PREFIXES = (
    "raw/v1/non-trainable/dataminer-api-production/sessions",
    "raw/v2/dataminer-api-production/sessions",
)
CLIP_SUFFIX = ".jsonl.zst"
CLIP_MARKER = "/frames-"

# freshness 探测的逐级放宽窗口（小时）。rclone 的 --max-age 是服务端过滤不了的
# 客户端过滤，但配 --fast-list 后代价可接受；从窄到宽第一个命中即停，正常情况
# （数据每小时都在进）只需要最窄的一次列举。
PROBE_WINDOWS_H = [6, 24, 72, 24 * 7, 24 * 30, 24 * 120]

FEISHU_DEFAULT_CHAT = "REPLACE_WITH_CHAT_ID"  # Diogenes 群（非机密）
FEISHU_SKILL_CREDS = Path.home() / ".claude/skills/feishu-notify/credentials.json"

RCLONE_FLAGS = ["--fast-list", "--contimeout", "15s", "--timeout", "300s",
                "--retries", "3", "--low-level-retries", "10"]


def now() -> datetime:
    return datetime.now(timezone.utc)


# --------------------------------------------------------------------------- #
# 判据 1：停摆
# --------------------------------------------------------------------------- #

BJT = timezone(timedelta(hours=8))  # Asia/Shanghai，无夏令时


def working_age_hours(last: datetime, ref: datetime) -> float:
    """停摆时长的**工作时间口径**：北京时间周六/周日整段不计入。

    采集是标注员人工开会话的，周末整段无入库是节律不是事故——08-22/23 与
    08-29/30 两个连续周末实测如此，而墙钟口径每逢周末必翻 24h 阈值：
    周六傍晚起连红 ~16 个 run 到周一早上，飞书 + GHA 邮件双通道刷屏（50 号
    盘点里「被刷到直接停用整条 workflow」的前科正是同款）。工作日的停摆
    仍按 1:1 计时，24h 阈值的抓捕速度不变；跨周末的真实停摆在周一继续
    累计，最迟周二凌晨（北京）照样触发。
    """
    if ref <= last:
        return 0.0
    total = 0.0
    cur = last.astimezone(BJT)
    end = ref.astimezone(BJT)
    while cur < end:
        day_end = (cur + timedelta(days=1)).replace(
            hour=0, minute=0, second=0, microsecond=0)
        seg_end = min(day_end, end)
        if cur.weekday() < 5:  # 周一–周五（北京时区）
            total += (seg_end - cur).total_seconds() / 3600.0
        cur = seg_end
    return total

def parse_tp(lines: list[str]) -> list[tuple[datetime, str]]:
    """解析 `rclone lsf --format tp` 输出（`<RFC3339 时间戳>;<相对路径>`），只留 clip。"""
    out = []
    for ln in lines:
        if ";" not in ln:
            continue
        ts, path = ln.split(";", 1)
        if CLIP_MARKER not in path or not path.endswith(CLIP_SUFFIX):
            continue
        raw = ts.strip().replace(" ", "T", 1)
        # rclone `lsf --format t` 打的是**运行机器本地时区的墙钟，且不带偏移量**
        # （实测 v1.74.4：同一个文件 TZ=UTC 打 `2026-08-23 06:42:48`、
        # TZ=Asia/Shanghai 打 `2026-08-23 14:42:48`，两者都没有 `Z`/`+08:00`）。
        # 所以「无偏移量 = UTC」**只在调用方把 TZ 钉成 UTC 时才成立**——
        # `rclone_lsf()` 正是这么做的，见那里的说明。
        # 带偏移量的输入（`--time-format RFC3339`，rclone ≥1.66）也照吃。
        try:
            dt = datetime.fromisoformat(raw)
        except ValueError:
            continue
        out.append((dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc), path.strip()))
    return out


def rclone_lsf(bucket: str, max_age_h: int) -> list[str]:
    out: list[str] = []
    for prefix in PREFIXES:
        out += _rclone_lsf_one(bucket, prefix, max_age_h)
    return out


def _rclone_lsf_one(bucket: str, prefix: str, max_age_h: int) -> list[str]:
    cmd = ["rclone", "lsf", "-R", "--files-only", "--use-server-modtime",
           "--max-age", f"{max_age_h}h", "--format", "tp",
           f"r2:{bucket}/{prefix}", *RCLONE_FLAGS]
    # **TZ=UTC 是判据的一部分，不是环境洁癖。** rclone 打的时间戳不带偏移量，
    # 解析侧只能按 UTC 解释；不钉 TZ 的话，停摆时长会整体偏移运行机器的时区差：
    # 在 UTC+8 上跑，一次 25h 的真实停摆读成 17h —— **低于 24h 阈值，告警不响**。
    # 这不是假设，是实测：同一份数据，GitHub Actions（TZ=UTC）算出 34.2h、
    # 本机（CST+0800）算出 27.2h，差值恰是 8h。哨兵在 UTC runner 上恰好正确过，
    # 那是巧合不是设计——换台机器跑就会静默漏报，而漏报正是它要防的那件事。
    # 用 `--time-format RFC3339` 也能解，但那要 rclone ≥1.66，
    # Ubuntu 打包的版本未必有；钉 TZ 对所有版本都成立。
    r = subprocess.run(cmd, capture_output=True, text=True,
                       env={**os.environ, "TZ": "UTC"})
    if r.returncode != 0:
        raise RuntimeError(f"rclone lsf 失败（{prefix}, --max-age {max_age_h}h）："
                           f"{r.stderr.strip()[-400:]}")
    return [f"{ts};{prefix}/{rel}" for ln in r.stdout.splitlines()
            if ";" in ln for ts, rel in [ln.split(";", 1)]]


def check_freshness(args) -> dict:
    """返回 {ok, last_ingest, age_hours, severity, detail, window_h}。"""
    if args.listing:
        clips = parse_tp(Path(args.listing).read_text().splitlines())
        window = None
        if not clips:
            return {"ok": False, "last_ingest": None, "age_hours": None,
                    "severity": "critical", "window_h": None,
                    "detail": f"离线清单 {args.listing} 内没有任何 clip 对象"}
    else:
        bucket = os.environ.get("R2_TRAINING_BUCKET", "diogenes-model-training")
        if not os.environ.get("RCLONE_CONFIG_R2_ACCESS_KEY_ID"):
            raise RuntimeError("缺少 RCLONE_CONFIG_R2_* 环境变量"
                               "（见 tools/r2-attested-reclass/README「凭据」）")
        clips, window = [], None
        for w in PROBE_WINDOWS_H:
            if w < args.stall_hours:
                continue          # 比阈值还窄的窗口给不出「没超线」的结论
            clips = parse_tp(rclone_lsf(bucket, w))
            window = w
            if clips:
                break
        if not clips:
            return {"ok": False, "last_ingest": None, "age_hours": None,
                    "severity": "critical", "window_h": window,
                    "detail": f"最近 {window}h（{window/24:.0f} 天）内没有任何 clip 入库——"
                              f"摄取长期停摆，或前缀/凭据配错"}

    last = max(dt for dt, _ in clips)
    age = (now() - last).total_seconds() / 3600.0
    # 阈值比较用工作时间口径（周末不计，见 working_age_hours）；
    # age_hours 保留墙钟原值，供回放/外部监控延续旧字段语义。
    eff = working_age_hours(last, now())
    crit = eff > args.critical_stall_hours
    ok = eff <= args.stall_hours
    weekend_excluded = age - eff > 0.05
    return {
        "ok": ok,
        "last_ingest": last.astimezone(timezone.utc).isoformat().replace("+00:00", "Z"),
        "age_hours": round(age, 1),
        "effective_age_hours": round(eff, 1),
        "severity": "ok" if ok else ("critical" if crit else "warn"),
        "window_h": window,
        "n_clips_in_window": len(clips),
        "detail": (f"最后一次入库 {last:%Y-%m-%d %H:%M:%S}Z，距今 {age:.1f}h"
                   + (f"（计停摆 {eff:.1f}h，北京周末不计）" if weekend_excluded else "")
                   + ("" if ok else f"，超过停摆阈值 {args.stall_hours}h")),
    }


# --------------------------------------------------------------------------- #
# 判据 2：分布（完全委托 qc.py）
# --------------------------------------------------------------------------- #

CHECK_RE = re.compile(r"^\s*(?P<mark>[✅❌])\s+(?P<name>.+?)\s{2,}(?P<reading>\S.*?)\s+\((?P<ref>.+)\)\s*$")


def run_distribution(args) -> dict:
    """跑 qc.py，读退出码 + 解析报告行。判据阈值一律以 qc.py 为准，此处不复制。"""
    cmd = [sys.executable, str(QC), "--days", str(args.days),
           "--max-clips", str(args.max_clips), "--seed", str(args.seed)]
    if args.listing:
        cmd += ["--listing", args.listing]
    r = subprocess.run(cmd, capture_output=True, text=True)
    failed, n_checks = [], 0
    for ln in r.stdout.splitlines():
        m = CHECK_RE.match(ln)
        if not m:
            continue
        n_checks += 1
        if m.group("mark") == "❌":
            failed.append(f"{m.group('name').strip()} = {m.group('reading').strip()}"
                          f"（{m.group('ref')}）")
    # 非零退出但一条判据行都没打出来 = QC 自己挂了（缺 rclone/zstd、清单为空、
    # 未捕获异常），不是「数据不合格」。两者都要告警，但不能混为一谈：前者说明
    # 巡检本身失效，正是 research/50 §5.1 那种「没人发现」的成因。
    if r.returncode == 2 or (r.returncode != 0 and n_checks == 0):
        return {"ok": False, "severity": "critical", "exit_code": r.returncode,
                "failed": [], "kind": "harness",
                "detail": "qc.py 自身失败（凭据/依赖/清单/下载），未产出分布报告：\n"
                          + (r.stderr.strip()[-600:] or r.stdout.strip()[-600:]),
                "stdout": r.stdout}
    if r.returncode == 0:
        return {"ok": True, "severity": "ok", "exit_code": 0, "failed": [],
                "kind": "thresholds",
                "detail": "116 §8.3 全部判据通过", "stdout": r.stdout}
    return {"ok": False, "severity": "critical", "exit_code": r.returncode,
            "failed": failed, "kind": "thresholds",
            "detail": (f"{len(failed) or '≥1'} 项分布判据不过"
                       + ("：\n" + "\n".join(f"  · {f}" for f in failed) if failed else "")),
            "stdout": r.stdout}


# --------------------------------------------------------------------------- #
# 通知（可选依赖）
# --------------------------------------------------------------------------- #

def feishu_creds() -> tuple[str, str] | None:
    aid, sec = os.environ.get("PANGMAO_APP_ID"), os.environ.get("PANGMAO_APP_SECRET")
    if aid and sec:
        return aid, sec
    if FEISHU_SKILL_CREDS.exists():
        try:
            c = json.loads(FEISHU_SKILL_CREDS.read_text())
            return c["app_id"], c["app_secret"]
        except Exception:
            return None
    return None


def feishu_post(url: str, payload: dict, token: str | None = None) -> dict:
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=20) as resp:
        return json.load(resp)


def notify(title: str, body: str, severity: str) -> tuple[bool, str]:
    """返回 (是否真的发出去了, 说明)。通知失败不改变告警本身的退出码。"""
    creds = feishu_creds()
    if not creds:
        return False, ("未配置飞书凭据（PANGMAO_APP_ID/PANGMAO_APP_SECRET 或 "
                       f"{FEISHU_SKILL_CREDS}），降级为 stdout")
    chat = os.environ.get("FEISHU_CHAT_ID", FEISHU_DEFAULT_CHAT)
    template = {"critical": "red", "warn": "orange", "ok": "green"}.get(severity, "grey")
    card = {
        "config": {"wide_screen_mode": True},
        "header": {"template": template,
                   "title": {"tag": "plain_text", "content": title}},
        "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": body}}],
    }
    try:
        tok = feishu_post(
            "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal",
            {"app_id": creds[0], "app_secret": creds[1]})["tenant_access_token"]
        resp = feishu_post(
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id",
            {"receive_id": chat, "msg_type": "interactive", "content": json.dumps(card)},
            token=tok)
    except Exception as e:                      # 网络/凭据/接口变更都算通道故障
        return False, f"飞书发送异常：{e}"
    if resp.get("code") != 0:
        return False, f"飞书拒绝：code={resp.get('code')} msg={resp.get('msg')}"
    return True, f"已发送 message_id={resp.get('data', {}).get('message_id')}"


# --------------------------------------------------------------------------- #
# 重复告警抑制。GitHub Actions 的 runner 是一次性的，所以状态得由 workflow 用
# actions/cache 喂进来（见 halo-intake-watch.yml），本文件只认 --state-file。
#
# **`ts` 记的是「上一次真的发出去」的时刻，不是「上一次跑过」的时刻。** 早先的实现
# 每次调用都无条件重写 `ts`，于是 3h 一轮的 cron 让 `now - ts` 恒为 3h < 12h ——
# 首次通知之后**永远**落在抑制窗内，再也不会响。对监控来说这比刷屏更糟：
# 一条持续的停摆只叫一次，之后彻底安静，而安静与「已恢复」在群里长得一模一样。
# --------------------------------------------------------------------------- #

def _read_state(state_file: str) -> dict:
    try:
        return json.loads(Path(state_file).read_text())
    except Exception:
        return {}


def suppressed_until(state_file: str | None, signature: str,
                     repeat_hours: float) -> str | None:
    """同一告警在 `repeat_hours` 内已通知过则返回「可再次通知的时刻」，否则 None。

    只读不写——写入由 `record_notified()` 在**通知确实发出之后**做。这样通道故障
    （飞书挂了 / 凭据缺失）不会占用抑制窗，与本文件卷首「告警绝不因为通知通道
    不通而被吞掉」是同一条纪律。
    """
    if not state_file:
        return None
    st = _read_state(state_file)
    prev_ts, prev_sig = st.get("ts"), st.get("signature")
    if prev_sig != signature or not prev_ts:
        return None
    try:
        quiet_until = datetime.fromisoformat(prev_ts) + timedelta(hours=repeat_hours)
    except ValueError:
        return None
    if now() >= quiet_until:
        return None
    return quiet_until.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def record_notified(state_file: str | None, signature: str) -> None:
    if not state_file:
        return
    p = Path(state_file)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps({"ts": now().isoformat(), "signature": signature}))


# --------------------------------------------------------------------------- #

def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--stall-hours", type=float,
                    default=float(os.environ.get("HALO_QC_STALL_HOURS", 24)),
                    help="最后一次入库距今超过此值即告警（默认 24h）")
    ap.add_argument("--critical-stall-hours", type=float,
                    default=float(os.environ.get("HALO_QC_CRITICAL_STALL_HOURS", 72)),
                    help="超过此值升级为 critical（默认 72h）")
    ap.add_argument("--stall-only", action="store_true", help="跳过分布 QC，只查停摆")
    ap.add_argument("--days", type=int, default=3, help="分布 QC 的时间窗（传给 qc.py）")
    ap.add_argument("--max-clips", type=int, default=240)
    ap.add_argument("--seed", type=int, default=44)
    ap.add_argument("--listing",
                    help="离线 `rclone lsf --format tp` 清单（取证/回放）。⚠️ 生成该清单时"
                         "必须 `TZ=UTC rclone …`（或用 --time-format RFC3339 带上偏移量），"
                         "否则时间戳是生成机的本地墙钟、会被按 UTC 误读")
    ap.add_argument("--dry-run", action="store_true", help="不发通知，只打印告警正文")
    ap.add_argument("--json", action="store_true", help="结果以 JSON 打到 stdout")
    ap.add_argument("--state-file",
                    help="重复告警抑制的状态文件（不给则每轮都通知）。记的是**上次成功"
                         "通知**的时刻；GitHub Actions 上由 actions/cache 跨 run 带过来")
    ap.add_argument("--repeat-hours", type=float, default=12.0,
                    help="同一告警在此时长内不重复通知（需 --state-file）")
    args = ap.parse_args()

    result: dict = {"checked_at": now().isoformat(), "prefixes": list(PREFIXES)}
    try:
        result["freshness"] = check_freshness(args)
    except Exception as e:
        # 哨兵自己挂了也必须叫人——「监控静默失效」正是 research/50 §5.1 那 44.8 小时
        # 无人发现的根因，不能用一次红色 CI run 了事。
        msg = f"FATAL: freshness 探测失败：{e}"
        print(msg, file=sys.stderr)
        if not args.dry_run:
            sent, why = notify("🚨 Halo 摄取巡检自身失败", f"```\n{msg}\n```", "critical")
            print(f"[notify] {'ok' if sent else 'DEGRADED'}: {why}")
        sys.exit(2)

    if args.stall_only:
        result["distribution"] = {"ok": None, "severity": "skipped",
                                  "detail": "--stall-only：本次未跑分布 QC"}
    else:
        result["distribution"] = run_distribution(args)

    fresh, dist = result["freshness"], result["distribution"]
    alerts = [c for c in (fresh, dist) if c.get("ok") is False]
    severity = "critical" if any(a["severity"] == "critical" for a in alerts) else \
               ("warn" if alerts else "ok")
    result["alert"] = bool(alerts)
    result["severity"] = severity

    lines = [
        "**前缀** " + " / ".join(f"`{p}`" for p in PREFIXES),
        "",
        f"**① 采集停摆**　{'✅ 正常' if fresh['ok'] else '❌ 告警'}",
        f"{fresh['detail']}",
        "",
        f"**② 分布判据（116 §8.3）**　"
        f"{'⏭ 跳过' if dist.get('ok') is None else ('✅ 通过' if dist['ok'] else '❌ 告警')}",
        f"{dist['detail']}",
    ]
    body = "\n".join(lines)
    kinds = ([] if fresh["ok"] else ["停摆"]) + ([] if dist.get("ok") is not False
                 else ["QC 失效" if dist.get("kind") == "harness" else "分布超线"])
    title = ("✅ Halo 摄取巡检正常" if not alerts else
             f"{'🚨' if severity == 'critical' else '⚠️'} Halo 摄取告警（{' + '.join(kinds)}）")

    print(f"== {title} ==\n{body}\n")
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))

    if not alerts:
        # 正常态默认静默（不刷群）；需要心跳的场合用 --json 喂给外部监控。
        result["notified"] = False
    elif args.dry_run:
        print("[dry-run] 未发送通知。")
        result["notified"] = False
    else:
        sig = f"{fresh['severity']}|{dist.get('severity')}|{sorted(dist.get('failed', []))}"
        quiet_until = suppressed_until(args.state_file, sig, args.repeat_hours)
        if quiet_until:
            print(f"[suppressed] 同一告警 {args.repeat_hours}h 内已通知过，"
                  f"下次可通知不早于 {quiet_until}。")
            result["notified"], result["suppressed_until"] = False, quiet_until
        else:
            sent, why = notify(title, body, severity)
            result["notified"], result["notify_detail"] = sent, why
            # 只有真的发出去才占抑制窗——发失败时下一轮应当再试。
            if sent:
                record_notified(args.state_file, sig)
            print(f"[notify] {'ok' if sent else 'DEGRADED'}: {why}")

    sys.exit(1 if alerts else 0)


if __name__ == "__main__":
    main()
