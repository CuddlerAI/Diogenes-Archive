"""Read-only, bounded lane progress; never contacts GTA or certifies lane QC."""
import argparse
import hashlib
import json
from pathlib import Path
import time


def last_complete_event(path):
    # Only read a tail, and ignore any row the producer has not newline-flushed.
    with path.open("rb") as handle:
        handle.seek(0, 2)
        size = handle.tell()
        offset = max(0, size - 131072)
        handle.seek(offset)
        data = handle.read(131072)
    rows = data.split(b"\n")
    if offset:
        rows = rows[1:]
    if not data.endswith(b"\n"):
        rows = rows[:-1]
    for row in reversed(rows):
        if not row.strip():
            continue
        try:
            event = json.loads(row)
        except (ValueError, UnicodeDecodeError):
            continue
        if event.get("schema") == "dm-gta-frame/1":
            return event
    return None


def lane_status(run, lane, expected, verify_latest):
    sealed = run / "evidence" / f"frame-events-lane{lane}.jsonl"
    partial = sealed.with_suffix(sealed.suffix + ".partial")
    path = sealed if sealed.is_file() else partial
    result = {"lane": lane, "expected_frames": expected,
              "sealed_event_file_present": sealed.is_file(),
              "qc_verified": False}
    if not path.is_file():
        return dict(result, status="no_event_file", last_acknowledged_frame=None)
    try:
        stat = path.stat()
        event = last_complete_event(path)
    except FileNotFoundError:
        # A failure can remove .partial or a successful seal can rename it.
        return dict(result, status="event_file_changed_during_read")
    result.update(event_file=str(path), event_file_bytes=stat.st_size,
                  last_write_age_s=round(max(0, time.time() - stat.st_mtime), 1))
    if event is None:
        return dict(result, status="awaiting_first_complete_event")
    frame = event.get("output_frame")
    if event.get("lane") != lane or type(frame) is not int or frame < 0:
        return dict(result, status="invalid_tail_event")
    result.update(status="sealed_file_present" if sealed.is_file() else "partial",
                  last_acknowledged_frame=frame,
                  acknowledged_through_frame=frame + 1,
                  source_time_s=event.get("source_t_ns", 0) / 1e9,
                  replay_sha256=event.get("replay_sha256"),
                  latest_png=event.get("output_path"))
    if expected:
        result["progress_percent"] = round(100 * (frame + 1) / expected, 1)
    if verify_latest:
        png = Path(event["output_path"])
        digest = hashlib.sha256()
        total = 0
        with png.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1048576), b""):
                digest.update(chunk)
                total += len(chunk)
        result["latest_png_sha256_verified"] = (
            total == event.get("output_bytes")
            and digest.hexdigest() == event.get("output_sha256"))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--lane", type=int, choices=range(1, 5))
    parser.add_argument("--verify-latest", action="store_true",
                        help="also hash each lane's last acknowledged PNG")
    args = parser.parse_args()
    run = args.run_dir.resolve()
    record = run / "evidence" / "native-record-result.json"
    expected = None
    if record.is_file():
        expected = json.loads(record.read_text(encoding="utf-8-sig")).get(
            "master", {}).get("player_observation_count")
    lanes = [args.lane] if args.lane else range(1, 5)
    print(json.dumps({"read_only": True, "progress_only": True,
                      "note": "Tail indexes are progress observations, not a full continuity or QC check.",
                      "lanes": [lane_status(run, lane, expected, args.verify_latest)
                                for lane in lanes]}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
