#!/usr/bin/env python3
"""Read-only CP2077 performance counters; never a capture or quality verdict."""
import argparse
import hashlib
import json
from pathlib import Path

PERFORMANCE_SCHEMA = "dm-cp2077-raw-performance/v1"
PHASES = ("source_armed", "selected_arm", "selected_seal")
HOOK_TIMINGS = (
    "selected_draw", "selected_draw_lock_wait", "selected_draw_locked",
    "execute_prepare", "execute_complete", "buffer_copy_provenance",
    "descriptor_copy", "descriptor_copy_prepare", "descriptor_copy_lock_wait",
    "descriptor_copy_apply", "descriptor_write", "descriptor_invalidate",
    "resource_map", "resource_unmap", "camera_snapshot", "camera_snapshot_lock_wait",
    "camera_snapshot_copy", "camera_snapshot_admission", "camera_present_summary",
    "camera_present_lock_wait", "camera_scissor_observation", "observation_snapshot",
)
FEEDBACK_TIMINGS = (
    "advance", "gpu_initialization", "gpu_ready_and_snapshots", "local_camera",
    "candidate_materialize", "readback_drain_and_submit", "select_readback_submit",
)


def unsigned(value):
    return type(value) is int and 0 <= value <= 2**64 - 1


def counter(value):
    if value is None:
        return None
    if not isinstance(value, dict) or not all(unsigned(value.get(k)) for k in (
            "calls", "total_us", "max_us")):
        raise ValueError("invalid timing counter")
    return value


def difference(start, end):
    start, end = counter(start), counter(end)
    if start is None or end is None:
        return {"available": False, "reason": "counter_not_recorded", "mean_us": None}
    calls, total = end["calls"] - start["calls"], end["total_us"] - start["total_us"]
    if calls < 0 or total < 0 or end["max_us"] < start["max_us"]:
        return {"available": False, "reason": "counter_regression", "mean_us": None}
    if calls == 0 and total != 0:
        return {"available": False, "reason": "non_transactional_counter_delta", "mean_us": None}
    return {"available": True, "calls": calls, "total_us": total,
            "mean_us": total / calls if calls else None,
            "window_max_us": None, "cumulative_max_us_at_end": end["max_us"]}


def table(start, end, keys):
    return {key: difference(start.get(key), end.get(key)) for key in keys}


def summarize(summary, camera=None):
    if (summary.get("schema") not in ("dm-cp2077-raw-selected-summary/v1",
                                      "dm-cp2077-raw-selected-summary/v2")
            or summary.get("non_trainable") is not True):
        raise ValueError("expected a non-trainable raw selected summary")
    result = {
        "schema": "dm-cp2077-arm-performance-readout/v1", "non_trainable": True,
        "acceptance": False, "actual_game_fps_computed": False,
        "process_launch_id": summary.get("process_launch_id"),
        "source_fault": summary.get("fault"), "source_sealed": summary.get("sealed"),
        "scope": "wall elapsed; nested/concurrent counters are not additive; not GPU or thread CPU time",
        "phase_note": "protocol checkpoints, not exact RGB windows; crossing calls and relaxed counters affect boundaries",
        "windows": {}, "hook_lifetime": None, "gpuva_ownership_lifetime": None,
        "gpu_allocation": summary.get("gpu_allocation"),
    }
    perf = summary.get("performance")
    result["phase_measurements_available"] = perf is not None
    if perf is not None:
        if (perf.get("schema") != PERFORMANCE_SCHEMA or perf.get("non_trainable") is not True
                or perf.get("acceptance") is not False):
            raise ValueError("unknown performance schema or diagnostic flags")
        frequency = perf.get("qpc_frequency")
        if type(frequency) is not int or frequency <= 0:
            raise ValueError("invalid QPC frequency")
        checkpoints = perf["checkpoints"]
        result["measurement"] = perf.get("measurement")
        result["advance_excludes"] = perf.get("advance_excludes")
        for a, b in zip(PHASES, PHASES[1:]):
            start, end = checkpoints.get(a), checkpoints.get(b)
            name = f"{a}_to_{b}"
            if start is None or end is None:
                result["windows"][name] = {"available": False, "reason": "checkpoint_not_recorded"}
                continue
            if (not all(type(c.get("qpc")) is int and c["qpc"] > 0
                        and unsigned(c.get("present_seq")) for c in (start, end))
                    or end["qpc"] <= start["qpc"] or end["present_seq"] < start["present_seq"]):
                raise ValueError("invalid checkpoint order")
            result["windows"][name] = {
                "available": True, "checkpoint_seconds": (end["qpc"] - start["qpc"]) / frequency,
                "start_present": start["present_seq"], "end_present": end["present_seq"],
                "hook_timing": table(start["hook_timing"], end["hook_timing"], HOOK_TIMINGS),
                "feedback_timing": table(start["feedback_timing"], end["feedback_timing"], FEEDBACK_TIMINGS),
            }
        zero = {key: {"calls": 0, "total_us": 0, "max_us": 0} for key in FEEDBACK_TIMINGS}
        result["feedback_lifetime"] = table(zero, perf["feedback_timing_at_report"], FEEDBACK_TIMINGS)
    if camera is not None:
        if (camera.get("schema") not in ("dm-cp2077-render-camera-observation/v2",
                                         "dm-cp2077-render-camera-observation/v3")
                or camera.get("non_trainable") is not True
                or not summary.get("process_launch_id")
                or camera.get("process_launch_id") != summary["process_launch_id"]):
            raise ValueError("camera metadata schema/launch mismatch")
        observation = camera["observation"]
        zero = {key: {"calls": 0, "total_us": 0, "max_us": 0} for key in HOOK_TIMINGS}
        result["hook_lifetime"] = table(zero, observation.get("hook_cpu_timing", {}), HOOK_TIMINGS)
        result["hook_lifetime_scope"] = observation.get("hook_cpu_timing_scope")
        result["camera_scope"] = "metadata only; archive, draw identities and geometry not validated by this tool"
        names = ("gpuva_ownership_cache_hits", "gpuva_ownership_cache_misses",
                 "gpuva_ownership_resources_scanned", "gpuva_ownership_cache_invalidations",
                 "gpuva_ownership_cache_evictions")
        cache = {k: observation.get(k) for k in names}
        if any(v is not None and not unsigned(v) for v in cache.values()):
            raise ValueError("invalid ownership counter")
        hits, misses = cache[names[0]], cache[names[1]]
        cache["hit_fraction"] = (hits / (hits + misses)
                                 if hits is not None and misses is not None and hits + misses else None)
        result["gpuva_ownership_lifetime"] = cache
    return result


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", required=True, type=Path)
    parser.add_argument("--camera-report", type=Path, help="optional metadata only, no archive/geometry audit")
    parser.add_argument("--output", required=True, type=Path, help="new JSON outside source sessions")
    args = parser.parse_args()
    paths = [args.summary] + ([args.camera_report] if args.camera_report else [])
    for path in paths:
        if args.output.resolve().is_relative_to(path.resolve().parent):
            raise ValueError("output must be outside original input directories")
    values, hashes = [], {}
    for path, limit in zip(paths, [128 * 1024, 320 * 1024 * 1024]):
        if not path.is_file() or path.stat().st_size > limit:
            raise ValueError("input exceeds native reader bound or is not a file")
        hashes[str(path.resolve())] = digest(path)
        with path.open(encoding="utf-8-sig") as source:
            values.append(json.load(source))
        if (path == args.camera_report
                and values[-1].get("schema") == "dm-cp2077-render-camera-observation/v3"
                and path.stat().st_size > 64 * 1024 * 1024):
            raise ValueError("compact camera metadata exceeds native 64 MiB bound")
    result = summarize(*values)
    if any(digest(Path(path)) != expected for path, expected in hashes.items()):
        raise ValueError("input changed during readout")
    result["source_sha256"] = hashes
    with args.output.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(result, output, ensure_ascii=False, indent=2, allow_nan=False)
        output.write("\n")
    print(json.dumps({"output": str(args.output), "phase_measurements_available":
                      result["phase_measurements_available"], "acceptance": False}))


if __name__ == "__main__":
    main()
