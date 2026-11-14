# icon-forge

Generates the DataMiner client icon set with Seedream (Volcengine Ark), then
vectorises it into an SVG sprite the client can recolour.

## Why the tracing step exists

Seedream returns raster art. The client draws its icons at 15–20 px and tints
them through `currentColor` on hover and active states — a PNG can do neither.
So every generated image is traced back to a vector path before it ships:

```
Seedream (2K raster) → greyscale → autocrop → threshold → potrace
  → path fitted to a 24×24 viewBox → <symbol> in out/sprite.svg
```

The traced paths are **filled outlines, not strokes** — the same trick icon fonts
use. That is what keeps `currentColor` working, and it means consumers must set
`fill: currentColor; stroke: none`, unlike `website/sprite.svg` whose symbols are
hand-drawn strokes. Do not mix the two conventions in one rule.

It also means the source art has to be clean: any grey shading, texture or
drop-shadow Seedream sneaks in becomes a ragged edge in the SVG. That is what
most of `icons.json` is arguing against.

## Usage

```bash
python3 -m venv .venv && .venv/bin/pip install potracer pillow
export VOLCES_API_KEY=...            # Ark API key, NOT a ByteHouse IAM key
.venv/bin/python forge.py            # generate + trace the whole set
```

| command | effect |
| --- | --- |
| `forge.py` | generate every icon in `icons.json`, then trace |
| `forge.py --only capture,history` | just those two |
| `forge.py --trace-only` | re-trace the cached PNGs, no API calls, no cost |
| `forge.py --emit-tsx <path>` | also write the React module the client imports |
| `preview.py` | render `out/sprite.svg` to `out/preview.html` |
| `appicon.py` | application-icon candidates (separate pipeline, see below) |
| `appicon.py --install brand-light` | copy a chosen candidate into `src-tauri/icons/` |

`SEEDREAM_MODEL` overrides the model (default `doubao-seedream-5-0-260128`).

`--only` rewrites `sprite.svg` with just the named icons, so after iterating on a
few, run a bare `--trace-only` to rebuild the full set before shipping it.

## What the client actually imports

`src/icons.generated.tsx`, not the sprite. A React module rather than
`<svg><use href="/sprite.svg#…">` because this runs inside Tauri, where the
document is served over a custom protocol and an external `use` href is one more
thing that can fail to resolve at runtime:

```bash
python3 forge.py --trace-only --emit-tsx ../../apps/client/src/icons.generated.tsx
```

`out/sprite.svg` is still produced, and `preview.py` reads it — it is the review
artefact and a convenience for any non-Tauri consumer.

The brand lamp is **not** in here. It lives in `src/brand.tsx`, hand-ported from
`website/logo.svg` and `website/favicon.svg`. Do not regenerate a logo you already
own — see the app-icon note below for how that went.

Everything lands in `out/`, which is gitignored: `out/raw/*.png` is the generated
art and `out/sprite.svg` is the deliverable. Raw PNGs are kept so tracing can be
re-tuned with `--trace-only` without paying for generation again — when a glyph
comes out wrong, look at the PNG first, because the fault is usually the prompt
rather than the tracer.

## Reviewing output

Run `preview.py` and open `out/preview.html`. It renders every symbol at 15, 17,
24 and 48 px, in both brand roles, plus a mock nav row and button. Judge the
icons at 15–17 px — that is what ships; the 48 px column only shows what the
tracer actually did.

## Stroke weight is normalised, not requested

The number that decides whether a glyph survives at 17 px is stroke-to-glyph-extent,
and that is a property of the **composition**, not the linework. A viewfinder with
brackets in the corners spreads across the whole box, so even chunky strokes come
out at ~5% of its extent; a compact concentric-circle glyph lands near 12%. Same
drawing hand, 2× difference in apparent weight once both are fitted to 24 units.
Left alone the set looks like it came from two different icon families.

So `forge.py` dilates each glyph until it reaches `TARGET_STROKE_RATIO` (8.5%,
which reproduces the 1.36 px that the old hand-drawn nav icons rendered at 17 px).
Dilate-only, never erode — an over-weight glyph is usually built from solid shapes
where the run-length metric is not measuring a stroke at all, and eroding would eat
it.

Every icon reports its ratio and the resulting px at 17, and the run exits non-zero
if any glyph lands under `MIN_STROKE_PX_AT_17`. When that happens the fix is the
prompt, not the tracer — dilation cannot add weight that survives as a shape.

## Tuning

`forge.py`'s knobs, near the top:

- `TARGET_STROKE_RATIO` (0.085) — the weight every glyph is dilated toward.
- `MIN_STROKE_PX_AT_17` (0.9) — the floor that fails the run.
- `MAX_THICKEN_PASSES` (10) — dilation also closes counters, so this caps it. A
  glyph that hits the cap is badly composed rather than merely thin.
- `TURDSIZE` (100) drops speckles by area. Seedream likes to leave a stray dot in
  a corner, which reads as dirt at 24 px. At the 512 px working resolution a
  deliberate small feature is ~175 px² and generation noise is well under 100.
- `ALPHAMAX` (0.9) below 1.0 keeps corners sharp. The potrace default of 1.0
  rounds them, which fights the geometric look.
- `PADDING` (1.6) is the margin inside the 24×24 viewBox, matching
  `website/sprite.svg`.

## Application icon

`appicon.py`, deliberately a separate script. An app icon is *born* raster — Tauri
wants an `.ico` plus a spread of PNGs — so there is nothing to vectorise, and the
judgement is "does this read in a 16 px taskbar", not "does this match a stroke
weight". It writes `out/appicon/<variant>/{16,32,48,64,128,256,512}.png` plus an
`index.html` contact sheet, and touches nothing under `src-tauri`.

Two sources. `brand-*` is the actual lamp, drawn with Pillow from
`website/favicon.svg`'s geometry — exact fidelity, crisp at every size, no model
involved. `gen-*` is Seedream's take on the same brief.

Note the body/ground pairing: `logo.svg`'s own comment observes that on a dark tab
strip the ink recedes and the green flame carries the mark alone. Fine for a favicon
sitting beside a page title, useless for a taskbar icon that has nothing else to
identify it — so a dark badge gets a light lamp body, not the ink one.

On the first pass the generated candidates all lost to the authored lamp: thin
outlines that die at 16 px, a flame drifting yellow off `#A3E635`, a beam variant
that made the whole body green and so inverted "green = flame only". Worth keeping
the script for exploration, but the brand answer is the drawn one.

`--install <variant>` copies a candidate into `apps/client/src-tauri/icons/` under
the filenames `tauri icon` would use, building `icon.ico` as a real
multi-resolution RGBA icon (16/32/48/64/128/256 — Explorer picks per context).
`.icns` is skipped: this client is Windows-only. **What ships is `brand-light`**;
`bundle.icon` in `tauri.conf.json` points at it. Before this the key was absent
entirely and the app had no icon at all.

## Two footguns

**potracer traces the zero region.** Despite the name `Bitmap` and the
pypotrace convention, `potracer` treats *False* as the shape. Hand it True-at-ink
and it traces the background instead, emitting a canvas-sized rectangle with the
glyph punched out as a hole. `to_bitmap()` inverts for this reason.

**Ink must not touch the bitmap edge.** If it does, potrace traces the edge as
part of the shape. `to_bitmap()` pads with 8 px of background unconditionally
rather than trusting Seedream to leave a margin.
