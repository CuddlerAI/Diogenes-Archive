#!/usr/bin/env python3
"""把各脚本的 Markdown 表格合成一页自包含 HTML，供截图发给业务方。

客户要的是「跑脚本 + 发报告截图」。四张表分别来自不同脚本，分开截四次既难读也容易漏，
所以合成一页：客户 oracle 在最前（那是他们的口径），我方内容闸在后（那是打回的真实原因）。
"""
from __future__ import annotations

import argparse
import html
import pathlib

SECTIONS = [
    ("customer_report.md", "① 客户验收脚本 · quality_check_depth_dataset.py",
     "客户 2026-08-11 提供、按字节原样入库的验收 oracle。<b>只查交付包结构</b>"
     "（命名 / 分辨率 / 帧数 / 帧号 / 字段顺序 / 时间戳 / depth 张数与命名对账）；"
     "不查 FOV、HUD、闭环、运镜与动态物体。"),
    ("sopv2_report.md", "② SOP v2 内容自检 · sopv2_video_check.py",
     "业务方 2026-08-11 反馈四条 + 客户「不要」项的机检实现，直接吃 R2 原始件。"
     "FOV 优先<b>读自 camera-pose-index</b>（docs/130 起随 session 上云）；该索引不在时"
     "回落到深度+光流的刚性一致性标定。HUD 判据是<b>常驻绿掩膜</b>而非逐帧绿像素数——"
     "后者分不开 HUD 与绿色布景。"),
    ("loop_report.md", "③ Loop 闭环专项 · loop_closure_check.py",
     "Loop-有位移 的标的物：A→B→A 首尾帧 location/rotation 须非常相近。"
     "阈值沿用 trajgen 生成侧机检闸 0.60 m / 8.0°。"),
    ("handheld_report.md", "④ 手持感自检 · handheld_feel_check.py",
     "量化业务方「摄影师手太稳」与客户「>10% 原地静止即打回」两条。"),
]

CSS = """
:root{--bg:#fff;--fg:#16181d;--mut:#5b6270;--line:#e2e5ea;--pass:#0a7f43;--passbg:#e9f7ef;
--fail:#b3261e;--failbg:#fdecea;--warn:#8a6100;--warnbg:#fff6e2;--head:#f4f6f8;}
*{box-sizing:border-box}
body{margin:0;padding:30px 34px 36px;font:14px/1.55 -apple-system,"PingFang SC",
"Helvetica Neue",Arial,sans-serif;color:var(--fg);background:var(--bg)}
h1{font-size:22px;margin:0 0 6px}
.meta{color:var(--mut);font-size:12.5px;margin:0 0 14px}
.meta code{background:#f2f4f7;padding:1px 5px;border-radius:4px;font-size:12px}
.banner{margin:0 0 24px;padding:11px 15px;border-radius:8px;border:1px solid;font-size:13.5px;line-height:1.6}
.banner.fail{background:var(--failbg);border-color:#f0b4af;color:#7d1a15}
.banner.pass{background:var(--passbg);border-color:#a8dcc0;color:#0a5c33}
section{margin:0 0 26px}
h2{font-size:15px;margin:0 0 3px;padding-bottom:6px;border-bottom:2px solid var(--line)}
.sub{color:var(--mut);font-size:12px;margin:0 0 9px;line-height:1.55}
table{border-collapse:collapse;width:100%;font-size:12.5px;table-layout:fixed}
th,td{border:1px solid var(--line);padding:6px 8px;text-align:left;vertical-align:top;
word-break:break-word}
th{background:var(--head);font-weight:600;color:#39404d}
th:nth-child(1),td:nth-child(1){width:18%}
th:nth-child(2),td:nth-child(2){width:25%}
th:nth-child(4),td:nth-child(4){width:8%;text-align:center}
td.status{font-weight:650;text-align:center;white-space:nowrap}
td.status.pass{color:var(--pass);background:var(--passbg)}
td.status.fail{color:var(--fail);background:var(--failbg)}
td.status.warn{color:var(--warn);background:var(--warnbg)}
.concl{margin:9px 0 0;font-weight:650;font-size:13.5px}
.concl.pass{color:var(--pass)}.concl.fail{color:var(--fail)}
footer{margin-top:24px;padding-top:11px;border-top:1px solid var(--line);color:var(--mut);
font-size:11.5px;line-height:1.6}
"""


def md_to_html(md: str, title: str, sub: str) -> str:
    rows = [l for l in md.strip().splitlines() if l.startswith("|")]
    concls = [l for l in md.strip().splitlines() if "结论" in l and not l.startswith("|")]
    out = [f"<section><h2>{html.escape(title)}</h2><p class='sub'>{sub}</p>"]
    if not rows:
        out.append(f"<pre>{html.escape(md[:2000])}</pre></section>")
        return "".join(out)
    hdr = [c.strip() for c in rows[0].strip("|").split("|")]
    out.append("<table><thead><tr>" +
               "".join(f"<th>{html.escape(c)}</th>" for c in hdr) + "</tr></thead><tbody>")
    for r in rows[2:]:
        cells = [c.strip() for c in r.strip("|").split("|")]
        cls = {"通过": "pass", "不通过": "fail",
               "需人工确认": "warn", "不计问题": "warn"}.get(cells[-1], "")
        out.append("<tr>" + "".join(
            f'<td class="{"status " + cls if i == len(cells) - 1 else ""}">{html.escape(c)}</td>'
            for i, c in enumerate(cells)) + "</tr>")
    out.append("</tbody></table>")
    for c in concls:
        cls = "fail" if "不通过" in c else "pass"
        out.append(f'<p class="concl {cls}">{html.escape(c)}</p>')
    out.append("</section>")
    return "".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reports_dir", type=pathlib.Path)
    ap.add_argument("out_html", type=pathlib.Path)
    ap.add_argument("--session", default="")
    ap.add_argument("--lane", default="")
    ap.add_argument("--note", default="")
    args = ap.parse_args()

    parts, fails = [], []
    for fn, title, sub in SECTIONS:
        p = args.reports_dir / fn
        if not p.exists():
            continue
        md = p.read_text()
        parts.append(md_to_html(md, title, sub))
        fails += [l.split("|")[1].strip() for l in md.splitlines()
                  if l.startswith("|") and l.rstrip().endswith("不通过 |")]

    if fails:
        banner = ("<b>结论：不通过。</b>硬性不通过 " + str(len(fails)) + " 项 —— "
                  + "、".join(dict.fromkeys(fails)) +
                  "<br>注意：客户脚本只查<b>结构</b>，结构全绿不等于可交付；"
                  "内容项踩线一样是打回。")
        cls = "fail"
    else:
        banner = "<b>结论：四张表全过。</b>仍需抽帧目检定性（动态物体、穿模、遮挡、过暗）。"
        cls = "pass"

    meta = f"session <code>{html.escape(args.session)}</code>"
    if args.lane:
        meta += f" · lane <code>{html.escape(args.lane)}</code>"
    if args.note:
        meta += f" · {html.escape(args.note)}"

    doc = f"""<!doctype html><html lang="zh"><head><meta charset="utf-8">
<title>Loop-有位移 数据验收报告</title><style>{CSS}</style></head><body>
<h1>Loop-有位移 · 单条 session 验收报告</h1>
<p class="meta">{meta}</p>
<div class="banner {cls}">{banner}</div>
{''.join(parts)}
<footer>
生成方式见 <b>DataMiner/docs/128</b>；复现：<code>DataMiner/tools/quality-check/run_acceptance.sh &lt;session-id&gt;</code>。<br>
① 是客户提供的验收 oracle，按字节原样入库不得修改；②③④ 为我方自检。<br>
位姿相关判定（③）基于 RGB-D 里程计重建 —— 摄取面不上传 <code>camera-pose-index.jsonl</code>，
且 FO4 渲染相机绑定仍在 #636 重新取证中。
</footer></body></html>"""
    args.out_html.write_text(doc)
    print(args.out_html)


if __name__ == "__main__":
    main()
