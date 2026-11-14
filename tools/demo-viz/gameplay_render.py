#!/usr/bin/env python3
"""Render the demo composite MP4 for human-annotator gameplay capture clips.

This is the counterpart to .claude/skills/depth-delivery-viz for the other
product line: commercial-title gameplay recorded by a human annotator, where
the sellable artifact is not depth but the per-frame action record and the
single clock that ties it to the video.

Layout 1920x1080, rendered at the clip's native frame rate:
  top-left     RGB video + live action HUD (held keys, mouse vector, buttons)
  bottom-left  scrolling timelines: key piano-roll, pointer delta trace, audio
  right-top    the actual frames.jsonl row for the frame on screen, verbatim
  right-mid    action-label lanes across the whole clip with a playhead
  right-bottom provenance and timing-integrity card read from meta.json

Multiple clips are rendered as parts with a title card each and concatenated,
so one file can walk a buyer across several games.

Input per clip directory (names as delivered, hash suffixes tolerated):
  video.mp4  frames.jsonl[.zst]  events.jsonl[.zst]  meta.json  audio.pcm[.zst]

Usage:
  gameplay_render.py --clips DIR [DIR ...] --out demo.mp4 [--subs "..." ...]
      [--crf 23] [--font <ttf>] [--seconds N]

ffmpeg is an external session tool (decode/encode); rendering is numpy/cv2.
"""
import argparse
import glob
import json
import os
import subprocess
import sys
import time
import zstandard

import cv2
import numpy as np
from PIL import Image, ImageDraw, ImageFont

W, H = 1920, 1080
PW, PH = 960, 540          # video / timeline panel size
TITLE_SECONDS = 2.0
BG = (16, 15, 14)
PANEL = (22, 21, 20)
GRID = (52, 50, 48)
FG = (232, 232, 232)
DIM = (150, 150, 150)
ACCENT = (90, 255, 140)    # BGR green, matches the depth renderer's HUD
ORANGE = (0, 165, 255)
BLUE = (255, 176, 92)

# Windows virtual-key codes seen in the capture stream. Only the keys a shooter
# or driving title actually binds are drawn; anything else falls back to "VK<n>".
VK = {
    8: 'Bksp', 9: 'Tab', 13: 'Enter', 16: 'Shift', 17: 'Ctrl', 18: 'Alt',
    20: 'Caps', 27: 'Esc', 32: 'Space', 37: 'Left', 38: 'Up', 39: 'Right',
    40: 'Down', 48: '0', 49: '1', 50: '2', 51: '3', 52: '4', 53: '5',
    54: '6', 55: '7', 56: '8', 57: '9',
    65: 'A', 66: 'B', 67: 'C', 68: 'D', 69: 'E', 70: 'F', 71: 'G', 72: 'H',
    73: 'I', 74: 'J', 75: 'K', 76: 'L', 77: 'M', 78: 'N', 79: 'O', 80: 'P',
    81: 'Q', 82: 'R', 83: 'S', 84: 'T', 85: 'U', 86: 'V', 87: 'W', 88: 'X',
    89: 'Y', 90: 'Z',
}
# Rows of the piano-roll, in the order a reader scans them.
ROLL_KEYS = [87, 65, 83, 68, 16, 32, 17, 69, 82, 81, 70, 9]
MOUSE_BTN = {0: 'LMB', 1: 'RMB', 2: 'MMB', 3: 'X1', 4: 'X2'}


def keyname(code):
    return VK.get(code, f"VK{code}")


# Recipient-facing material must not name our capture tooling, the GPU vendor, or
# the internal input-path identifiers. These strings live in the source metadata,
# so the renderer rewrites them on the way out rather than us hand-editing packs.
# Substance is preserved -- only the internal name is replaced.
REDACT = [
    ("dataminer-", ""),
    ("DataMiner", "capture client"),
    ("raw_input_v1", "device-level v1"),
    ("raw_input", "device-level"),
    ("NVIDIA H.264 Encoder MFT", "hardware H.264 encoder"),
    ("NVIDIA", "hardware"),
    ("WGC", "windowed capture"),
    ("QueryPerformanceCounter", "high-resolution counter"),
    ("QueryPerformance", "high-resolution counter"),
]


def clean(text):
    s = str(text)
    for a, b in REDACT:
        s = s.replace(a, b)
    return s


# ---------------------------------------------------------------- loading

def _find(clip, *stems):
    """Delivered files carry a -<sha256> suffix; accept both shapes."""
    for stem in stems:
        base, ext = stem.split(".", 1)
        for pat in (os.path.join(clip, stem),
                    os.path.join(clip, f"{base}-*.{ext}")):
            hit = sorted(glob.glob(pat))
            if hit:
                return hit[0]
    return None


def _read_lines(path):
    if path is None:
        return []
    raw = open(path, "rb").read()
    if path.endswith(".zst"):
        raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=1 << 30)
    return [json.loads(l) for l in raw.decode("utf-8").splitlines() if l.strip()]


def load_clip(clip):
    meta_p = _find(clip, "meta.json")
    meta = json.load(open(meta_p)) if meta_p else {}
    frames = _read_lines(_find(clip, "frames.jsonl.zst", "frames.jsonl"))
    events = _read_lines(_find(clip, "events.jsonl.zst", "events.jsonl"))
    video = _find(clip, "video.mp4")
    audio_p = _find(clip, "audio.pcm.zst", "audio.pcm")
    audio = None
    if audio_p:
        raw = open(audio_p, "rb").read()
        if audio_p.endswith(".zst"):
            raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=1 << 31)
        a = np.frombuffer(raw, np.int16).astype(np.float32) / 32768.0
        audio = a.reshape(-1, 2).mean(1) if a.size % 2 == 0 else a
    return dict(dir=clip, meta=meta, frames=frames, events=events,
                video=video, audio=audio)


# ---------------------------------------------------------------- text

class Text:
    """PIL text blitting with a cached font ladder (cv2 has no CJK/mono)."""

    def __init__(self, font_path, mono_path=None):
        self.font_path = font_path
        self.mono_path = mono_path or font_path
        self._cache = {}

    def font(self, size, mono=False):
        key = (size, mono)
        if key not in self._cache:
            self._cache[key] = ImageFont.truetype(
                self.mono_path if mono else self.font_path, size)
        return self._cache[key]

    def draw(self, img, xy, text, size=15, color=FG, mono=False, anchor=None):
        pil = Image.fromarray(img[:, :, ::-1])
        d = ImageDraw.Draw(pil)
        d.text(xy, text, font=self.font(size, mono), fill=tuple(color[::-1]),
               anchor=anchor)
        img[:, :] = np.array(pil)[:, :, ::-1]

    def block(self, img, x, y, lines, size=15, color=FG, mono=False, lh=None):
        """Batch many lines through one PIL round-trip (per-line is far slower)."""
        lh = lh or size + 5
        pil = Image.fromarray(img[:, :, ::-1])
        d = ImageDraw.Draw(pil)
        f = self.font(size, mono)
        for i, ln in enumerate(lines):
            col = color
            if isinstance(ln, tuple):
                ln, col = ln
            d.text((x, y + i * lh), ln, font=f, fill=tuple(col[::-1]))
        img[:, :] = np.array(pil)[:, :, ::-1]


# ---------------------------------------------------------------- renderer

class ClipRenderer:
    def __init__(self, clip, sub, txt, seconds=None):
        self.c = clip
        self.sub = sub
        self.t = txt
        m = clip["meta"]
        self.fps = (m.get("fps_num", 60) / m.get("fps_den", 1)) if m else 60.0
        self.frames = clip["frames"]
        if seconds:
            self.frames = self.frames[:int(seconds * self.fps)]
        self.n = len(self.frames)
        self.w = m.get("width", 1920)
        self.h = m.get("height", 1080)
        self.game = clean((m.get("catalog", {}).get("game", {}) or {}).get("id", "?"))
        self.title = clean(m.get("target", {}).get("game_id", self.game))
        # Set by concat_segments.py when this directory is several 10 s segments
        # rejoined. Only affects labelling — see the HUD note below.
        self.joined = bool(m.get("concatenated_from"))
        # Per-frame derived series, computed once.
        self.pdx = np.array([f.get("pointerDx", 0) for f in self.frames], np.float32)
        self.pdy = np.array([f.get("pointerDy", 0) for f in self.frames], np.float32)
        self.keysets = [set(f.get("keys") or []) for f in self.frames]
        self.btnsets = [set(f.get("buttons") or []) for f in self.frames]
        self.actions = [f.get("actions") or [] for f in self.frames]
        lanes = {}
        for a in self.actions:
            for name in a:
                lanes.setdefault(name, 0)
                lanes[name] += 1
        self.lane_names = [k for k, _ in sorted(lanes.items(), key=lambda kv: -kv[1])][:9]
        self.lane_count = lanes
        self._roll = self._build_roll()
        self._lanes = self._build_lanes()
        self._wave = self._build_wave()
        self._facts = self._build_facts()

    # ---- static strips ---------------------------------------------------
    def _build_roll(self):
        """Key piano-roll: rows = keys, columns = frames, full clip width."""
        rows, wpx = len(ROLL_KEYS), PW - 120
        img = np.full((rows * 18, wpx, 3), PANEL, np.uint8)
        xs = (np.arange(self.n) / max(1, self.n - 1) * (wpx - 1)).astype(np.int32)
        for r, code in enumerate(ROLL_KEYS):
            y0 = r * 18
            img[y0:y0 + 17, :] = (30, 29, 28) if r % 2 else (25, 24, 23)
            on = np.array([code in s for s in self.keysets])
            if on.any():
                img[y0 + 3:y0 + 14, xs[on]] = ACCENT
        return img

    def _build_lanes(self):
        """Action-label lanes across the clip."""
        rows, wpx = len(self.lane_names), 940 - 190
        if rows == 0:
            return np.full((10, wpx, 3), PANEL, np.uint8)
        img = np.full((rows * 22, wpx, 3), PANEL, np.uint8)
        xs = (np.arange(self.n) / max(1, self.n - 1) * (wpx - 1)).astype(np.int32)
        for r, name in enumerate(self.lane_names):
            y0 = r * 22
            img[y0:y0 + 20, :] = (30, 29, 28) if r % 2 else (25, 24, 23)
            on = np.array([name in a for a in self.actions])
            if on.any():
                img[y0 + 4:y0 + 17, xs[on]] = BLUE
        return img

    def _build_wave(self):
        wpx, hpx = PW - 120, 60
        img = np.full((hpx, wpx, 3), PANEL, np.uint8)
        a = self.c["audio"]
        if a is None or a.size == 0:
            return img
        seg = np.array_split(a[:int(self.n / self.fps * 48000)] if a.size else a, wpx)
        env = np.array([np.abs(s).max() if s.size else 0.0 for s in seg])
        env = np.clip(env / max(1e-6, env.max()), 0, 1)
        for x in range(wpx):
            hh = int(env[x] * (hpx // 2 - 2))
            cv2.line(img, (x, hpx // 2 - hh), (x, hpx // 2 + hh), (120, 200, 255), 1)
        return img

    def _build_facts(self):
        m = self.c["meta"]
        cat = m.get("catalog", {})
        vid, fr, ev = m.get("video", {}), m.get("frames", {}), m.get("events", {})
        enc = m.get("effective_encoder", {})
        aud = m.get("audio", {}) or {}
        dup = sum(1 for f in self.frames if f.get("duplicate"))
        sup = sum(int(f.get("source_superseded") or 0) for f in self.frames)
        drop = sum(int(f.get("queue_drop") or 0) for f in self.frames)
        qs, qe = m.get("qpc_start"), m.get("qpc_end")
        hz = aud.get("qpc_frequency") or 10_000_000
        wall = (qe - qs) / hz if (qs and qe) else self.n / self.fps
        return [
            ("PROVENANCE", ORANGE),
            (clean(f"game id        {cat.get('game', {}).get('id', '?')}"), DIM),
            (clean(f"mapping        {(m.get('mapping') or {}).get('id', '?')}"
                   f"  v{(m.get('mapping') or {}).get('version', '?')}"), DIM),
            (clean(f"catalog        {cat.get('catalog_id', '?')}"
                   f"  v{cat.get('catalog_version', '?')}"), DIM),
            (clean(f"input path     {m.get('input_status', '?')}"), DIM),
            (clean(f"schema         {m.get('schema_version', '?')}"), DIM),
            ("", DIM),
            ("TIMING INTEGRITY", ORANGE),
            (f"frames         {self.n}  @ {self.fps:g} fps CFR", DIM),
            (f"wall clock     {wall:.3f} s  ({hz/1e6:g} MHz counter, single time base)", DIM),
            (f"duplicate      {dup}", ACCENT if dup == 0 else (90, 200, 255)),
            (f"superseded     {sup}", ACCENT if sup == 0 else (90, 200, 255)),
            (f"queue drops    {drop}", ACCENT if drop == 0 else (90, 200, 255)),
            (f"event rows     {ev.get('lines', len(self.c['events']))}"
             f"   frame rows {fr.get('lines', self.n)}", DIM),
            ("", DIM),
            ("CARRIER", ORANGE),
            (f"video          {self.w}x{self.h} H.264  {vid.get('bytes', 0)/1e6:.1f} MB", DIM),
            (clean(f"encoder        {enc.get('friendly_name', '?')}"), DIM),
            (f"audio          {aud.get('format', {}).get('sample_rate', '?')} Hz"
             f"  {aud.get('format', {}).get('channels', '?')} ch"
             f"  {aud.get('source', '?')}", DIM),
            (f"integrity      sha256 per file, manifest-sealed", DIM),
            ("", DIM),
            (f"non_trainable = {str(m.get('non_trainable', True)).lower()}"
             f"   production_certified = {str(m.get('production_certified', False)).lower()}",
             (120, 190, 255)),
        ]

    # ---- dynamic panels --------------------------------------------------
    def _hud(self, panel, i):
        f = self.frames[i]
        roi = panel[10:150, 8:392]
        roi[:] = (roi.astype(np.float32) * 0.32).astype(np.uint8)
        cv2.rectangle(panel, (8, 10), (392, 150), (70, 70, 70), 1)
        held = self.keysets[i]
        x, y = 18, 74
        for code in ROLL_KEYS[:8]:
            nm = keyname(code)
            wb = 26 if len(nm) == 1 else 46
            on = code in held
            cv2.rectangle(panel, (x, y), (x + wb, y + 24),
                          (60, 220, 90) if on else (95, 95, 95), -1 if on else 1)
            self.t.draw(panel, (x + 5, y + 4), nm, 13,
                        (10, 30, 10) if on else (185, 185, 185))
            x += wb + 4
        # mouse delta vector
        mx0, my0 = 300, 58
        cv2.rectangle(panel, (mx0, my0), (mx0 + 62, my0 + 62), (95, 95, 95), 1)
        cx0, cy0 = mx0 + 31, my0 + 31
        dx, dy = float(self.pdx[i]), float(self.pdy[i])
        tip = (int(np.clip(cx0 + dx * 0.45, mx0 + 2, mx0 + 60)),
               int(np.clip(cy0 + dy * 0.45, my0 + 2, my0 + 60)))
        cv2.line(panel, (cx0, cy0), tip, ACCENT, 1, cv2.LINE_AA)
        cv2.circle(panel, tip, 2, ACCENT, -1)
        btns = " ".join(MOUSE_BTN.get(b, str(b)) for b in sorted(self.btnsets[i]))
        # `frame` and `t` count the clip we are showing; `video_t_us` is the row's
        # own field and is measured from the start of its 10 s segment. On a
        # concatenated clip the two disagree on purpose — t=30.000s next to
        # video_t=0.0ms is a segment boundary, not a cut — so say which is which
        # rather than leave a viewer to decide we spliced something.
        vt = f"video_t={f.get('video_t_us', 0) / 1000:.1f}ms"
        if self.joined:
            vt += " (segment-local)"
        self.t.block(panel, 18, 18, [
            (f"frame {i}/{self.n - 1}   t={i / self.fps:06.3f}s   {vt}", ACCENT),
            (f"pointer  dx={dx:+.0f}  dy={dy:+.0f}   buttons [{btns}]", ACCENT),
        ], 13, ACCENT, lh=17)
        self.t.draw(panel, (mx0, my0 + 64), "mouse", 11, DIM)

    def _timeline(self, i):
        img = np.full((PH, PW, 3), BG, np.uint8)
        px = lambda wpx: int(i / max(1, self.n - 1) * (wpx - 1))
        self.t.draw(img, (14, 10), "INPUT — held keys, whole clip", 14, ORANGE)
        rh = self._roll.shape[0]
        img[32:32 + rh, 100:100 + self._roll.shape[1]] = self._roll
        self.t.block(img, 58, 34, [keyname(k) for k in ROLL_KEYS], 11, DIM, lh=18)
        cv2.line(img, (100 + px(self._roll.shape[1]), 32),
                 (100 + px(self._roll.shape[1]), 32 + rh), ORANGE, 1)
        y = 32 + rh + 18
        self.t.draw(img, (14, y), "POINTER — per-frame delta (px)", 14, ORANGE)
        y += 22
        ph = 92
        img[y:y + ph, 100:100 + PW - 120] = PANEL
        wpx = PW - 120
        xs = (np.arange(self.n) / max(1, self.n - 1) * (wpx - 1)).astype(np.int32)
        scale = max(8.0, float(np.abs(np.concatenate([self.pdx, self.pdy])).max()))
        mid = y + ph // 2
        cv2.line(img, (100, mid), (100 + wpx, mid), GRID, 1)
        for series, col in ((self.pdx, (110, 220, 255)), (self.pdy, (255, 150, 110))):
            pts = np.stack([100 + xs,
                            mid - (series / scale * (ph // 2 - 3))], -1).astype(np.int32)
            cv2.polylines(img, [pts], False, col, 1, cv2.LINE_AA)
        self.t.block(img, 14, y + 8, [("dx", (110, 220, 255)), ("dy", (255, 150, 110)),
                                      (f"±{scale:.0f}", DIM)], 11, DIM, lh=15)
        cv2.line(img, (100 + px(wpx), y), (100 + px(wpx), y + ph), ORANGE, 1)
        y += ph + 18
        self.t.draw(img, (14, y), "AUDIO — process loopback envelope", 14, ORANGE)
        y += 22
        wh = self._wave.shape[0]
        img[y:y + wh, 100:100 + self._wave.shape[1]] = self._wave
        cv2.line(img, (100 + px(self._wave.shape[1]), y),
                 (100 + px(self._wave.shape[1]), y + wh), ORANGE, 1)
        return img

    def _right(self, i):
        img = np.full((H, PW, 3), BG, np.uint8)
        f = self.frames[i]
        self.t.draw(img, (20, 14), "THE RECORD — this frame's row of frames.jsonl", 15, ORANGE)
        # Pretty-print with the long arrays trimmed so it stays readable.
        shown = dict(f)
        ev = shown.get("events") or []
        if len(ev) > 3:
            shown["events"] = ev[:3] + [f"... {len(ev) - 3} more"]
        js = json.dumps(shown, indent=1, ensure_ascii=False)
        lines = js.splitlines()[:26]
        cv2.rectangle(img, (16, 36), (PW - 16, 448), PANEL, -1)
        cv2.rectangle(img, (16, 36), (PW - 16, 448), (60, 58, 56), 1)
        self.t.block(img, 26, 44, lines, 13, (200, 214, 200), mono=True, lh=15.5)

        y = 470
        self.t.draw(img, (20, y), "ACTION LABELS — decoded from the device input stream", 15, ORANGE)
        y += 24
        lh = self._lanes.shape[0]
        img[y:y + lh, 190:190 + self._lanes.shape[1]] = self._lanes
        self.t.block(img, 20, y + 3, [
            (f"{n}  ({self.lane_count[n]})", BLUE if n in (self.actions[i] or []) else DIM)
            for n in self.lane_names], 11, DIM, lh=22)
        px = int(i / max(1, self.n - 1) * (self._lanes.shape[1] - 1))
        cv2.line(img, (190 + px, y), (190 + px, y + lh), ORANGE, 1)

        y = y + lh + 26
        cv2.line(img, (16, y - 10), (PW - 16, y - 10), (60, 58, 56), 1)
        self.t.block(img, 20, y, self._facts, 13, DIM, mono=True, lh=15.5)
        return img

    def _title_card(self):
        im = np.full((H, W, 3), (13, 12, 11), np.uint8)
        self.t.draw(im, (W // 2, 330), self.title, 52, (240, 240, 240), anchor="mm")
        if self.sub:
            self.t.draw(im, (W // 2, 404), self.sub, 27, (170, 190, 210), anchor="mm")
        top = [n for n in self.lane_names[:6]]
        lines = [
            f"video.mp4  {self.w}x{self.h}  {self.fps:g} fps CFR  H.264"
            f"   |   {self.n} frames  {self.n / self.fps:.3f} s",
            "frames.jsonl  one row per video frame: held keys, mouse buttons,"
            " pointer delta, decoded action labels",
            "events.jsonl  the device input event stream with hardware timestamps,"
            " kept alongside the frame-aligned view",
            "audio.pcm  process loopback, same clock   |   meta.json  provenance,"
            " encoder state, integrity counters",
            f"action labels present: {', '.join(top)}",
        ]
        y = 520
        for ln in lines:
            self.t.draw(im, (W // 2, y), ln, 23, (150, 150, 150), anchor="mm")
            y += 50
        return im

    # ---- drive -----------------------------------------------------------
    def render(self, out_path, crf=20):
        dec = subprocess.Popen(
            ["ffmpeg", "-v", "error", "-i", self.c["video"],
             "-f", "rawvideo", "-pix_fmt", "bgr24", "-s", f"{PW}x{PH}", "-"],
            stdout=subprocess.PIPE, bufsize=PW * PH * 3 * 4)
        enc = subprocess.Popen(
            ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgr24",
             "-s", f"{W}x{H}", "-r", f"{self.fps:g}", "-i", "-",
             "-c:v", "libx264", "-preset", "veryfast", "-crf", str(crf),
             "-pix_fmt", "yuv420p", out_path],
            stdin=subprocess.PIPE)
        tc = self._title_card()
        for _ in range(int(TITLE_SECONDS * self.fps)):
            enc.stdin.write(tc.tobytes())
        canvas = np.zeros((H, W, 3), np.uint8)
        t0 = time.time()
        for i in range(self.n):
            raw = dec.stdout.read(PW * PH * 3)
            if len(raw) < PW * PH * 3:
                print(f"  video ended early at frame {i}", flush=True)
                self.n = i
                break
            rgb = np.frombuffer(raw, np.uint8).reshape(PH, PW, 3).copy()
            self._hud(rgb, i)
            canvas[0:PH, 0:PW] = rgb
            canvas[PH:H, 0:PW] = self._timeline(i)
            canvas[0:H, PW:W] = self._right(i)
            cv2.line(canvas, (PW, 0), (PW, H), (60, 60, 60), 2)
            cv2.line(canvas, (0, PH), (PW, PH), (60, 60, 60), 2)
            enc.stdin.write(canvas.tobytes())
            if i % 300 == 0:
                print(f"  {os.path.basename(out_path)} {i}/{self.n} "
                      f"{time.time() - t0:.0f}s", flush=True)
        dec.stdout.close(); dec.wait()
        enc.stdin.close(); enc.wait()
        return self.n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--clips", nargs="+", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--subs", nargs="*", default=[])
    ap.add_argument("--seconds", type=float, default=None,
                    help="truncate each clip to N seconds (smoke tests)")
    ap.add_argument("--font", default="/System/Library/Fonts/Helvetica.ttc")
    ap.add_argument("--mono", default="/System/Library/Fonts/Menlo.ttc")
    ap.add_argument("--crf", type=int, default=23)
    a = ap.parse_args()

    txt = Text(a.font, a.mono)
    outdir = os.path.dirname(os.path.abspath(a.out)) or "."
    tag = os.path.splitext(os.path.basename(a.out))[0]
    os.makedirs(outdir, exist_ok=True)

    parts, srcs = [], []
    for i, cd in enumerate(a.clips):
        clip = load_clip(cd)
        if not clip["video"] or not clip["frames"]:
            print(f"skip {cd}: missing video or frames.jsonl", file=sys.stderr)
            continue
        sub = a.subs[i] if i < len(a.subs) else ""
        part = os.path.join(outdir, f"_part_{tag}_{i}.mp4")
        print(f"[{i + 1}/{len(a.clips)}] {os.path.basename(cd)}", flush=True)
        r = ClipRenderer(clip, sub, txt, a.seconds)
        r.render(part, crf=max(18, a.crf - 3))
        parts.append(part)
        # Audio ships as a separate loopback PCM file -- video.mp4 carries no
        # audio stream -- so decode it to wav here for the mux below.
        wav = None
        ap_ = _find(cd, "audio.pcm.zst", "audio.pcm")
        if ap_ is not None:
            wav = os.path.join(outdir, f"_a_{tag}_{i}.wav")
            pcm = os.path.join(outdir, f"_a_{tag}_{i}.pcm")
            raw = open(ap_, "rb").read()
            if ap_.endswith(".zst"):
                raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=1 << 31)
            open(pcm, "wb").write(raw)
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "s16le",
                            "-ar", "48000", "-ac", "2", "-i", pcm, wav], check=True)
            os.remove(pcm)
        srcs.append((wav, r.fps, r.n))

    if not parts:
        sys.exit("nothing rendered")

    concat = os.path.join(outdir, f"_concat_{tag}.txt")
    with open(concat, "w") as f:
        for p in parts:
            f.write(f"file '{os.path.abspath(p)}'\n")
    combined = os.path.join(outdir, f"_combined_{tag}.mp4")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0",
                    "-i", concat, "-c", "copy", combined], check=True)

    # Audio: silence under each title card, then that clip's own game audio,
    # trimmed to the rendered length so it cannot drift past its part.
    audio = os.path.join(outdir, f"_audio_{tag}.m4a")
    have_audio = all(w for w, _, _ in srcs)
    if have_audio:
        ins, pre = [], []
        for i, (wav, fps, n) in enumerate(srcs):
            ins += ["-i", wav]
            pre.append(f"aevalsrc=0:c=stereo:s=48000:d={TITLE_SECONDS}[s{i}];")
            pre.append(f"[{i}:a]atrim=end={n / fps:.6f},asetpts=N/SR/TB[c{i}];")
        chain = "".join(pre) + "".join(f"[s{i}][c{i}]" for i in range(len(srcs)))
        fc = chain + f"concat=n={2 * len(srcs)}:v=0:a=1[a]"
        have_audio = subprocess.run(
            ["ffmpeg", "-v", "error", "-y", *ins, "-filter_complex", fc,
             "-map", "[a]", "-c:a", "aac", "-b:a", "128k", audio]).returncode == 0
    for wav, _, _ in srcs:
        if wav and os.path.exists(wav):
            os.remove(wav)

    cmd = ["ffmpeg", "-v", "error", "-y", "-i", combined]
    if have_audio:
        cmd += ["-i", audio, "-c:a", "copy"]
    cmd += ["-c:v", "libx264", "-preset", "medium", "-crf", str(a.crf),
            "-pix_fmt", "yuv420p", "-movflags", "+faststart", a.out]
    subprocess.run(cmd, check=True)
    for p in parts + [concat, combined] + ([audio] if have_audio else []):
        if os.path.exists(p):
            os.remove(p)
    print("wrote", a.out)


if __name__ == "__main__":
    main()
