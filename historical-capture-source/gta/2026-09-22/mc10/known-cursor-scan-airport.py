"""Read-only, fixed-position detection of the real render_0053/frame300 arrow.

Event snapshot/partial handling and PNG SHA checks follow the earlier mc2
known-cursor-scan.py. This standalone copy never modifies that tool or inputs.
Exit 0: no candidate in checked frames; 1: known candidate; 2: error.
Neither exit 0 nor any report grants whole-image or delivery approval.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import sys

from PIL import Image


TEMPLATE_DEFAULT = Path("C:/capture/gta/replus-output/render_0053/frame_000300.png")
TEMPLATE_SHA256 = "71cc4c6d24550e99a2019dceb3983eb299fc203207ed26e38afe3e4c13467aeb"
TEMPLATE_ROI = (1825, 1040, 1855, 1078)  # right/bottom exclusive
IMAGE_SIZE = (1920, 1080)
WHITE_PIXELS = 53
BLACK_PIXELS = 61
LIMITATION = (
    "Only the diagnosed render_0053/frame300 arrow at fixed ROI "
    "[1825,1040,1855,1078] is checked. This is known-template-only, not a "
    "whole-image cursor/HUD detector. Moved, different, scaled or partly "
    "matching cursors may be missed; matching background can be ambiguous. "
    "Unsampled frames are not checked. Even --all covers only complete events "
    "in the captured file snapshot, not future frames or other locations. "
    "Inspect original PNGs, including any new pointer position."
)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def png_rgb(data, label):
    with Image.open(io.BytesIO(data)) as source:
        if source.format != "PNG" or source.size != IMAGE_SIZE:
            raise ValueError(f"{label} is not a 1920x1080 PNG")
        return source.convert("RGB")


def load_template(path):
    data = path.read_bytes()
    if sha256(data) != TEMPLATE_SHA256:
        raise ValueError("template PNG does not match the pinned real frame300 SHA-256")
    image = png_rgb(data, "template")
    x0, y0, x1, y1 = TEMPLATE_ROI
    white, black = [], []
    for y in range(y0, y1):
        for x in range(x0, x1):
            color = image.getpixel((x, y))
            if all(channel >= 242 for channel in color):
                white.append((x, y, color))
            if all(channel <= 24 for channel in color):
                black.append((x, y, color))
    if len(white) != WHITE_PIXELS or len(black) != BLACK_PIXELS:
        raise ValueError("template differs from the diagnosed 53-white/61-black mask")
    return white, black


def read_events(path):
    data = path.read_bytes()
    partial = path.name.endswith(".partial")
    lines = data.splitlines(keepends=True)
    ignored_tail_bytes = 0
    if partial and lines and not lines[-1].endswith(b"\n"):
        ignored_tail_bytes = len(lines.pop())
    rows = []
    lane = replay = None
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except (ValueError, UnicodeDecodeError) as exc:
            raise ValueError(f"invalid completed event line {line_number}: {exc}") from exc
        if not isinstance(row, dict) or row.get("schema") != "dm-gta-frame/1" or row.get("mock") is not False:
            raise ValueError(f"line {line_number} is not a real dm-gta-frame/1 event")
        frame = row.get("output_frame")
        if type(frame) is not int or frame < 0 or row.get("render_succeeded") is not True:
            raise ValueError(f"line {line_number} lacks a successful output frame")
        if rows and frame != rows[-1]["output_frame"] + 1:
            raise ValueError(f"non-contiguous output frames at line {line_number}")
        if not rows:
            lane, replay = row.get("lane"), row.get("replay_sha256")
            if type(lane) is not int or lane not in (1, 2, 3, 4):
                raise ValueError("invalid lane identity")
            if not isinstance(replay, str) or len(replay) != 64 or any(c not in "0123456789abcdef" for c in replay):
                raise ValueError("invalid replay SHA-256 identity")
        if row.get("lane") != lane or row.get("replay_sha256") != replay:
            raise ValueError(f"mixed lane/replay identities at line {line_number}")
        rows.append(row)
    if not rows:
        raise ValueError("no complete real frame events in this snapshot")
    return rows, {
        "event_file": str(path.resolve()), "event_snapshot_bytes": len(data),
        "event_snapshot_sha256": sha256(data), "partial_snapshot": partial,
        "ignored_unflushed_tail_bytes": ignored_tail_bytes,
        "available_frame_count": len(rows), "lane": lane, "replay_sha256": replay,
        "available_first_frame": rows[0]["output_frame"],
        "available_last_frame": rows[-1]["output_frame"],
    }


def choose_rows(rows, args):
    if args.all:
        return rows, "all_complete_events_in_snapshot"
    if args.frames is not None:
        requested = sorted(set(int(value) for value in args.frames.split(",")))
        by_frame = {row["output_frame"]: row for row in rows}
        missing = [frame for frame in requested if frame not in by_frame]
        if missing:
            raise ValueError(f"requested frames unavailable in this snapshot: {missing}")
        return [by_frame[frame] for frame in requested], "explicit_output_frames"
    count = min(args.samples, len(rows))
    indices = ([0] if count == 1 else
               sorted({round(index * (len(rows) - 1) / (count - 1)) for index in range(count)}))
    return [rows[index] for index in indices], "evenly_spaced_samples_in_snapshot"


def inspect_frame(row, masks):
    path = Path(row["output_path"])
    if not path.is_absolute():
        raise ValueError("sealed PNG output_path must be absolute")
    data = path.read_bytes()
    if len(data) != row.get("output_bytes") or sha256(data) != row.get("output_sha256"):
        raise ValueError(f"PNG byte length/SHA-256 mismatch: frame {row['output_frame']}")
    image = png_rgb(data, f"frame {row['output_frame']}")
    white, black = masks
    wm = sum(all(c >= 230 for c in image.getpixel((x, y))) for x, y, _ in white)
    bm = sum(all(c <= 40 for c in image.getpixel((x, y))) for x, y, _ in black)
    exact = sum(image.getpixel((x, y)) == color for x, y, color in white + black)
    return {
        "output_frame": row["output_frame"], "replay_time_ms": row.get("replay_time_ms"),
        "output_path": str(path.resolve()), "output_sha256": row["output_sha256"],
        "png_sha256_verified": True,
        "white_matching_pixels": wm, "white_mask_pixels": WHITE_PIXELS,
        "black_matching_pixels": bm, "black_mask_pixels": BLACK_PIXELS,
        "white_matching_fraction": wm / WHITE_PIXELS,
        "black_matching_fraction": bm / BLACK_PIXELS,
        "exact_reference_rgb_pixels": exact, "reference_mask_pixels": WHITE_PIXELS + BLACK_PIXELS,
        "known_template_full_threshold_match": wm == WHITE_PIXELS and bm == BLACK_PIXELS,
        "known_cursor_candidate": wm / WHITE_PIXELS > 0.95 and bm / BLACK_PIXELS > 0.95,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("frame_events", type=Path, nargs="+",
                        help="real frame-events-laneN.jsonl or .jsonl.partial snapshots")
    parser.add_argument("--template-png", type=Path, default=TEMPLATE_DEFAULT,
                        help="the same SHA-pinned real PNG, if its copy was relocated")
    parser.add_argument("--out", type=Path, help="write a new JSON report; refuse overwrite")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--all", action="store_true")
    selection.add_argument("--frames", help="comma-separated output-frame numbers")
    selection.add_argument("--samples", type=int, default=16)
    args = parser.parse_args(argv)
    if args.samples < 1:
        parser.error("--samples must be at least 1")
    if args.out and args.out.exists():
        parser.error("--out already exists; preserve the previous report")
    report = {
        "schema": "dm-gta-known-cursor-scan/2", "read_only_inputs": True,
        "scope": "fixed_location_known_template_only", "limitation": LIMITATION,
        "whole_image_cursor_free_verified": False, "delivery_accepted": False,
        "human_review_approval": False,
        "template": {
            "path": str(args.template_png.resolve()), "sha256": TEMPLATE_SHA256,
            "roi_xyxy_right_bottom_exclusive": TEMPLATE_ROI,
            "white_mask_pixels": WHITE_PIXELS, "black_mask_pixels": BLACK_PIXELS,
            "mask_criteria": "template white RGB>=242; black RGB<=24, all channels",
            "comparison_criteria": "target white RGB>=230; black RGB<=40, all channels",
            "candidate_criteria": "white fraction>0.95 AND black fraction>0.95",
        },
        "files": [], "errors": [],
    }
    try:
        masks = load_template(args.template_png)
        for path in args.frame_events:
            try:
                rows, file_report = read_events(path)
                selected, selection_name = choose_rows(rows, args)
                file_report.update(selection=selection_name, checked_frames=[], errors=[])
                report["files"].append(file_report)
                for row in selected:
                    try:
                        file_report["checked_frames"].append(inspect_frame(row, masks))
                    except (OSError, ValueError, KeyError, TypeError) as exc:
                        error = f"frame {row.get('output_frame')}: {exc}"
                        file_report["errors"].append(error)
                        report["errors"].append(f"{path}: {error}")
                file_report["checked_frame_count"] = len(file_report["checked_frames"])
                file_report["candidate_frames"] = [x["output_frame"] for x in file_report["checked_frames"] if x["known_cursor_candidate"]]
                file_report["full_threshold_match_frames"] = [x["output_frame"] for x in file_report["checked_frames"] if x["known_template_full_threshold_match"]]
            except (OSError, ValueError, KeyError, TypeError) as exc:
                report["errors"].append(f"{path}: {exc}")
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    found = any(x.get("candidate_frames") for x in report["files"])
    report["known_cursor_candidate_in_checked_frames"] = found
    report["status"] = ("ERROR" if report["errors"] else "KNOWN_CURSOR_CANDIDATE" if found
                        else "NO_KNOWN_TEMPLATE_CANDIDATE_IN_CHECKED_FRAMES")
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("x", encoding="utf-8", newline="\n") as stream:
            json.dump(report, stream, ensure_ascii=False, indent=2)
            stream.write("\n")
        print(json.dumps({"report": str(args.out.resolve()), "status": report["status"],
                          "checked_frames": sum(x["checked_frame_count"] for x in report["files"]),
                          "errors": report["errors"]}, ensure_ascii=False))
    else:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    return 2 if report["errors"] else 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
