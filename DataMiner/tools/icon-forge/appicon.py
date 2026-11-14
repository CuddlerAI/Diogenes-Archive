#!/usr/bin/env python3
"""Produce DataMiner application-icon candidates for review.

Different problem from the nav glyphs, and deliberately a different script. An
app icon is *born* raster — Tauri wants an .ico plus a spread of PNGs — so there
is nothing to vectorise, and the judgement is "does this read in a 16px taskbar
and on a 512px store page", not "does this match a stroke weight".

Two sources, because they answer different questions:

  brand/   the actual Diogenes lamp, drawn from the geometry in website/favicon.svg
           and painted with Pillow. Exact brand fidelity, crisp at every size,
           no model in the loop. This is the safe answer.
  gen/     Seedream takes on the same brief. Free to be more interesting; also
           free to invent a lamp that is not our lamp.

Nothing here writes into src-tauri — pick a candidate first, then wire it.

Usage:
    VOLCES_API_KEY=... python3 appicon.py
    python3 appicon.py --brand-only        # no API calls
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from PIL import Image, ImageDraw

from forge import generate

HERE = Path(__file__).parent
OUT = HERE / "out" / "appicon"

# The sizes that decide whether an icon works. 16 is the Windows taskbar and the
# real test; 256 is what the installer and Alt-Tab show; 512 is for macOS .icns
# if this ever ships there.
SIZES = (16, 32, 48, 64, 128, 256, 512)
# 512 is generated but not shown inline — at that width the contact sheet stops
# being comparable and starts being a scroll.
REVIEW_SIZES = (16, 32, 48, 64, 128, 256)

INK = "#0D1310"      # site --ink: lamp body on a light ground
LIGHT = "#E9EBEF"    # site dark --ink: lamp body on a dark ground
GREEN = "#A3E635"    # the flame — brand green, never substituted
PAPER = "#F7F7F3"    # site --paper, for the light-ground variant
DARK = "#0B0D11"     # site dark --paper-2, for the dark-ground variant

# Seedream briefs. Kept few and distinct: three tries at the same idea beat ten
# variations on framing. All three must survive being shrunk to 16px, so the
# prompts fight detail as hard as the glyph prompts do.
GEN_PROMPTS = {
    "gen-lamp-badge": (
        "一枚现代软件应用图标：圆角方形底板，深近黑色的底色，正中央是一盏极简几何化的"
        "古典手提油灯——梯形灯罩、竖直提环、方正的灯身，灯身内部有一道明亮的黄绿色"
        "斜线作为火焰。整体扁平矢量风格，形状简洁厚重，构图居中对称，"
        "只有底板和油灯两个层次。不要文字，不要字母，不要渐变，不要写实材质，"
        "不要复杂细节，不要多个图标"
    ),
    "gen-lamp-flat": (
        "一个极简扁平的油灯标志：纯黑的几何化油灯剪影置于浅灰白色背景上，"
        "灯罩为梯形，灯身为圆角方形，灯身中央有一道粗壮的黄绿色斜线代表火焰。"
        "图形厚重简洁，笔画粗，居中构图，四周留白。不要文字，不要渐变，不要阴影，"
        "不要立体，不要边框，不要多个图标"
    ),
    "gen-lamp-beam": (
        "一枚应用图标：深色圆角方形底板，中央一盏简化的几何油灯，灯身透出一道"
        "黄绿色的斜向光束，光束笔直有力，斜向切过底板。扁平矢量风格，色块干净，"
        "只有深底色与黄绿色两种颜色，形状粗壮。不要文字，不要字母，不要渐变，"
        "不要写实光效，不要复杂细节"
    ),
}


def draw_lamp(size: int, ground: str | None, body: str = INK) -> Image.Image:
    """Render the brand lamp at `size`, from website/favicon.svg's geometry.

    `body` has to track `ground`. logo.svg's own comment notes that on a dark
    tab strip the ink recedes and the green flame carries the mark alone — which
    is fine for a favicon beside a title, and useless for a taskbar icon that has
    nothing else to identify it. So a dark badge gets a light body.

    favicon.svg is the right source rather than logo.svg: it is built from solid
    shapes instead of 2.4px strokes, precisely because strokes collapse at small
    sizes — the same reason an app icon needs to survive 16px. Coordinates below
    are that file's 32-unit viewBox, scaled.

    Drawn at 8x and downsampled, which is the cheapest way to get antialiased
    edges out of Pillow's non-antialiased polygon fill.
    """
    ss = 8
    n = size * ss
    u = n / 32.0  # one favicon unit in supersampled pixels

    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    if ground:
        # Squircle-ish rounded square, ~22% corner radius — the platform
        # convention for Windows/macOS app icons.
        d.rounded_rectangle([0, 0, n - 1, n - 1], radius=int(n * 0.22), fill=ground)

    # Inset the lamp so it does not crowd the badge edge. On a transparent
    # ground there is no badge, so it can use the full box.
    inset = 0.14 if ground else 0.02
    def x(v: float) -> float:
        return n * inset + v * u * (1 - 2 * inset)
    y = x

    # <rect x=14.4 y=2.5 w=3.2 h=4.5 rx=1.4/> — the hanger
    d.rounded_rectangle([x(14.4), y(2.5), x(17.6), y(7.0)], radius=1.4 * u * (1 - 2 * inset), fill=body)
    # <path d="M5 13.4 10.8 6h10.4L27 13.4H5Z"/> — the hood
    d.polygon([(x(5), y(13.4)), (x(10.8), y(6)), (x(21.2), y(6)), (x(27), y(13.4))], fill=body)
    # <path d="M7 14.6h18v9.6A5.8 5.8 0 0 1 19.2 30h-6.4A5.8 5.8 0 0 1 7 24.2v-9.6Z"/>
    d.rounded_rectangle([x(7), y(14.6), x(25), y(30)], radius=5.8 * u * (1 - 2 * inset), fill=body)
    d.rectangle([x(7), y(14.6), x(25), y(22)], fill=body)  # square off the top
    # <path d="M20 18.2 13 26" stroke=GREEN stroke-width=5.4 stroke-linecap=round/>
    d.line([(x(20), y(18.2)), (x(13), y(26))], fill=GREEN, width=int(5.4 * u * (1 - 2 * inset)))
    r = 2.7 * u * (1 - 2 * inset)  # round the cap manually; PIL has no linecap
    for px, py in ((x(20), y(18.2)), (x(13), y(26))):
        d.ellipse([px - r, py - r, px + r, py + r], fill=GREEN)

    return img.resize((size, size), Image.LANCZOS)


def review_page(variants: list[str]) -> str:
    """Contact sheet on both grounds, at the sizes that actually decide it."""
    rows = []
    for v in variants:
        cells = "".join(
            f'<div class=c><img src="{v}/{s}.png" width={s} height={s} alt=""><b>{s}</b></div>'
            for s in REVIEW_SIZES
        )
        rows.append(f"<tr><td class=n>{v}</td><td><div class=r>{cells}</div></td></tr>")
    return (
        "<!doctype html><meta charset=utf-8><title>DataMiner app icon candidates</title>"
        "<style>"
        ":root{--bg:#08090B;--fg:#E9EBEF;--q:#5C6472;--line:#1C2028;--green:#A3E635;"
        "--mono:'IBM Plex Mono',ui-monospace,Menlo,monospace}"
        "body{margin:0;padding:30px;background:var(--bg);color:var(--fg);"
        "font-family:Inter,-apple-system,system-ui,sans-serif}"
        "h1{font-size:17px;font-weight:650;margin:0 0 4px}"
        ".sub{color:var(--q);font:600 10px/1.5 var(--mono);letter-spacing:.1em;"
        "text-transform:uppercase;margin:0 0 26px}.sub::before{content:'// ';color:var(--green)}"
        "table{border-collapse:collapse}td{padding:16px 0;border-bottom:1px solid var(--line);"
        "vertical-align:middle}td.n{font:600 11px/1.4 var(--mono);padding-right:26px;white-space:nowrap}"
        ".r{display:flex;align-items:flex-end;gap:22px}.c{display:flex;flex-direction:column;"
        "align-items:center;gap:7px}.c b{color:var(--q);font:500 8px/1 var(--mono)}"
        "img{image-rendering:auto;display:block}"
        ".pane{display:flex;gap:30px;flex-wrap:wrap}"
        ".light{background:#F7F7F3;padding:20px;border-radius:10px}"
        ".dark{background:#0B0D11;padding:20px;border-radius:10px;border:1px solid var(--line)}"
        "</style>"
        "<h1>DataMiner 应用图标候选</h1><p class=sub>16px is the taskbar — judge there first</p>"
        f"<table>{''.join(rows)}</table>"
    )


# Tauri's own `tauri icon` command emits these names, so matching them keeps the
# bundle config recognisable to anyone who has used it. .icns is omitted: this
# client is Windows-only (see the msvc target in Cargo/CI), and Tauri only needs
# the entries actually listed in bundle.icon.
TAURI_PNGS = {"32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256}
# Sizes inside the multi-resolution .ico. 16 is the taskbar and title bar, 256 the
# installer and large-icon views; Explorer picks per context.
ICO_SIZES = (16, 32, 48, 64, 128, 256)


def install(variant: str, dest: Path) -> None:
    """Copy a chosen candidate into src-tauri/icons/ under Tauri's filenames."""
    src = OUT / variant
    if not (src / "512.png").exists():
        raise SystemExit(f"no such candidate: {variant} (run without --install first)")

    dest.mkdir(parents=True, exist_ok=True)
    master = Image.open(src / "512.png").convert("RGBA")

    for filename, size in TAURI_PNGS.items():
        Image.open(src / f"{size}.png").convert("RGBA").save(dest / filename)
        print(f"  {filename}  ({size}px)")

    # Pillow builds the multi-resolution ICO itself; it stores the larger entries
    # PNG-compressed, which is what Windows expects above 48px.
    master.save(dest / "icon.ico", format="ICO", sizes=[(s, s) for s in ICO_SIZES])
    print(f"  icon.ico  ({', '.join(str(s) for s in ICO_SIZES)})")

    print(
        "\nAdd to tauri.conf.json bundle:\n"
        '  "icon": ["icons/32x32.png", "icons/128x128.png",\n'
        '           "icons/128x128@2x.png", "icons/icon.ico"]'
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--brand-only", action="store_true", help="skip Seedream, draw the lamp only")
    ap.add_argument(
        "--install",
        metavar="VARIANT",
        help="copy a candidate into src-tauri/icons/ (e.g. brand-light); generates nothing",
    )
    args = ap.parse_args()

    if args.install:
        target = HERE / ".." / ".." / "apps" / "client" / "src-tauri" / "icons"
        print(f"installing {args.install} → {target.resolve()}")
        install(args.install, target.resolve())
        return 0

    OUT.mkdir(parents=True, exist_ok=True)
    variants: list[str] = []

    # The authored lamp, on three grounds.
    # (name, badge ground, lamp body). "bare" is transparent for platforms that
    # mask their own shape, and takes the light body so it survives dark taskbars.
    for name, ground, body in (
        ("brand-dark", DARK, LIGHT),
        ("brand-light", PAPER, INK),
        ("brand-bare", None, LIGHT),
    ):
        (OUT / name).mkdir(exist_ok=True)
        for s in SIZES:
            draw_lamp(s, ground, body).save(OUT / name / f"{s}.png")
        variants.append(name)
        print(f"[{name}] drawn at {', '.join(map(str, SIZES))}")

    if not args.brand_only:
        key = __import__("os").environ.get("VOLCES_API_KEY", "")
        if not key:
            print("VOLCES_API_KEY unset; brand variants only.", file=sys.stderr)
        else:
            for name, prompt in GEN_PROMPTS.items():
                try:
                    png = generate(prompt, key)
                except Exception as err:  # noqa: BLE001
                    print(f"[{name}] GENERATE FAILED: {err}", file=sys.stderr)
                    continue
                (OUT / name).mkdir(exist_ok=True)
                (OUT / name / "source.png").write_bytes(png)
                src = Image.open(OUT / name / "source.png").convert("RGBA")
                for s in SIZES:
                    src.resize((s, s), Image.LANCZOS).save(OUT / name / f"{s}.png")
                variants.append(name)
                print(f"[{name}] generated, downsampled to {', '.join(map(str, SIZES))}")

    # List every candidate on disk, not just this run's. Re-drawing the brand
    # variants with --brand-only should not silently drop the generated ones from
    # the comparison — the whole point of the page is judging them side by side.
    on_disk = sorted(p.name for p in OUT.iterdir() if p.is_dir() and (p / "16.png").exists())
    (OUT / "index.html").write_text(review_page(on_disk), encoding="utf-8")
    print(f"\nreview: {OUT / 'index.html'} — {len(on_disk)} candidate(s) on disk")
    return 0


if __name__ == "__main__":
    sys.exit(main())
