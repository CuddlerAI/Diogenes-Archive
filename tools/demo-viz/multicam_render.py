#!/usr/bin/env python3
"""Render the demo composite MP4 for engine-side multi-camera capture packages.

Third product line, next to gameplay_render.py (annotator capture) and the
depth-delivery-viz skill (depth packages). Here the sellable property is that
four cameras observe one scene on one clock with a measured co-visibility
guarantee -- so the render has to show the four lanes together, their poses in
a shared world frame, and the co-visibility curve that the delivery gate reads.

Layout 1920x1080 at the package frame rate:
  left-top     2x2 grid of the four camera lanes
  right-top    shared world frame: four trajectories, live frustums, ground grid
  bottom-left  co-visibility per camera pair over time + per-lane speed
  bottom-right package facts read from plan/covis/provenance/qc-report

Input directory (engine-data delivery shape):
  video_N.mp4  action_camera_N.json  systeminfo_N.json  for N in 1..4
  plan.json  covis.json  provenance.json  [dyn_metrics.json]  [qc-report.json]

Usage:
  multicam_render.py --pkg DIR --out demo.mp4 [--sub "..."] [--seconds N] [--crf 23]
"""
import argparse
import json
import os
import subprocess
import sys
import time

import cv2
import numpy as np
from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "..", ".claude", "skills",
                                "depth-delivery-viz", "scripts"))
from renderlib import (View, draw_polyline, draw_frustum, draw_grid,  # noqa: E402
                       draw_axes, draw_seg)

W, H = 1920, 1080
CW, CH = 640, 360          # per-lane tile
GRID_W, GRID_H = 1280, 720
V3_W, V3_H = 640, 720
BAND_Y = 720
TITLE_SECONDS = 2.0
BG = (16, 15, 14)
PANEL = (24, 23, 22)
FG = (232, 232, 232)
DIM = (150, 150, 150)
ORANGE = (0, 165, 255)
# One colour per lane, reused for the tile border, the trajectory and the legend
# so a reader can tie the three together without a key.
LANE_COL = [(90, 255, 140), (255, 176, 92), (140, 160, 255), (200, 140, 255)]
PAIR_COL = {"1-2": (90, 255, 140), "1-3": (255, 176, 92), "2-3": (140, 160, 255)}

# covis.json states its method in Chinese; the mono face used for the facts card
# has no CJK coverage, so an untranslated string would render as tofu boxes.
# Known strings get an English equivalent, anything else defers to the file.
METHOD_EN = {
    "图像空间 8x8 均匀采样的双向重投影 overlap（等面积加权）":
        "bidirectional reprojection overlap, 8x8 uniform image-space "
        "sampling, equal-area weighted",
}


def ascii_or(text, fallback):
    text = str(text)
    if text in METHOD_EN:
        return METHOD_EN[text]
    return text if text.isascii() else fallback


class Text:
    def __init__(self, font_path, mono_path):
        self.p, self.m, self._c = font_path, mono_path, {}

    def font(self, size, mono=False):
        k = (size, mono)
        if k not in self._c:
            self._c[k] = ImageFont.truetype(self.m if mono else self.p, size)
        return self._c[k]

    def block(self, img, x, y, lines, size=14, color=FG, mono=False, lh=None,
              anchor=None):
        lh = lh or size + 5
        pil = Image.fromarray(img[:, :, ::-1])
        d = ImageDraw.Draw(pil)
        f = self.font(size, mono)
        for i, ln in enumerate(lines):
            col = color
            if isinstance(ln, tuple):
                ln, col = ln
            d.text((x, y + i * lh), ln, font=f, fill=tuple(col[::-1]), anchor=anchor)
        img[:, :] = np.array(pil)[:, :, ::-1]

    def draw(self, img, xy, text, size=14, color=FG, mono=False, anchor=None):
        self.block(img, xy[0], xy[1], [text], size, color, mono, anchor=anchor)


class MultiCamRenderer:
    def __init__(self, pkg, sub, txt, seconds=None):
        self.pkg, self.sub, self.t = pkg, sub, txt
        self.ac = []
        for n in (1, 2, 3, 4):
            p = os.path.join(pkg, f"action_camera_{n}.json")
            self.ac.append(json.load(open(p)) if os.path.exists(p) else [])
        self.nlane = sum(1 for a in self.ac if a)
        self.fps = float(self.ac[0][0].get("fps", 30.0))
        self.n = min(len(a) for a in self.ac if a)
        if seconds:
            self.n = min(self.n, int(seconds * self.fps))
        self.plan = self._load("plan.json")
        self.covis = self._load("covis.json")
        self.prov = self._load("provenance.json")
        self.dyn = self._load("dyn_metrics.json")
        self.qc = self._load("qc-report.json")
        self.traj = [np.array([e["camera_position"] for e in a[:self.n]])
                     if a else None for a in self.ac]
        allp = np.concatenate([t for t in self.traj if t is not None])
        # Frame on the correlated trio: cam4 is a wide observer that can sit far
        # outside the action, and averaging it in pushes the eye so far back that
        # the trajectories collapse to a smudge.
        trio = np.concatenate([t for t in self.traj[:3] if t is not None])
        self.center = trio.mean(0)
        span = trio.max(0) - trio.min(0)
        self.radius = max(45.0, 0.95 * float(np.linalg.norm(span[[0, 2]])))
        self.ground = float(np.percentile(allp[:, 1], 2)) - 2.0
        self.view = View(V3_W, V3_H, 50)
        self.speed = [np.linalg.norm(np.array([e["camera_speed"] for e in a[:self.n]]),
                                     axis=1) if a else None for a in self.ac]
        self._covis_strip = self._build_covis()
        self._speed_strip = self._build_speed()
        self._facts = self._build_facts()

    def _load(self, name):
        p = os.path.join(self.pkg, name)
        return json.load(open(p)) if os.path.exists(p) else {}

    # ---- static strips ---------------------------------------------------
    def _build_covis(self):
        wpx, hpx = 1150, 128
        img = np.full((hpx, wpx, 3), PANEL, np.uint8)
        curves = (self.covis or {}).get("pair_curves") or {}
        gate = float((self.covis or {}).get("overlap_min", 0.30))
        gy = int(hpx - gate * (hpx - 14)) - 7
        cv2.line(img, (0, gy), (wpx, gy), (70, 70, 160), 1)
        for name, ys in curves.items():
            ys = np.asarray(ys, np.float32)
            xs = (np.linspace(0, 1, len(ys)) * (wpx - 1)).astype(np.int32)
            pts = np.stack([xs, (hpx - ys * (hpx - 14) - 7)], -1).astype(np.int32)
            cv2.polylines(img, [pts], False, PAIR_COL.get(name, FG), 1, cv2.LINE_AA)
        return img, gate, gy

    def _build_speed(self):
        wpx, hpx = 1150, 84
        img = np.full((hpx, wpx, 3), PANEL, np.uint8)
        mx = max(1e-3, max(float(s.max()) for s in self.speed if s is not None))
        for i, s in enumerate(self.speed):
            if s is None:
                continue
            xs = (np.linspace(0, 1, len(s)) * (wpx - 1)).astype(np.int32)
            pts = np.stack([xs, hpx - 4 - s / mx * (hpx - 12)], -1).astype(np.int32)
            cv2.polylines(img, [pts], False, LANE_COL[i], 1, cv2.LINE_AA)
        return img, mx

    def _build_facts(self):
        pl, cv_, pr = self.plan or {}, self.covis or {}, self.prov or {}
        cams = pl.get("cameras") or []
        focals = ", ".join(f"{c.get('focal_mm', '?')}mm" for c in cams) or "?"
        strict = cv_.get("covis_ratio")
        gate = cv_.get("overlap_min", 0.30)
        qcv = (self.qc or {}).get("verdict")
        qcc = (self.qc or {}).get("counts") or {}
        lines = [
            ("PACKAGE", ORANGE),
            (f"scene        {pl.get('scene', '?')}   seed {pl.get('seed', '?')}", DIM),
            (f"asset        {pr.get('scene_asset_id', '?')}", DIM),
            (f"mode         {pl.get('mode', '?')}   {pl.get('duration_s', '?')} s"
             f" @ {pl.get('fps', self.fps):g} fps   aspect {pl.get('aspect', '?')}", DIM),
            (f"lanes        {self.nlane} cameras, focal {focals}", DIM),
            (f"sdk          {pr.get('sdk_version', '?')}", DIM),
            ("", DIM),
            ("CO-VISIBILITY", ORANGE),
            (f"per-pair gate      min(A->B, B->A) >= {gate:.2f}", DIM),
            (f"strict ratio       {strict if strict is not None else '?'}"
             f"   (all three pairs of cam1-3 simultaneously)",
             (90, 255, 140) if (strict or 0) >= 0.5 else (90, 200, 255)),
            (f"any-pair ratio     {cv_.get('covis_ratio_any_pair', '?')}", DIM),
            (f"method             {ascii_or(cv_.get('method', '?'), 'see covis.json')[:56]}", DIM),
            ("", DIM),
            ("QC", ORANGE),
        ]
        if qcv:
            lines.append((f"verdict      {qcv}   "
                          f"PASS {qcc.get('PASS', 0)} / FAIL {qcc.get('FAIL', 0)}"
                          f" / SKIP {qcc.get('SKIP', 0)}",
                          (90, 255, 140) if qcv == "PASS" else (90, 200, 255)))
            fails = sorted({r["rule"] for r in (self.qc.get("results") or [])
                            if r.get("status") == "FAIL"})
            for f in fails[:3]:
                lines.append((f"  failing rule   {f}", (90, 200, 255)))
        else:
            lines.append(("verdict      no qc-report.json in package", (90, 200, 255)))
        lines += [
            ("", DIM),
            ("RIGHTS", ORANGE),
            (f"license      {pr.get('license_id', '?')}", DIM),
            (f"scope        {ascii_or(pr.get('license', '?'), 'see provenance.json')}",
             (120, 190, 255)),
        ]
        return lines

    # ---- panels ----------------------------------------------------------
    def _grid(self, tiles, i):
        img = np.full((GRID_H, GRID_W, 3), BG, np.uint8)
        for k in range(4):
            r, c = divmod(k, 2)
            y0, x0 = r * CH, c * CW
            if tiles[k] is not None:
                img[y0:y0 + CH, x0:x0 + CW] = tiles[k]
            cv2.rectangle(img, (x0, y0), (x0 + CW - 1, y0 + CH - 1), LANE_COL[k], 2)
            e = self.ac[k][i] if self.ac[k] else None
            role = "?"
            for cam in (self.plan or {}).get("cameras") or []:
                if cam.get("id") == k + 1:
                    segs = cam.get("segments") or []
                    role = segs[0].get("cls", "?") if segs else "?"
            lbl = [f"cam {k + 1}   {role}"]
            if e:
                p = e["camera_position"]
                lbl.append(f"pos ({p[0]:+.1f}, {p[1]:+.1f}, {p[2]:+.1f})   "
                           f"|v| {float(np.linalg.norm(e['camera_speed'])):.1f} m/s")
            cv2.rectangle(img, (x0 + 4, y0 + 4), (x0 + 336, y0 + 44), (14, 14, 14), -1)
            self.t.block(img, x0 + 10, y0 + 8, lbl, 12, LANE_COL[k], lh=16)
        return img

    def _world(self, i):
        img = np.full((V3_H, V3_W, 3), BG, np.uint8)
        tt = i / max(1, self.n - 1)
        az = np.radians(40) + np.radians(26) * np.sin(2 * np.pi * tt)
        el = np.radians(30 + 5 * np.sin(4 * np.pi * tt))
        eye = self.center + np.array([self.radius * np.cos(el) * np.sin(az),
                                      self.radius * np.sin(el) + 12,
                                      self.radius * np.cos(el) * np.cos(az)])
        self.view.look_at(eye, self.center)
        draw_grid(img, self.view, self.center, self.ground, half=110, step=10)
        draw_axes(img, self.view, [self.center[0], self.ground, self.center[2]])
        for k in range(4):
            if self.traj[k] is None:
                continue
            draw_polyline(img, self.view, self.traj[k][::6],
                          tuple(int(x * 0.35) for x in LANE_COL[k]), 1)
            if i > 4:
                idx = np.arange(0, i, 2)
                draw_polyline(img, self.view, self.traj[k][idx], LANE_COL[k], 2)
            e = self.ac[k][i]
            draw_frustum(img, self.view, e, LANE_COL[k], 6.0, 2, top_mark=True)
            t = np.array(e["camera_position"])
            draw_seg(img, self.view, t, [t[0], self.ground, t[2]],
                     tuple(int(x * 0.45) for x in LANE_COL[k]), 1)
        self.t.block(img, 12, 10, [
            (f"[{i}/{self.n - 1}]  t={i / self.fps:06.2f}s", FG),
            ("shared world frame — one clock, four poses", DIM),
        ], 13, FG, lh=17)
        for k in range(4):
            x0 = 12 + k * 74
            cv2.rectangle(img, (x0, V3_H - 28), (x0 + 11, V3_H - 18), LANE_COL[k], -1)
            self.t.draw(img, (x0 + 16, V3_H - 30), f"cam{k + 1}", 12, LANE_COL[k])
        return img

    def _band(self, i):
        img = np.full((H - BAND_Y, W, 3), BG, np.uint8)
        strip, gate, gy = self._covis_strip
        self.t.draw(img, (16, 8), "CO-VISIBILITY — bidirectional overlap per camera pair", 14, ORANGE)
        img[28:28 + strip.shape[0], 90:90 + strip.shape[1]] = strip
        cv2.line(img, (90, 28 + gy), (90 + strip.shape[1], 28 + gy), (70, 70, 160), 1)
        self.t.draw(img, (16, 28 + gy - 7), f"gate {gate:.2f}", 11, (120, 120, 200))
        self.t.block(img, 16, 34, [(p, PAIR_COL[p]) for p in ("1-2", "1-3", "2-3")],
                     11, DIM, lh=15)
        px = int(i / max(1, self.n - 1) * (strip.shape[1] - 1))
        cv2.line(img, (90 + px, 28), (90 + px, 28 + strip.shape[0]), ORANGE, 1)

        y = 28 + strip.shape[0] + 16
        sstrip, smax = self._speed_strip
        self.t.draw(img, (16, y), f"CAMERA SPEED — per lane, peak {smax:.1f} m/s", 14, ORANGE)
        y += 22
        img[y:y + sstrip.shape[0], 90:90 + sstrip.shape[1]] = sstrip
        cv2.line(img, (90 + px, y), (90 + px, y + sstrip.shape[0]), ORANGE, 1)

        cv2.line(img, (1268, 6), (1268, H - BAND_Y - 6), (60, 58, 56), 1)
        self.t.block(img, 1288, 8, self._facts, 12, DIM, mono=True, lh=14.5)
        return img

    def _title_card(self):
        im = np.full((H, W, 3), (13, 12, 11), np.uint8)
        name = os.path.basename(os.path.normpath(self.pkg))
        self.t.draw(im, (W // 2, 320), name, 46, (240, 240, 240), anchor="mm")
        if self.sub:
            self.t.draw(im, (W // 2, 392), self.sub, 26, (170, 190, 210), anchor="mm")
        pl = self.plan or {}
        lines = [
            f"{self.nlane} synchronised camera lanes  ·  {self.n} frames"
            f"  ·  {self.n / self.fps:.3f} s  ·  {pl.get('fps', self.fps):g} fps"
            f"  ·  1920x1080 H.264 per lane",
            "action_camera_N.json  per-frame pose quaternion, Euler, intrinsics,"
            " follow offset, speed — one lane per camera",
            "covis.json  bidirectional image-space overlap per camera pair,"
            " sampled through the clip, with the delivery gate applied",
            "plan.json  the motion plan the capture was generated from"
            "   |   provenance.json  scene asset, seed, licence, calibration",
            "Right: the four poses drawn in one world frame — the co-visibility"
            " number below is measured from exactly these poses",
        ]
        y = 512
        for ln in lines:
            self.t.draw(im, (W // 2, y), ln, 22, (150, 150, 150), anchor="mm")
            y += 48
        return im

    def render(self, out_path, crf=20):
        decs = []
        for n in (1, 2, 3, 4):
            v = os.path.join(self.pkg, f"video_{n}.mp4")
            decs.append(subprocess.Popen(
                ["ffmpeg", "-v", "error", "-i", v, "-f", "rawvideo",
                 "-pix_fmt", "bgr24", "-s", f"{CW}x{CH}", "-"],
                stdout=subprocess.PIPE, bufsize=CW * CH * 3 * 4)
                if os.path.exists(v) else None)
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
            tiles = []
            short = False
            for d in decs:
                if d is None:
                    tiles.append(None)
                    continue
                raw = d.stdout.read(CW * CH * 3)
                if len(raw) < CW * CH * 3:
                    short = True
                    tiles.append(None)
                else:
                    tiles.append(np.frombuffer(raw, np.uint8).reshape(CH, CW, 3))
            if short:
                print(f"  a lane ended early at frame {i}", flush=True)
                self.n = i
                break
            canvas[0:GRID_H, 0:GRID_W] = self._grid(tiles, i)
            canvas[0:V3_H, GRID_W:W] = self._world(i)
            canvas[BAND_Y:H, 0:W] = self._band(i)
            cv2.line(canvas, (GRID_W, 0), (GRID_W, BAND_Y), (60, 60, 60), 2)
            cv2.line(canvas, (0, BAND_Y), (W, BAND_Y), (60, 60, 60), 2)
            enc.stdin.write(canvas.tobytes())
            if i % 300 == 0:
                print(f"  {os.path.basename(out_path)} {i}/{self.n} "
                      f"{time.time() - t0:.0f}s", flush=True)
        for d in decs:
            if d:
                d.stdout.close(); d.wait()
        enc.stdin.close(); enc.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pkg", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--sub", default="")
    ap.add_argument("--seconds", type=float, default=None)
    ap.add_argument("--font", default="/System/Library/Fonts/Helvetica.ttc")
    ap.add_argument("--mono", default="/System/Library/Fonts/Menlo.ttc")
    ap.add_argument("--crf", type=int, default=23)
    a = ap.parse_args()

    outdir = os.path.dirname(os.path.abspath(a.out)) or "."
    os.makedirs(outdir, exist_ok=True)
    tag = os.path.splitext(os.path.basename(a.out))[0]
    part = os.path.join(outdir, f"_part_{tag}.mp4")
    r = MultiCamRenderer(a.pkg, a.sub, Text(a.font, a.mono), a.seconds)
    r.render(part, crf=max(18, a.crf - 3))

    # Engine mix ships as audio_mix.wav; lane videos also carry it, so prefer the
    # standalone file and fall back to lane 1.
    mix = os.path.join(a.pkg, "audio_mix.wav")
    src = mix if os.path.exists(mix) else os.path.join(a.pkg, "video_1.mp4")
    audio = os.path.join(outdir, f"_audio_{tag}.m4a")
    fc = (f"aevalsrc=0:c=stereo:s=48000:d={TITLE_SECONDS}[s];"
          f"[0:a]atrim=end={r.n / r.fps:.6f},asetpts=N/SR/TB[c];"
          f"[s][c]concat=n=2:v=0:a=1[a]")
    have = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", src,
                           "-filter_complex", fc, "-map", "[a]",
                           "-c:a", "aac", "-b:a", "128k", audio]).returncode == 0
    cmd = ["ffmpeg", "-v", "error", "-y", "-i", part]
    if have:
        cmd += ["-i", audio, "-c:a", "copy"]
    cmd += ["-c:v", "libx264", "-preset", "medium", "-crf", str(a.crf),
            "-pix_fmt", "yuv420p", "-movflags", "+faststart", a.out]
    subprocess.run(cmd, check=True)
    for p in (part, audio):
        if os.path.exists(p):
            os.remove(p)
    print("wrote", a.out)


if __name__ == "__main__":
    main()
