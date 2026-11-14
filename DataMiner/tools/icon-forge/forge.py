#!/usr/bin/env python3
"""Generate the DataMiner icon set with Seedream, then vectorise it to an SVG sprite.

Seedream returns raster art. The client draws its icons at 15–20 px and recolours
them through `currentColor` on hover/active, which a PNG cannot do, so every
generated image is traced back to a filled vector path before it reaches the UI.

Pipeline, per icon:

    Seedream (2K raster)  →  greyscale  →  autocrop  →  threshold  →  potrace
      →  path in a 24×24 viewBox  →  <symbol> in sprite.svg

The traced paths are *filled outlines*, not strokes — the same trick icon fonts
use. That is what keeps `fill: currentColor` working, but it also means the
source art has to be clean: any grey shading or texture Seedream sneaks in
becomes a ragged edge here, which is why icons.json argues so hard against it.

Usage:
    VOLCES_API_KEY=... python3 forge.py            # generate + trace everything
    VOLCES_API_KEY=... python3 forge.py --only capture,history
    python3 forge.py --trace-only                  # re-trace cached PNGs, no API

Raw PNGs are kept in out/raw/ so tracing can be re-tuned without paying for
generation again. Review them when a glyph comes out wrong — the fault is
usually in the prompt, not the tracer.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

import numpy as np
import potrace
from PIL import Image, ImageFilter, ImageOps

HERE = Path(__file__).parent
OUT = HERE / "out"
RAW = OUT / "raw"

ENDPOINT = "https://ark.cn-beijing.volces.com/api/v3/images/generations"
MODEL = os.environ.get("SEEDREAM_MODEL", "doubao-seedream-5-0-260128")

# Trace tuning. `turdsize` drops speckles by area — Seedream likes to leave a
# stray dot in a corner, and at 24 px that reads as dirt. At the 512 px working
# resolution a deliberate small feature (a titlebar dot, say) is ~175 px², while
# generation noise is well under 100, so 100 separates them with room to spare.
# `alphamax` below 1.0 keeps hard corners hard, which matters for the geometric
# look; the default 1.0 rounds them into blobs.
TURDSIZE = 100
ALPHAMAX = 0.9
OPTTOLERANCE = 0.2

VIEWBOX = 24.0
PADDING = 1.6  # keeps strokes off the viewBox edge, matching website/sprite.svg


def load_spec() -> dict:
    return json.loads((HERE / "icons.json").read_text(encoding="utf-8"))


def generate(prompt: str, api_key: str) -> bytes:
    """Call Seedream and return PNG bytes.

    Ark accepts either a named size ("2K") or explicit WxH. We ask for a square
    explicitly because a 16:9 icon wastes half the traced bitmap, and fall back
    to the named size if this deployment rejects the explicit form.
    """
    for size in ("2048x2048", "2K"):
        body = json.dumps(
            {
                "model": MODEL,
                "prompt": prompt,
                "sequential_image_generation": "disabled",
                "response_format": "b64_json",
                "size": size,
                "stream": False,
                "watermark": False,
            }
        ).encode()
        req = urllib.request.Request(
            ENDPOINT,
            data=body,
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {api_key}"},
        )
        try:
            with urllib.request.urlopen(req, timeout=180) as res:
                payload = json.loads(res.read())
        except urllib.error.HTTPError as err:
            detail = err.read().decode(errors="replace")[:400]
            if size == "2048x2048" and err.code == 400:
                print(f"    size=2048x2048 rejected, retrying with 2K — {detail}", file=sys.stderr)
                continue
            raise RuntimeError(f"HTTP {err.code}: {detail}") from err

        item = payload["data"][0]
        if item.get("b64_json"):
            return base64.b64decode(item["b64_json"])
        with urllib.request.urlopen(item["url"], timeout=120) as res:  # noqa: S310
            return res.read()

    raise RuntimeError("no size accepted")


def to_bitmap(png: bytes) -> np.ndarray:
    """PNG bytes → boolean array in the polarity potracer wants.

    Careful: potracer traces the *zero* region, the opposite of what the name
    `Bitmap` and the pypotrace convention suggest. Hand it True-at-ink and it
    traces the background instead, emitting a canvas-sized rectangle with your
    glyph punched out of it as a hole. So ink must be False here.

    Seedream renders on near-white rather than pure white, so the threshold sits
    well above the midpoint; anything darker than 0.72 luma counts as ink. The
    autocrop runs *before* thresholding so a soft vignette does not survive as a
    full-bleed rectangle.
    """
    img = Image.open(__import__("io").BytesIO(png)).convert("L")

    # Flatten near-white to white so autocrop finds the true glyph bounds.
    arr = np.asarray(img, dtype=np.uint8)
    arr = np.where(arr > 235, 255, arr).astype(np.uint8)
    img = Image.fromarray(arr)

    bbox = ImageOps.invert(img).getbbox()
    if bbox:
        img = img.crop(bbox)

    # Work at a fixed resolution: potrace cost scales with edge length, and 512
    # is well past the point where extra pixels change the output curve.
    img.thumbnail((512, 512), Image.LANCZOS)

    img, passes = thicken(img)

    ink = np.asarray(img, dtype=np.uint8) < 184

    # Pad with background. Any ink touching the bitmap edge makes potrace trace
    # the edge itself as part of the shape, so a margin costs nothing and removes
    # the failure mode regardless of how tightly Seedream cropped its output.
    return np.pad(~ink, 8, constant_values=True), passes


def thicken(img: Image.Image) -> tuple[Image.Image, int]:
    """Dilate ink until its stroke ratio reaches TARGET_STROKE_RATIO.

    MinFilter spreads dark pixels, which is a dilation of the ink since ink is
    dark — one pixel per side per pass. Dilate-only, never erode: an over-weight
    glyph is usually one built from solid shapes (a filled record dot), where the
    run-length metric is not really measuring a stroke and eroding would just eat
    the shape. Better a slightly heavy outlier than a destroyed one.
    """
    for n in range(MAX_THICKEN_PASSES + 1):
        ink = np.asarray(img, dtype=np.uint8) < 184
        if stroke_ratio(ink) >= TARGET_STROKE_RATIO:
            return img, n
        img = img.filter(ImageFilter.MinFilter(3))
    return img, MAX_THICKEN_PASSES


# The nav renders at 17px and buttons at 15px. A stroke thinner than ~0.9px there
# greys out into nothing, so that is the gate. Expressed against the 24-unit
# viewBox it means ~1.3 units, and the hand-drawn symbols in website/sprite.svg
# sit at 1.6, so this is a floor rather than a target.
MIN_STROKE_PX_AT_17 = 0.9

# Stroke weight is normalised here rather than asked for in the prompt, because
# the ratio that matters is stroke-to-glyph-extent and that is a property of the
# *composition*, not the linework. A viewfinder whose brackets sit in the corners
# spreads over the whole box, so even chunky strokes come out at ~5% of its
# extent, while a compact concentric-circle glyph lands near 12% — the same
# drawing hand, a 2x difference in apparent weight once both are fitted to 24
# units. Left alone the set looks like it came from two different icon families.
#
# 8.5% reproduces the existing hand-drawn nav icons: stroke-width 1.6 in a 20-unit
# viewBox renders 1.36px at 17px, which needs ~1.9 units out of the 20.8-unit
# content box here.
TARGET_STROKE_RATIO = 0.085

# Each pass adds a pixel per side at the 512px working resolution. Going from the
# ~5% Seedream typically returns to 8.5% costs about 5, so 10 is generous; the cap
# exists because dilation also closes counters, and a glyph that needs more than
# this is badly composed rather than merely thin.
MAX_THICKEN_PASSES = 10


def stroke_ratio(ink: np.ndarray) -> float:
    """Thinnest ink run as a fraction of glyph width — the legibility predictor.

    Measured on the raster rather than the traced path because a run length is
    unambiguous, whereas recovering "stroke weight" from filled outlines means
    guessing which pairs of edges face each other.
    """
    rows = np.where(ink.any(axis=1))[0]
    cols = np.where(ink.any(axis=0))[0]
    if not len(rows) or not len(cols):
        return 0.0
    width = cols[-1] - cols[0] + 1

    runs: list[int] = []
    for axis in (1, 0):  # horizontal runs, then vertical, so bars in either
        arr = ink if axis == 1 else ink.T          # orientation get measured
        for line in arr[np.linspace(0, arr.shape[0] - 1, 40).astype(int)]:
            run = 0
            for on in line:
                if on:
                    run += 1
                elif run:
                    runs.append(run)
                    run = 0
            if run:
                runs.append(run)

    if not runs:
        return 0.0
    # The 10th percentile, not the minimum: a single antialiased tip or a stray
    # 1px nick would otherwise decide the verdict for the whole glyph.
    return float(np.percentile(runs, 10)) / width


def trace(bitmap: np.ndarray) -> tuple[str, tuple[float, float, float, float]]:
    """Trace a bitmap, returning an SVG path and its bounding box."""
    path = potrace.Bitmap(bitmap).trace(
        turdsize=TURDSIZE, alphamax=ALPHAMAX, opticurve=True, opttolerance=OPTTOLERANCE
    )

    parts: list[str] = []
    xs: list[float] = []
    ys: list[float] = []

    for curve in path:
        start = curve.start_point
        parts.append(f"M{start.x:.2f} {start.y:.2f}")
        xs.append(start.x)
        ys.append(start.y)
        for seg in curve:
            end = seg.end_point
            if seg.is_corner:
                # CornerSegment carries one control point: the corner itself.
                parts.append(f"L{seg.c.x:.2f} {seg.c.y:.2f}L{end.x:.2f} {end.y:.2f}")
                xs += [seg.c.x, end.x]
                ys += [seg.c.y, end.y]
            else:
                c1, c2 = seg.c1, seg.c2
                parts.append(
                    f"C{c1.x:.2f} {c1.y:.2f} {c2.x:.2f} {c2.y:.2f} {end.x:.2f} {end.y:.2f}"
                )
                xs += [c1.x, c2.x, end.x]
                ys += [c1.y, c2.y, end.y]
        parts.append("Z")

    if not xs:
        raise RuntimeError("traced to an empty path — check the raw PNG")
    return "".join(parts), (min(xs), min(ys), max(xs), max(ys))


def fit(d: str, bbox: tuple[float, float, float, float]) -> str:
    """Wrap a raw path in a transform that centres it in a 24×24 viewBox."""
    x0, y0, x1, y1 = bbox
    w, h = max(x1 - x0, 1e-6), max(y1 - y0, 1e-6)
    scale = (VIEWBOX - 2 * PADDING) / max(w, h)
    tx = (VIEWBOX - w * scale) / 2 - x0 * scale
    ty = (VIEWBOX - h * scale) / 2 - y0 * scale
    return f'<path transform="translate({tx:.3f} {ty:.3f}) scale({scale:.5f})" d="{d}"/>'


def pascal(name: str) -> str:
    return "".join(part.capitalize() for part in name.split("-"))


def emit_tsx(icons: list, paths: dict) -> str:
    """Emit the symbols as a React module.

    A module rather than an <svg><use href="/sprite.svg#..."> reference because
    this runs inside Tauri, where the document is served over a custom protocol
    and an external `use` href is one more thing that can fail to resolve at
    runtime. Inlining the paths cannot fail, and the icons end up in the same
    bundle as everything else.
    """
    roles = {i["name"]: i["role"] for i in icons}
    components = "\n\n".join(
        f"/** {roles.get(name, name)} */\n"
        f"export function {pascal(name)}Icon(props: IconProps) {{\n"
        f"  return (\n"
        f'    <svg viewBox="0 0 24 24" aria-hidden="true" {{...props}}>\n'
        f"      {paths[name]}\n"
        f"    </svg>\n"
        f"  );\n"
        f"}}"
        for name in paths
    )
    return (
        "// GENERATED by tools/icon-forge/forge.py — do not hand-edit.\n"
        "//\n"
        "// Seedream raster art traced to filled paths, so every glyph is fill-based\n"
        "// rather than stroked. Style them with `fill: currentColor; stroke: none` —\n"
        "// the stroke-width rules that drive the hand-drawn marks in main.tsx do\n"
        "// nothing here, and a leftover `fill: none` renders them invisible.\n"
        "//\n"
        "// Regenerate:  python3 forge.py --emit-tsx ../../apps/client/src/icons.generated.tsx\n"
        "\n"
        "import type { SVGProps } from \"react\";\n"
        "\n"
        "type IconProps = SVGProps<SVGSVGElement>;\n"
        "\n" + components + "\n"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="comma-separated icon names")
    ap.add_argument("--trace-only", action="store_true", help="re-trace cached PNGs, no API calls")
    ap.add_argument(
        "--emit-tsx",
        metavar="PATH",
        help="also write a React module of the symbols (needs the full set, so not with --only)",
    )
    args = ap.parse_args()
    if args.emit_tsx and args.only:
        print("--emit-tsx writes the whole module, so it cannot be combined with --only.", file=sys.stderr)
        return 2

    spec = load_spec()
    style = spec["style"]
    icons = spec["icons"]
    if args.only:
        wanted = {n.strip() for n in args.only.split(",")}
        icons = [i for i in icons if i["name"] in wanted]
        missing = wanted - {i["name"] for i in icons}
        if missing:
            print(f"unknown icon(s): {', '.join(sorted(missing))}", file=sys.stderr)
            return 2

    api_key = os.environ.get("VOLCES_API_KEY", "")
    if not args.trace_only and not api_key:
        print("VOLCES_API_KEY is not set. Export it, or use --trace-only.", file=sys.stderr)
        return 2

    RAW.mkdir(parents=True, exist_ok=True)
    symbols: list[str] = []
    paths: dict[str, str] = {}
    failed: list[str] = []
    thin: list[str] = []

    for icon in icons:
        name = icon["name"]
        png_path = RAW / f"{name}.png"
        print(f"[{name}] {icon['role']}")

        if not args.trace_only:
            prompt = f"{style['prefix']}{icon['prompt']}{style['suffix']}"
            try:
                png_path.write_bytes(generate(prompt, api_key))
                print(f"    generated → {png_path.relative_to(HERE)}")
            except Exception as err:  # noqa: BLE001 — one bad icon must not sink the run
                print(f"    GENERATE FAILED: {err}", file=sys.stderr)
                failed.append(name)
                continue
            time.sleep(1)  # be polite to the endpoint

        if not png_path.exists():
            print("    no cached PNG; skipping", file=sys.stderr)
            failed.append(name)
            continue

        bitmap, passes = to_bitmap(png_path.read_bytes())
        try:
            d, bbox = trace(bitmap)
        except Exception as err:  # noqa: BLE001
            print(f"    TRACE FAILED: {err}", file=sys.stderr)
            failed.append(name)
            continue

        # Legibility gate. Not fatal — the sprite is still written so it can be
        # inspected — but a thin glyph needs the prompt pushed, not the tracer.
        ratio = stroke_ratio(~bitmap)
        at17 = ratio * (VIEWBOX - 2 * PADDING) * 17 / VIEWBOX
        verdict = "ok" if at17 >= MIN_STROKE_PX_AT_17 else "TOO THIN"
        grew = f", +{passes} dilation pass{'es' if passes != 1 else ''}" if passes else ""
        print(f"    stroke {ratio * 100:.1f}% of glyph → {at17:.2f}px at 17px [{verdict}]{grew}")
        if at17 < MIN_STROKE_PX_AT_17:
            thin.append(name)

        fitted = fit(d, bbox)
        symbols.append(
            f'  <!-- {icon["role"]} -->\n'
            f'  <symbol id="i-{name}" viewBox="0 0 24 24" fill="currentColor">'
            f"{fitted}</symbol>"
        )
        paths[name] = fitted
        print(f"    traced ({len(d)} chars)")

    if symbols:
        sprite = (
            '<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" style="position:absolute">\n'
            "  <!-- GENERATED by tools/icon-forge/forge.py — do not hand-edit.\n"
            "       Seedream raster art, traced to filled paths. Every symbol is fill-based\n"
            "       (not stroked), so consumers must set `fill: currentColor; stroke: none`. -->\n"
            + "\n".join(symbols)
            + "\n</svg>\n"
        )
        (OUT / "sprite.svg").write_text(sprite, encoding="utf-8")
        print(f"\nwrote {OUT / 'sprite.svg'} — {len(symbols)} symbol(s)")

    if args.emit_tsx and paths:
        dest = Path(args.emit_tsx)
        dest.write_text(emit_tsx(icons, paths), encoding="utf-8")
        print(f"wrote {dest} — {len(paths)} component(s)")

    if thin:
        print(
            f"\ntoo thin to ship at 17px: {', '.join(thin)}\n"
            "  Push the stroke-weight clause in icons.json and re-generate those names;\n"
            "  re-tracing alone cannot add weight that is not in the raster.",
            file=sys.stderr,
        )
    if failed:
        print(f"failed: {', '.join(failed)}", file=sys.stderr)
        return 1
    return 1 if thin else 0


if __name__ == "__main__":
    sys.exit(main())
