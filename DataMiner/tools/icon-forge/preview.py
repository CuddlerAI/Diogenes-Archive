#!/usr/bin/env python3
"""Render out/sprite.svg to out/preview.html for eyeballing.

Icons are judged at the size they ship at, not at the size they were drawn. The
nav uses 17 px and the buttons 15 px, so those columns are the ones that decide
whether a glyph works; 48 px is only there to show what the tracer actually did.
Also renders each icon in both brand roles, because a shape that reads at 17 px
in green can still mud up in cyan.
"""

from __future__ import annotations

import re
from pathlib import Path

HERE = Path(__file__).parent
SPRITE = HERE / "out" / "sprite.svg"
OUT = HERE / "out" / "preview.html"

SIZES = (15, 17, 24, 48)

CSS = """
:root{--bg:#08090B;--surface:#0B0D11;--line:#1C2028;--text:#E9EBEF;--muted:#8B93A1;
  --quiet:#5C6472;--green:#A3E635;--cyan:#56C7F5;
  --mono:"IBM Plex Mono",ui-monospace,Menlo,Consolas,monospace;
  --sans:Inter,-apple-system,system-ui,sans-serif}
*{box-sizing:border-box}
body{margin:0;padding:32px;background:var(--bg);color:var(--text);font-family:var(--sans)}
h1{font-size:17px;font-weight:650;letter-spacing:-.02em;margin:0 0 4px}
.sub{color:var(--muted);font:600 10px/1.5 var(--mono);letter-spacing:.1em;text-transform:uppercase;margin:0 0 26px}
.sub::before{content:"// ";color:var(--green)}
table{border-collapse:collapse;width:100%;max-width:820px}
th{text-align:left;color:var(--quiet);font:600 9px/1 var(--mono);letter-spacing:.1em;
  text-transform:uppercase;padding:0 0 10px;border-bottom:1px solid var(--line)}
td{padding:14px 0;border-bottom:1px solid var(--line);vertical-align:middle}
td.name{font:600 11px/1.4 var(--mono);color:var(--text)}
td.name span{display:block;color:var(--quiet);font-size:9px;font-weight:500;margin-top:3px}
.row{display:flex;align-items:center;gap:20px}
.cell{display:flex;flex-direction:column;align-items:center;gap:6px}
.cell b{color:var(--quiet);font:500 8px/1 var(--mono)}
svg.ic{fill:currentColor;stroke:none;display:block}
.g{color:var(--green)}.c{color:var(--cyan)}.m{color:var(--muted)}
.chip{display:inline-flex;align-items:center;gap:7px;padding:0 12px;height:32px;border-radius:5px;
  background:var(--green);color:#0A0D04;font:750 10.5px/1 var(--sans)}
.chip svg{width:15px;height:15px;fill:currentColor}
.nav{display:grid;gap:3px;width:168px;background:#070809;padding:8px;border-radius:6px}
.nav a{display:grid;grid-template-columns:18px 1fr;align-items:center;gap:10px;height:40px;padding:0 10px;
  border-left:2px solid transparent;color:var(--muted);font:600 11.5px/1 var(--sans);text-decoration:none}
.nav a.on{border-color:var(--green);color:var(--text);background:rgba(163,230,53,.10)}
.nav a.on svg{color:var(--green)}
.nav svg{width:17px;height:17px;fill:currentColor}
.insitu{display:flex;gap:28px;align-items:flex-start;margin-top:30px;padding-top:24px;border-top:1px solid var(--line)}
"""


def main() -> int:
    if not SPRITE.exists():
        print(f"no sprite at {SPRITE} — run forge.py first")
        return 1

    sprite = SPRITE.read_text(encoding="utf-8")
    ids = re.findall(r'<symbol id="(i-[\w-]+)"', sprite)
    roles = dict(zip(ids, re.findall(r"<!-- (.+?) -->\n\s*<symbol", sprite)))
    if not ids:
        print("sprite contains no symbols")
        return 1

    def use(i: str, size: int, cls: str = "") -> str:
        return (
            f'<svg class="ic {cls}" width="{size}" height="{size}" viewBox="0 0 24 24">'
            f'<use href="#{i}"/></svg>'
        )

    rows = "\n".join(
        f'<tr><td class="name">{i}<span>{roles.get(i, "")}</span></td><td><div class="row">'
        + "".join(f'<div class="cell">{use(i, s)}<b>{s}</b></div>' for s in SIZES)
        + f'<div class="cell">{use(i, 24, "g")}<b>green</b></div>'
        + f'<div class="cell">{use(i, 24, "c")}<b>cyan</b></div>'
        + "</div></td></tr>"
        for i in ids
    )

    nav_ids = [i for i in ("i-capture", "i-account", "i-history") if i in ids]
    nav = "".join(
        f'<a class="{"on" if n == 0 else ""}">{use(i, 17)}'
        f'<span>{i.removeprefix("i-")}</span></a>'
        for n, i in enumerate(nav_ids)
    )
    chip = (
        f'<div class="chip">{use("i-record", 15)}开始录制</div>' if "i-record" in ids else ""
    )

    OUT.write_text(
        "<!doctype html><meta charset=utf-8><title>icon-forge preview</title>"
        f"<style>{CSS}</style>"
        f'<div style="display:none">{sprite}</div>'
        "<h1>DataMiner icon set</h1>"
        "<p class=sub>seedream → potrace → sprite.svg</p>"
        "<table><tr><th>symbol</th><th>rendered</th></tr>"
        f"{rows}</table>"
        f'<div class="insitu"><div><div class="nav">{nav}</div></div><div>{chip}</div></div>',
        encoding="utf-8",
    )
    print(f"wrote {OUT} — {len(ids)} symbol(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
