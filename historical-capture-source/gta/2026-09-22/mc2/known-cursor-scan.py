"""Read-only detection of one known cursor template at one fixed pixel location.

This is NOT a whole-image cursor/HUD detector or a delivery acceptance check.
Exit 0: no template candidate in checked samples; 1: candidate found; 2: error.
Only stdout is written. Source events, PNGs, and approval flags are never changed.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import sys

from PIL import Image


TEMPLATE_SHA256 = "59247eaf872f96dd1c6213493987e8a8b91f133b78e266db123b4f1a284fed23"
TEMPLATE_DEFAULT = Path("C:/capture/gta/replus-output/render_0042/frame_000805.png")
TEMPLATE_ROI = (1840, 1040, 1880, 1080)
TEMPLATE_BOUNDS = (1848, 1049, 1864, 1074)
IMAGE_SIZE = (1920, 1080)
CANDIDATE_MIN_PIXELS = 95
LIMITATION = (
    "Only the 100-pixel RUN002 lane1 frame805 cursor template at its original "
    "location is checked. 100/100 is an exact match; 95-99/100 is a heuristic "
    "candidate requiring visual review. No candidate does not establish a cursor-free image: moved, "
    "different, scaled, or partly matching cursors and other HUD pixels are outside "
    "this check. Unsampled frames are not checked. Inspect original PNGs manually, "
    "including the new pointer location after moving the mouse to a title bar."
)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def load_template(path):
    data = path.read_bytes()
    if sha256(data) != TEMPLATE_SHA256:
        raise ValueError("template PNG does not match the pinned RUN002 frame805 SHA-256")
    with Image.open(io.BytesIO(data)) as source:
        if source.size != IMAGE_SIZE or source.format != "PNG":
            raise ValueError("template is not the expected 1920x1080 PNG")
        image = source.convert("RGB")
        x0, y0, x1, y1 = TEMPLATE_ROI
        mask = [(x, y, image.getpixel((x, y)))
                for y in range(y0, y1) for x in range(x0, x1)
                if len(set(image.getpixel((x, y)))) == 1]
    bounds = (min(x for x, y, c in mask), min(y for x, y, c in mask),
              max(x for x, y, c in mask), max(y for x, y, c in mask))
    if len(mask) != 100 or bounds != TEMPLATE_BOUNDS:
        raise ValueError("template mask differs from the diagnosed 100-pixel pattern")
    return mask


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
        if row.get("schema") != "dm-gta-frame/1" or row.get("mock") is not False:
            raise ValueError(f"line {line_number} is not a real dm-gta-frame/1 event")
        frame = row.get("output_frame")
        if type(frame) is not int or frame < 0 or row.get("render_succeeded") is not True:
            raise ValueError(f"line {line_number} lacks a successful output frame")
        if rows and frame != rows[-1]["output_frame"] + 1:
            raise ValueError(f"non-contiguous output frames at line {line_number}")
        if not rows:
            lane, replay = row.get("lane"), row.get("replay_sha256")
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
            raise ValueError(f"requested frames are unavailable in this snapshot: {missing}")
        return [by_frame[frame] for frame in requested], "explicit_output_frames"
    count = min(args.samples, len(rows))
    indices = ([0] if count == 1 else
               sorted({round(index * (len(rows) - 1) / (count - 1))
                       for index in range(count)}))
    return [rows[index] for index in indices], "evenly_spaced_samples_in_snapshot"


def inspect_frame(row, mask, event_dir):
    path = Path(row["output_path"])
    if not path.is_absolute():
        path = event_dir / path
    data = path.read_bytes()
    if len(data) != row.get("output_bytes") or sha256(data) != row.get("output_sha256"):
        raise ValueError(f"PNG byte length/SHA-256 mismatch: frame {row['output_frame']}")
    with Image.open(io.BytesIO(data)) as source:
        if source.size != IMAGE_SIZE or source.format != "PNG":
            raise ValueError(f"frame {row['output_frame']} is not a 1920x1080 PNG")
        image = source.convert("RGB")
        matches = sum(image.getpixel((x, y)) == color for x, y, color in mask)
    return {
        "output_frame": row["output_frame"], "replay_time_ms": row.get("replay_time_ms"),
        "output_path": str(path.resolve()), "output_sha256": row["output_sha256"],
        "png_sha256_verified": True, "matching_template_pixels": matches,
        "template_pixels": len(mask), "known_cursor_match": matches == len(mask),
        "known_cursor_candidate": matches >= CANDIDATE_MIN_PIXELS,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("frame_events", type=Path, nargs="+",
                        help="real frame-events-laneN.jsonl or .jsonl.partial files")
    parser.add_argument("--template-png", type=Path, default=TEMPLATE_DEFAULT,
                        help="path to the exact pinned frame805 PNG, if relocated")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--all", action="store_true",
                           help="read and SHA-verify every complete event's PNG (may be slow)")
    selection.add_argument("--samples", type=int, default=16,
                           help="evenly spaced samples from available frames (default: 16)")
    selection.add_argument("--frames", help="explicit output-frame numbers, comma-separated")
    args = parser.parse_args(argv)
    if args.samples < 1:
        parser.error("--samples must be at least 1")
    report = {
        "schema": "dm-gta-known-cursor-scan/1", "read_only": True,
        "scope": "fixed_location_template_only", "limitation": LIMITATION,
        "whole_image_cursor_free_verified": False, "delivery_accepted": False,
        "human_review_approval": False,
        "template": {"path": str(args.template_png.resolve()),
                     "sha256": TEMPLATE_SHA256, "mask_pixels": 100,
                     "heuristic_candidate_min_matching_pixels": CANDIDATE_MIN_PIXELS,
                     "inclusive_mask_bounds_xyxy": TEMPLATE_BOUNDS},
        "files": [], "errors": [],
    }
    try:
        mask = load_template(args.template_png)
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 2
    any_match = False
    any_candidate = False
    for path in args.frame_events:
        try:
            rows, file_report = read_events(path)
            selected, selection_name = choose_rows(rows, args)
            file_report.update(selection=selection_name, checked_frames=[], errors=[])
            report["files"].append(file_report)
            for row in selected:
                try:
                    result = inspect_frame(row, mask, path.parent)
                    file_report["checked_frames"].append(result)
                    any_match |= result["known_cursor_match"]
                    any_candidate |= result["known_cursor_candidate"]
                except (OSError, ValueError, KeyError) as exc:
                    error = f"frame {row.get('output_frame')}: {exc}"
                    file_report["errors"].append(error)
                    report["errors"].append(f"{path}: {error}")
            file_report["checked_frame_count"] = len(file_report["checked_frames"])
            file_report["exact_match_frames"] = [
                result["output_frame"] for result in file_report["checked_frames"]
                if result["known_cursor_match"]]
            file_report["candidate_frames"] = [
                result["output_frame"] for result in file_report["checked_frames"]
                if result["known_cursor_candidate"]]
        except (OSError, ValueError, KeyError) as exc:
            report["errors"].append(f"{path}: {exc}")
    report["known_cursor_found_in_checked_samples"] = any_match
    report["known_cursor_candidate_in_checked_samples"] = any_candidate
    report["status"] = ("ERROR" if report["errors"] else
                        "KNOWN_CURSOR_FOUND" if any_match else
                        "KNOWN_CURSOR_CANDIDATE" if any_candidate else
                        "NO_TEMPLATE_CANDIDATE_IN_CHECKED_SAMPLES")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 2 if report["errors"] else 1 if any_candidate else 0


if __name__ == "__main__":
    sys.exit(main())
