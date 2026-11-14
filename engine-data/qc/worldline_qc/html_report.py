"""单文件 HTML 报告（中文）：顶部判定 → 12 路一览 → 需求条款↔结论总表 → 曲线 → 缩略图 → 规则明细 → 来源与口径。

样式参考 DataMiner/tools/quality-check/render_report.py；不依赖外部资源，可直接发给业务方或截图。
"""

from __future__ import annotations

import html
from datetime import datetime
from typing import Dict, List, Optional, Sequence

from .model import FAIL, MANUAL, PASS, SKIP, STATUS_ZH, UNVERIFIED, WARN, GroupReport, RuleResult, ROLE_ZH
from .requirements import evaluate

CSS = """
:root{--bg:#fff;--fg:#16181d;--mut:#5b6270;--line:#e2e5ea;--head:#f4f6f8;
--pass:#0a7f43;--passbg:#e9f7ef;--fail:#b3261e;--failbg:#fdecea;--warn:#8a6100;--warnbg:#fff6e2;
--manual:#3b4fb8;--manualbg:#eceff9;--skip:#6b7280;--skipbg:#f1f2f4;--unv:#a34a00;--unvbg:#ffefe3}
*{box-sizing:border-box}
body{margin:0;padding:28px 34px 40px;font:14px/1.55 -apple-system,"PingFang SC","Helvetica Neue",Arial,sans-serif;color:var(--fg);background:var(--bg)}
h1{font-size:22px;margin:0 0 6px}h2{font-size:17px;margin:34px 0 8px;padding-bottom:6px;border-bottom:2px solid var(--line)}
h3{font-size:14.5px;margin:18px 0 6px}
.meta{color:var(--mut);font-size:12.5px;margin:0 0 12px}.meta code{background:#f2f4f7;padding:1px 5px;border-radius:4px;font-size:12px}
.chip{display:inline-block;padding:2px 9px;border-radius:999px;font-weight:650;font-size:12.5px;border:1px solid transparent;white-space:nowrap}
.chip.PASS{color:var(--pass);background:var(--passbg)}.chip.FAIL{color:var(--fail);background:var(--failbg)}
.chip.WARN{color:var(--warn);background:var(--warnbg)}.chip.MANUAL{color:var(--manual);background:var(--manualbg)}
.chip.SKIP{color:var(--skip);background:var(--skipbg)}.chip.UNVERIFIED{color:var(--unv);background:var(--unvbg)}
.chip.big{font-size:16px;padding:5px 14px}
.banner{margin:12px 0 18px;padding:12px 16px;border-radius:8px;border:1px solid;font-size:13.5px;line-height:1.6}
.banner.FAIL{background:var(--failbg);border-color:#f0b4af;color:#7d1a15}.banner.PASS{background:var(--passbg);border-color:#a8dcc0;color:#0a5c33}
.banner.UNVERIFIED{background:var(--unvbg);border-color:#f3c9a6;color:#7a3700}
nav a{margin-right:14px;font-size:13px}
table{border-collapse:collapse;width:100%;font-size:12.5px}
th,td{border:1px solid var(--line);padding:5px 8px;text-align:left;vertical-align:top;word-break:break-word}
th{background:var(--head);font-weight:600;color:#39404d;position:sticky;top:0}
td.st{white-space:nowrap;text-align:center}
tr.FAIL td{background:#fff8f7}tr.UNVERIFIED td{background:#fff9f4}
.sub{color:var(--mut);font-size:12px;margin:0 0 9px;line-height:1.55}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:14px}
.card{border:1px solid var(--line);border-radius:8px;padding:10px 12px;background:#fff}
.card h4{margin:0 0 6px;font-size:13px}
.thumbs{display:flex;flex-wrap:wrap;gap:6px}.thumbs figure{margin:0;width:160px}.thumbs img{width:160px;height:90px;object-fit:cover;border:1px solid var(--line);border-radius:4px}
.thumbs figcaption{font-size:11px;color:var(--mut);text-align:center}
svg text{font-size:10px;fill:#5b6270}
details summary{cursor:pointer;color:#39404d;font-weight:600;margin:8px 0}
ul.tight{margin:4px 0 4px 18px;padding:0}ul.tight li{margin:2px 0}
.ev{font-size:12px;color:#39404d}
footer{margin-top:30px;padding-top:10px;border-top:1px solid var(--line);color:var(--mut);font-size:11.5px;line-height:1.6}
.small{font-size:11.5px;color:var(--mut)}
"""

SOURCES = [
    ("wiki MszLw2Drdi2EQWklQ0pclf1onjc", "《【Multi-Camera】游戏引擎数据采集新需求 for 外部》（09-22 修订，§1–§7.3）"),
    ("wiki Pq7Gwymw6i5ZvzkM1KccsIfcnkf", "《Multi-camera 12 路相机数据交付说明 for外部》（平铺布局、编号 1～12、命名）"),
    ("sheet Th4uwL95hiq3zLk9UeocwwxlnWS", "《心影-Multicam3正式交付表》（链路测试/元信息/镜头分类备注/交付数据 4 个 tab 的表头）"),
    ("docx KBghdFXYRoJBSQxJG5Mc8Wguncc", "《智能纪要：多相机及demo整改落地部署会 2026-09-22》"),
    ("wiki U12uw6nbBiDSmrkvIhOcuETWnKb", "《心影随形-Multicam3期游戏多样性》（空模板表，无 QC 条款）"),
    ("wiki QuwcwtuYgiVEIakYWckcOmppnAh", "《心影随形-mu3-demo跟进表》（demo1–demo4 反馈；demo4 = 09-22 提交的本 GTA5 demo，8 条反馈）"),
    ("minutes obcn6mpq5136dyl9by53uf3i", "《【心影随形】游戏数据资源沟通--外部的视频会议》妙记（09-21，AI 纪要）"),
    ("用户确认点①–④", "小动作分叉前一致 / 交互键中文语义 / 真人优先脚本须自然 / C 接管 N 整数秒 ∈[0,55]"),
    ("DataMiner/docs/69", "二期合同：p95 上限、cam4、共视、验收表"),
]

FACES = [("pack", "打包与目录"), ("sysinfo", "systeminfo"), ("video", "视频容器"), ("json", "action_camera 字段"),
         ("sync", "同世界线同步"), ("cam", "相机语义"), ("quota", "批次配额"), ("motion", "运镜运动参数"),
         ("input", "真实输入↔运动"), ("geo", "视锥几何代理"), ("wl", "世界线与因果记录"), ("pix", "像素面"), ("manual", "人工复核清单")]


def _e(s) -> str:
    return html.escape(str(s), quote=True)


def chip(st: str, big=False) -> str:
    return f'<span class="chip {st}{" big" if big else ""}">{_e(STATUS_ZH.get(st, st))}·{_e(st)}</span>'


def _svg_lines(series: Sequence[tuple], width=640, height=150, ymax: Optional[float] = None, yfmt="{:.0%}",
               markers: Sequence[tuple] = (), xlabel="秒", ylabel="") -> str:
    """series: [(label, values, color)]；values 按秒。markers: [(x, label)]。"""
    pad_l, pad_r, pad_t, pad_b = 42, 10, 12, 22
    n = max((len(v) for _, v, _ in series), default=0)
    if n < 2:
        return '<div class="small">（数据不足）</div>'
    if ymax is None:
        ymax = max((max(v) for _, v, _ in series if v), default=1.0) or 1.0
        ymax *= 1.1
    w, h = width - pad_l - pad_r, height - pad_t - pad_b

    def X(i):
        return pad_l + w * i / (n - 1)

    def Y(v):
        return pad_t + h - h * min(v, ymax) / ymax

    parts = [f'<svg viewBox="0 0 {width} {height}" width="100%" preserveAspectRatio="none" style="max-width:{width}px;height:{height}px">']
    for k in range(5):
        yv = ymax * k / 4
        parts.append(f'<line x1="{pad_l}" y1="{Y(yv):.1f}" x2="{width - pad_r}" y2="{Y(yv):.1f}" stroke="#e8eaee"/>'
                     f'<text x="{pad_l - 4}" y="{Y(yv) + 3:.1f}" text-anchor="end">{_e(yfmt.format(yv))}</text>')
    step = max(1, n // 8)
    for i in range(0, n, step):
        parts.append(f'<text x="{X(i):.1f}" y="{height - 6}" text-anchor="middle">{i}</text>')
    parts.append(f'<text x="{width - pad_r}" y="{height - 6}" text-anchor="end">{_e(xlabel)}</text>')
    for label, vals, color in series:
        pts = " ".join(f"{X(i):.1f},{Y(v):.1f}" for i, v in enumerate(vals))
        parts.append(f'<polyline fill="none" stroke="{color}" stroke-width="1.6" points="{pts}"><title>{_e(label)}</title></polyline>')
    for x, lab in markers:
        if 0 <= x <= n - 1:
            parts.append(f'<line x1="{X(x):.1f}" y1="{pad_t}" x2="{X(x):.1f}" y2="{pad_t + h}" stroke="#b3261e" stroke-dasharray="4 3"/>'
                         f'<text x="{X(x) + 3:.1f}" y="{pad_t + 10}" fill="#b3261e">{_e(lab)}</text>')
    lx = pad_l
    for label, _, color in series:
        parts.append(f'<rect x="{lx}" y="{pad_t - 10}" width="10" height="3" fill="{color}"/><text x="{lx + 13}" y="{pad_t - 6}">{_e(label)}</text>')
        lx += 14 + 7 * len(label) + 12
    parts.append("</svg>")
    return "".join(parts)


PALETTE = ["#2b6cb0", "#c05621", "#2f855a", "#805ad5", "#b83280", "#0987a0"]


def _lane_label(info) -> str:
    return info.label


def _rules_for(rep: GroupReport, target: str) -> List[RuleResult]:
    return [r for r in rep.results if r.target == target]


def _lanes_table(rep: GroupReport, art: dict) -> str:
    vids = rep.meta.get("videos", {})
    rows = []
    for info in rep.lanes:
        key = info.key
        v = vids.get(key) or {}
        rs = _rules_for(rep, key)

        def ev(rule):
            for r in rs:
                if r.rule == rule:
                    return r.evidence
            return "—"

        focal = f"{info.focal_mm:g} mm" if info.focal_mm else "—"
        first_thumb = ""
        th = (art.get("thumbs") or {}).get(key) or []
        if th:
            first_thumb = f'<img src="data:image/jpeg;base64,{th[len(th) // 2][1]}" style="width:120px;height:68px;object-fit:cover;border-radius:3px">'
        n_fail = sum(1 for r in rs if r.status == FAIL)
        n_warn = sum(1 for r in rs if r.status == WARN)
        rows.append("<tr>"
                    f"<td>#{info.global_index or '—'}</td><td>{_e(info.take)}/{info.lane}</td><td>{_e(info.worldline)}</td>"
                    f"<td>{_e(ROLE_ZH.get(info.role, info.role))}</td><td>{_e(focal)}</td>"
                    f"<td>{v.get('width', '—')}×{v.get('height', '—')}</td>"
                    f"<td>{v.get('nb_frames', '—')} / {round(v['duration'], 2) if v.get('duration') else '—'} s</td>"
                    f"<td>{_e(ev('json.game_fps').replace('游戏运行帧率 ', ''))}</td>"
                    f"<td>{'FAIL ' + str(n_fail) if n_fail else '—'} / {'WARN ' + str(n_warn) if n_warn else '—'}</td>"
                    f"<td>{first_thumb}</td></tr>")
    return ("<div style='overflow-x:auto'><table style='min-width:900px'><thead><tr><th>需方编号</th><th>文件夹/路</th><th>世界线</th><th>相机角色</th><th>焦距</th><th>分辨率</th>"
            "<th>帧数 / 时长</th><th>游戏 fps</th><th>本路 FAIL/WARN</th><th>中段缩略图</th></tr></thead><tbody>" + "".join(rows) + "</tbody></table></div>")


def _clauses_table(rep: GroupReport) -> str:
    rows = []
    for row in evaluate(rep.results):
        c, st, matched, counts = row["clause"], row["status"], row["matched"], row["counts"]
        cnt = " ".join(f'<span class="chip {k}">{_e(STATUS_ZH[k])} {v}</span>' for k, v in sorted(counts.items(), key=lambda kv: kv[0]))
        # 证据：按状态严重度排，最多 6 条
        order = {FAIL: 0, UNVERIFIED: 1, WARN: 2, MANUAL: 3, PASS: 4, SKIP: 5}
        ms = sorted(matched, key=lambda r: order.get(r.status, 9))
        evs = "".join(f'<li><span class="chip {r.status}">{_e(r.status)}</span> <b>{_e(r.rule)}</b> @ {_e(r.target)}：{_e(r.evidence)}</li>' for r in ms[:6])
        more = f'<details><summary class="small">还有 {len(ms) - 6} 条</summary><ul class="tight">' + "".join(
            f'<li><span class="chip {r.status}">{_e(r.status)}</span> <b>{_e(r.rule)}</b> @ {_e(r.target)}：{_e(r.evidence)}</li>' for r in ms[6:]) + "</ul></details>" if len(ms) > 6 else ""
        if not matched:
            evs = f"<li class='small'>无自动规则命中；按默认口径记为 {_e(STATUS_ZH[c.default])}</li>"
        rows.append(f'<tr class="{st}"><td><b>{_e(c.id)}</b><br><span class="small">{_e(c.section)}</span></td>'
                    f'<td>{_e(c.text)}<br><span class="small">适用：{_e(c.scope)}</span></td>'
                    f'<td class="small">{_e(c.method)}</td><td class="st">{chip(st)}<br>{cnt}</td>'
                    f'<td class="ev"><ul class="tight">{evs}</ul>{more}</td></tr>')
    return ("<div style='overflow-x:auto'><table style='min-width:1100px'><thead><tr><th style='width:8%'>条款</th><th style='width:24%'>要求</th>"
            "<th style='width:13%'>判定方式</th><th style='width:11%'>结论</th><th>证据</th></tr></thead><tbody>" + "".join(rows) + "</tbody></table></div>")


def _charts(rep: GroupReport, art: dict) -> str:
    out = []
    xt = art.get("xtake") or {}
    if xt:
        out.append("<h3>固定机位跨遍逐秒变化像素占比（A 遍 ↔ C/D 遍，|Δ|>20 的像素比例）</h3>"
                   "<p class='sub'>接管/干预前应只剩海浪、植被等环境动画的本底噪声；接管后差异应局部化并且起点不早于接管时刻（红虚线）。</p>")
        series, markers = [], []
        for i, (k, v) in enumerate(xt.items()):
            series.append((f"{k}（世界线{v.get('worldline')}）", v["per_sec"], PALETTE[i % len(PALETTE)]))
            if v.get("takeover_s") is not None:
                markers.append((v["takeover_s"], f"{v.get('worldline')} 接管 {v['takeover_s']:g}s"))
        out.append(_svg_lines(series, ymax=max(0.3, max(max(v["per_sec"]) for v in xt.values()) * 1.15), markers=markers))
    # 共视时间线
    cov = [r for r in rep.results if r.rule == "geo.covis" and r.data and "timeline" in r.data]
    if cov:
        out.append("<h3>共视时间线（同一逻辑帧看到同一主体的路数，几何代理）</h3>")
        series = []
        for i, r in enumerate(cov):
            tl = r.data["timeline"]
            per_sec = [sum(tl[k * 30:(k + 1) * 30]) / 30 for k in range(len(tl) // 30)]
            series.append((r.target, per_sec, PALETTE[i % len(PALETTE)]))
        out.append(_svg_lines(series, ymax=4.4, yfmt="{:.0f} 路"))
    # 运动像素
    m1 = art.get("motion_1s") or {}
    if m1:
        out.append("<h3>逐秒运动像素占比（|Δ|>12 的像素比例，逐帧均值）</h3><div class='grid'>")
        for key, vals in m1.items():
            info = next((l for l in rep.lanes if l.key == key), None)
            out.append(f"<div class='card'><h4>{_e(info.label if info else key)}</h4>" + _svg_lines([("运动像素", vals, "#2b6cb0")], width=300, height=110, ymax=1.0) + "</div>")
        out.append("</div>")
    # 速度
    sp = [r for r in rep.results if r.rule == "motion.segments" and r.data and r.data.get("speed_1s")]
    if sp:
        out.append("<h3>逐秒平移速度与 1 s 窗运镜类型</h3><div class='grid'>")
        for r in sp:
            info = next((l for l in rep.lanes if l.key == r.target), None)
            kinds = r.data.get("kinds_1s", [])
            strip = "".join(f'<span title="{i}s {k}" style="display:inline-block;width:{max(2, 280 // max(1, len(kinds)))}px;height:8px;background:{ {"静止": "#cbd5e0", "纯平移": "#2b6cb0", "纯转动": "#c05621", "平移+转动": "#2f855a"}.get(k, "#999")}"></span>' for i, k in enumerate(kinds))
            out.append(f"<div class='card'><h4>{_e(info.label if info else r.target)}</h4>"
                       + _svg_lines([("m/s", r.data["speed_1s"], "#2f855a")], width=300, height=110, yfmt="{:.1f}")
                       + f"<div>{strip}</div><div class='small'>灰=静止 蓝=纯平移 橙=纯转动 绿=平移+转动</div></div>")
        out.append("</div>")
    return "".join(out) if out else "<p class='sub'>无曲线数据（像素面未跑或无位姿）。</p>"


def _thumbs(rep: GroupReport, art: dict) -> str:
    th = art.get("thumbs") or {}
    if not th:
        return "<p class='sub'>无缩略图（像素面未跑）。</p>"
    out = []
    for info in rep.lanes:
        items = th.get(info.key) or []
        if not items:
            continue
        figs = "".join(f'<figure><img src="data:image/jpeg;base64,{b64}"><figcaption>帧 {fi}（{fi / 30:.1f}s）</figcaption></figure>' for fi, b64 in items)
        out.append(f"<h3>{_e(info.label)}</h3><div class='thumbs'>{figs}</div>")
    return "".join(out)


def _rules_detail(rep: GroupReport) -> str:
    out = []
    for prefix, name in FACES:
        rs = [r for r in rep.results if r.rule.split(".", 1)[0] == prefix]
        if not rs:
            continue
        counts = {}
        for r in rs:
            counts[r.status] = counts.get(r.status, 0) + 1
        cnt = " ".join(f'<span class="chip {k}">{_e(STATUS_ZH[k])} {v}</span>' for k, v in counts.items())
        rows = "".join(f'<tr class="{r.status}"><td><code>{_e(r.rule)}</code></td><td>{_e(r.target)}</td><td class="st">{chip(r.status)}</td>'
                       f'<td>{_e(r.evidence)}</td><td class="small">{_e(r.req)}</td></tr>' for r in rs)
        opened = " open" if any(r.status in (FAIL, UNVERIFIED) for r in rs) else ""
        out.append(f"<details{opened}><summary>{_e(name)}（{len(rs)} 条）{cnt}</summary><table><thead><tr><th style='width:16%'>规则</th>"
                   f"<th style='width:12%'>对象</th><th style='width:9%'>状态</th><th>证据</th><th style='width:14%'>条款</th></tr></thead><tbody>{rows}</tbody></table></details>")
    return "".join(out)


def render_html(rep: GroupReport, art: Optional[dict] = None) -> str:
    art = art or {}
    v = rep.verdict
    c = rep.counts
    fails = [r for r in rep.results if r.status == FAIL]
    unv = [r for r in rep.results if r.status == UNVERIFIED]
    warns = [r for r in rep.results if r.status == WARN]
    manuals = [r for r in rep.results if r.status == MANUAL]
    banner = {FAIL: f"组判定 <b>不达标</b>：{len(fails)} 条硬性条款不通过（下列），另有 {len(warns)} 条待需方确认口径、{len(manuals)} 条须人工看画面。WARN/MANUAL 不阻断判定但必须处置。",
              UNVERIFIED: f"组判定 <b>未能核验</b>：{len(unv)} 条规则该跑没跑（工具链/文件缺失），不得视为通过。",
              PASS: f"自动规则全部通过；另有 {len(warns)} 条待需方确认口径、{len(manuals)} 条须人工看画面。"}[v]
    fail_list = "".join(f"<li><b>{_e(r.rule)}</b> @ {_e(r.target)}：{_e(r.evidence)} <span class='small'>（{_e(r.req)}）</span></li>" for r in fails)
    unv_list = "".join(f"<li><b>{_e(r.rule)}</b> @ {_e(r.target)}：{_e(r.evidence)}</li>" for r in unv)
    # WARN 去重（同规则同证据前缀合并）
    seen, warn_items = set(), []
    for r in warns:
        k = (r.rule, r.evidence[:40])
        if k in seen:
            continue
        seen.add(k)
        same = [x for x in warns if (x.rule, x.evidence[:40]) == k]
        tg = r.target if len(same) == 1 else f"{len(same)} 处（{'、'.join(x.target for x in same[:4])}{'…' if len(same) > 4 else ''}）"
        warn_items.append(f"<li><b>{_e(r.rule)}</b> @ {_e(tg)}：{_e(r.evidence)}</li>")
    manual_items = "".join(f"<li><b>{_e(r.rule)}</b> @ {_e(r.target)}：{_e(r.evidence)}</li>" for r in manuals if r.target == "group")
    meta = rep.meta
    vids = meta.get("videos", {})
    parts = [f"<!doctype html><html lang='zh-CN'><head><meta charset='utf-8'><title>四世界线 QC · {_e(rep.group)}</title><style>{CSS}</style></head><body>",
             f"<h1>Multi-Camera 三期四世界线 QC 报告 · {_e(rep.group)} {chip(v, big=True)}</h1>",
             f"<p class='meta'>{_e(meta.get('tool', 'worldline_qc'))} · 生成 {datetime.now().strftime('%Y-%m-%d %H:%M')} · 数据 <code>{_e(meta.get('root', ''))}</code>"
             f" · 布局 {_e(meta.get('layout', ''))} · 期望时长 {meta.get('expect_duration_s', '')} s · 用时 {meta.get('elapsed_s', '')} s · 规则 {len(rep.results)} 条："
             + " ".join(f'<span class="chip {k}">{_e(STATUS_ZH[k])} {n}</span>' for k, n in c.items() if n) + "</p>",
             f"<div class='banner {v}'>{banner}</div>",
             "<nav><a href='#fails'>不达标</a><a href='#lanes'>12 路一览</a><a href='#clauses'>需求条款总表</a><a href='#warns'>待需方确认</a>"
             "<a href='#manual'>人工复核</a><a href='#charts'>曲线</a><a href='#thumbs'>缩略图</a><a href='#rules'>规则明细</a><a href='#sources'>来源与口径</a></nav>"]
    parts.append("<h2 id='fails'>不达标条目（FAIL）</h2>" + (f"<ul class='tight'>{fail_list}</ul>" if fails else "<p class='sub'>无。</p>"))
    if unv:
        parts.append(f"<h2>未能核验（UNVERIFIED）</h2><ul class='tight'>{unv_list}</ul>")
    parts.append("<h2 id='lanes'>12 路一览</h2><p class='sub'>需方编号按交付说明（A=1–3、B=4、C=5–8、D=9–12）由本包布局推得；角色来自 README 路表；焦距为内参反推档位。</p>" + _lanes_table(rep, art))
    parts.append("<h2 id='clauses'>需求条款 ↔ 是否达标</h2><p class='sub'>条款结论取命中规则里最严重的状态（不达标 &gt; 未能核验 &gt; 待确认 &gt; 需人工 &gt; 达标 &gt; 不适用）；"
                 "没有自动规则的条款按默认口径列出，不会从表里消失。</p>" + _clauses_table(rep))
    parts.append("<h2 id='warns'>待需方确认 / 已知差距（WARN）</h2><p class='sub'>这些项要么需方口径未定（不能写成硬 FAIL），要么是内部演示件的已知差距，正式交付前须逐条处置。</p>"
                 + (f"<ul class='tight'>{''.join(warn_items)}</ul>" if warn_items else "<p class='sub'>无。</p>"))
    parts.append("<h2 id='manual'>须人工复核（MANUAL）</h2><p class='sub'>QC 只能给证据（缩略图、曲线、统计），最终要人看画面/试听。逐路的 MANUAL 项见规则明细。</p>"
                 + (f"<ul class='tight'>{manual_items}</ul>" if manual_items else "<p class='sub'>无。</p>"))
    parts.append("<h2 id='charts'>曲线</h2>" + _charts(rep, art))
    parts.append("<h2 id='thumbs'>缩略图（供复核 UI/水印/指针/穿模/闪屏）</h2>" + _thumbs(rep, art))
    parts.append("<h2 id='rules'>规则明细</h2>" + _rules_detail(rep))
    src = "".join(f"<li><code>{_e(a)}</code> {_e(b)}</li>" for a, b in SOURCES)
    parts.append("<h2 id='sources'>需求来源与判定口径</h2><ul class='tight'>" + src + "</ul>"
                 "<h3>状态口径</h3><ul class='tight'>"
                 "<li><b>达标 PASS</b>：自动核验通过。</li><li><b>不达标 FAIL</b>：硬性条款不通过，阻断。</li>"
                 "<li><b>待确认 WARN</b>：已量化但需方口径未定，或内部件已知差距；不阻断但必须处置。</li>"
                 "<li><b>需人工 MANUAL</b>：只能给证据，须人看画面/试听。</li><li><b>不适用 SKIP</b>：本路/本组不适用。</li>"
                 "<li><b>未能核验 UNVERIFIED</b>：该跑没跑（工具链缺失等），不得视为通过，阻断。</li></ul>"
                 "<h3>方法要点</h3><ul class='tight'>"
                 "<li>运动参数由位姿有限差分反算（不信任 camera_speed），四元数夹角用 atan2 形式避免量化噪声放大。</li>"
                 "<li>几何代理：相机本体系 = 四元数旋转 (1,0,0)/(0,1,0)/(0,0,1)（docs/69 y-up 左手系）；主体 = 程序相机注视点（相机−Follow Offset，即舞台中心）与受控人物 player_position；无遮挡模型。</li>"
                 "<li>像素面：320×180 灰度，|Δ|&gt;12 计运动、&gt;20 计跨遍变化；仅在完整内外参、画幅和交付逻辑帧对应均核实后跨遍比较，否则三项均未核验。顺序帧对应不替代逐帧 PTS 或世界状态同步证据。</li>"
                 "<li>输入一致性：按键帧中相机本体系速度符号与键位方向一致率（滞后 0–6 帧搜索）；鼠标 dx/dy 与偏航/俯仰速率的 Pearson 相关（滞后 0–10 帧搜索）。</li></ul>")
    parts.append(f"<footer>worldline_qc · 规则依据见上；源码 engine-data/qc/worldline_qc。视频探测 {len(vids)} 路。</footer></body></html>")
    return "".join(parts)
