#!/usr/bin/env python3
"""Read-only Present boundary attribution; never a certification/quality verdict."""
import argparse
from collections import Counter
import json
from pathlib import Path


def positive(value):
    return type(value) is int and value > 0


def decompose(left, right, frequency, schema=None):
    """Partition the existing anchor-to-anchor interval, not a new FPS clock."""
    a, b = left.get("present_call"), right.get("present_call")
    if not isinstance(a, dict) or not isinstance(b, dict):
        return {"available": False, "reason": "present_call_not_recorded"}
    if right["seq"] != left["seq"] + 1:
        return {"available": False, "reason": "nonconsecutive_present"}
    if not all(positive(call.get("thread_id")) for call in (a, b)):
        return {"available": False, "reason": "invalid_thread_identity"}
    v6 = schema == "dm-perf-frames-v6" or (schema is None and "epoch" in a)
    if v6:
        if any(not positive(a.get(key)) or a[key] != b.get(key)
               for key in ("epoch", "generation", "swapchain_key")):
            return {"available": False, "reason": "epoch_generation_or_swapchain_mismatch"}
        previous = a.get("exit")  # exact left-row slot, NOT the current thread's TLS
        if not isinstance(previous, dict):
            return {"available": False, "reason": "exit_not_recorded"}
    else:
        previous = b.get("previous")
        if (not isinstance(previous, dict) or previous.get("seq") != left["seq"]
                or a["thread_id"] != b.get("thread_id")):
            return {"available": False, "reason": "no_same_thread_consecutive_exit_witness"}
    boundaries = [
        left["qpc"], a.get("original_begin_qpc"), a.get("original_end_qpc"),
        a.get("commit_begin_qpc"), a.get("commit_lock_acquired_qpc"),
        a.get("commit_recorded_qpc"), previous.get("commit_return_qpc"),
        previous.get("hook_exit_qpc"), b.get("entry_qpc"),
        b.get("entry_lock_acquired_qpc"), right["qpc"],
    ]
    if not all(positive(qpc) for qpc in boundaries):
        return {"available": False, "reason": "missing_or_nonmonotonic_boundary"}
    if any(y < x for x, y in zip(boundaries[:8], boundaries[1:8])) or any(
            y < x for x, y in zip(boundaries[8:], boundaries[9:])):
        return {"available": False, "reason": "missing_or_nonmonotonic_boundary"}
    if boundaries[7] > boundaries[8]:
        return {"available": False, "reason": "overlapping_present_calls"}
    labels = (
        "probe_pre_original", "original_present", "probe_post_original",
        "commit_prepare_and_lock_wait", "commit_locked", "commit_unlock_and_return",
        "hook_tail", "between_hooks", "next_entry_admission_and_lock_wait",
        "next_anchor_after_lock",
    )
    parts = {name: (y - x) * 1000 / frequency
             for name, x, y in zip(labels, boundaries, boundaries[1:])}
    return {"available": True, "cross_thread": a["thread_id"] != b["thread_id"], "parts_ms": parts,
            "largest_interval": max(parts, key=parts.get),
            "sum_ms": sum(parts.values())}


def call_parts(row, frequency):
    """Independent per-call evidence remains useful when two Presents overlap."""
    call = row.get("present_call")
    if not isinstance(call, dict):
        return None
    points = [call.get("entry_qpc"), call.get("entry_lock_acquired_qpc"), row["qpc"],
              call.get("original_begin_qpc"), call.get("original_end_qpc"),
              call.get("commit_begin_qpc"), call.get("commit_lock_acquired_qpc"),
              call.get("commit_recorded_qpc")]
    labels = ["entry_admission_and_lock_wait", "anchor_after_lock", "probe_pre_original",
              "original_present", "probe_post_original", "commit_prepare_and_lock_wait", "commit_locked"]
    if not all(positive(qpc) for qpc in points) or any(y < x for x, y in zip(points, points[1:])):
        return None
    return {key: (y - x) * 1000 / frequency for key, x, y in zip(labels, points, points[1:])}


def select_ack_window(header, rows, ack):
    if ack.get("ack", {}).get("type") != "flushed":
        raise ValueError("expected a Flushed ACK")
    window = ack["ack"]["summary"].get("present_timing") or {}
    start, end = window.get("qpc_start"), window.get("qpc_end")
    if not positive(start) or not positive(end) or end <= start or window.get("qpc_frequency") != header["qpf"]:
        raise ValueError("ACK window/QPC frequency mismatch")
    selected = [row for row in rows if start <= row["qpc"] <= end]
    if (len(selected) != window.get("sample_count") or not selected
            or selected[0]["qpc"] != start or selected[-1]["qpc"] != end):
        raise ValueError("ACK window is not fully witnessed by this frame file")
    if header["schema"] == "dm-perf-frames-v6" and any(
            (row.get("present_call") or {}).get("generation", ack["identity"]["generation"])
            != ack["identity"]["generation"] for row in selected):
        raise ValueError("ACK generation mismatch")
    return selected


PREPARE_TIMES = ("total_ns", "outer_lock_wait_ns", "outer_lock_held_ns", "mapped_ranges_ns",
                 "validation_ns", "descriptor_wait_ns", "descriptor_lookup_ns", "memory_query_ns",
                 "allocation_ns", "memory_read_ns", "reuse_clone_ns", "residual_ns")
PREPARE_COUNTS = ("lists", "requests", "live_requests", "validation_rejected", "read_attempts",
                  "read_rejected", "reused_snapshots", "read_bytes", "reused_bytes",
                  "write_combine_reads", "no_cache_reads", "unflagged_reads", "byte_loads", "wide_loads")
QUERY_COUNTS = ("memory_query_calls", "memory_query_rejected", "query_cache_hits",
                "query_cache_ineligible_reads", "query_cache_inserts", "query_cache_full_reads")
PREFETCH_TIMES = ("query_prefetch_ns", "query_prefetch_query_ns")
PREFETCH_COUNTS = ("query_prefetch_requests", "query_prefetch_calls", "query_prefetch_rejected",
                   "query_prefetch_cache_hits", "query_prefetch_inserts", "query_prefetch_full",
                   "query_prefetch_read_hits")
SLOW_SCHEMAS = tuple(f"dm-perf-slow-hooks-v{version}" for version in range(1, 6))
COPY_TIMES = ("total_ns", "prepare_ns", "wait_ns", "apply_ns", "residual_ns", "max_wait_ns")
COPY_COUNTS = ("descriptors", "shard_count", "source_visits", "destination_visits",
               "max_wait_group_descriptors")


def validate_descriptor_detail(header, event):
    detail = event.get("descriptor_copy")
    if detail is None:  # Early invalid/unsupported copy plans may have no detail.
        return
    if (header["schema"] != "dm-perf-slow-hooks-v5" or event["kind"] != "descriptor_copy"
            or event.get("execute_prepare") is not None or not isinstance(detail, dict)
            or type(detail.get("completed")) is not bool
            or any(type(detail.get(key)) is not int or detail[key] < 0
                   for key in COPY_TIMES + COPY_COUNTS)):
        raise ValueError("invalid descriptor-copy detail")
    visits = detail["source_visits"] + detail["destination_visits"]
    count, shards = detail["descriptors"], detail["shard_count"]
    if (detail["total_ns"] != event["duration_ns"]
            or sum(detail[key] for key in ("prepare_ns", "wait_ns", "apply_ns", "residual_ns")) != detail["total_ns"]
            or detail["max_wait_ns"] > detail["wait_ns"] or count > 4096 or shards > min(64, 2 * count)
            or any(detail[key] > min(shards, count) for key in ("source_visits", "destination_visits"))
            or (detail["completed"] and ((count == 0) != (shards == 0)
                or shards > visits or (count > 0 and (not detail["source_visits"] or not detail["destination_visits"]))))):
        raise ValueError("inconsistent descriptor-copy counts/times")
    if visits:
        if (type(detail.get("max_wait_shard")) is not int or not 0 <= detail["max_wait_shard"] < 64
                or detail.get("max_wait_phase") not in ("source", "destination")
                or not 1 <= detail["max_wait_group_descriptors"] <= count
                or not detail[f'{detail["max_wait_phase"]}_visits']):
            raise ValueError("invalid descriptor-copy slowest acquisition")
    elif (detail.get("max_wait_shard") is not None or detail.get("max_wait_phase") is not None
          or detail["max_wait_group_descriptors"] or detail["max_wait_ns"] or detail["wait_ns"] or detail["apply_ns"]):
        raise ValueError("descriptor-copy acquisition without a visit")


def validate_prepare_detail(header, event):
    detail = event.get("execute_prepare")
    if detail is None:
        return
    v4 = header["schema"] in SLOW_SCHEMAS[3:]
    v3 = header["schema"] in SLOW_SCHEMAS[2:]
    if (header["schema"] not in SLOW_SCHEMAS[1:] or event["kind"] != "execute_prepare"
            or not isinstance(detail, dict)
            or any(type(detail.get(key)) is not int or detail[key] < 0
                   for key in PREPARE_TIMES + PREPARE_COUNTS + (QUERY_COUNTS if v3 else ())
                   + (PREFETCH_TIMES + PREFETCH_COUNTS if v4 else ()))):
        raise ValueError("invalid execute-prepare detail")
    exclusive = ("outer_lock_wait_ns", "mapped_ranges_ns", "validation_ns", "memory_query_ns",
                 "allocation_ns", "memory_read_ns", "reuse_clone_ns", "residual_ns")
    if v4:
        exclusive += ("query_prefetch_ns",)
    if (detail["total_ns"] != event["duration_ns"]
            or sum(detail[key] for key in exclusive) != detail["total_ns"]
            or detail["descriptor_wait_ns"] + detail["descriptor_lookup_ns"] > detail["validation_ns"]
            or detail["requests"] != detail["live_requests"] + detail["validation_rejected"]
            or detail["live_requests"] != detail["read_attempts"] + detail["reused_snapshots"]
            or detail["read_rejected"] > detail["read_attempts"]
            or detail["byte_loads"] + 8 * detail["wide_loads"] != detail["read_bytes"]):
        raise ValueError("inconsistent execute-prepare detail")
    if v3:
        calls, rejected, hits, ineligible, inserts, full = (detail[key] for key in QUERY_COUNTS)
        successful_reads = sum(detail[key] for key in
                               ("write_combine_reads", "no_cache_reads", "unflagged_reads"))
        if (calls + hits > detail["read_attempts"] or rejected > calls
                or rejected > detail["read_rejected"] or ineligible > calls
                or inserts + full > calls - rejected or inserts + full > calls - ineligible
                or calls - rejected + hits != successful_reads
                or successful_reads != detail["read_attempts"] - detail["read_rejected"]):
            raise ValueError("inconsistent execute-prepare query counts")
    if v4:
        requests, calls, rejected, hits, inserts, full, read_hits = (detail[key] for key in PREFETCH_COUNTS)
        if (calls + hits > requests or rejected > calls or inserts + full > calls - rejected
                or read_hits > detail["query_cache_hits"] or (read_hits and not inserts)
                or detail["query_prefetch_query_ns"] > detail["query_prefetch_ns"]
                or detail["outer_lock_held_ns"] + detail["outer_lock_wait_ns"] > detail["total_ns"]):
            raise ValueError("inconsistent execute-prepare prefetch counts/times")


def slow_hook_overlaps(header, events, frequency, interval):
    matches = []
    for event in events:
        end = event["observed_end_qpc"]
        # The exact call end lies before this QPC, by up to the reported
        # alignment uncertainty. Use conservative possible bounds, not fake precision.
        span_ns = event["duration_ns"] + event["alignment_uncertainty_ns"]
        start = end - (span_ns * frequency + 999_999_999) // 1_000_000_000
        if start < interval["end_qpc"] and end > interval["start_qpc"]:
            match = {**event, "duration_ms": event["duration_ns"] / 1_000_000,
                     "possible_start_qpc": start}
            if event.get("execute_prepare") is not None:
                match["prepare_times_ms"] = {key.removesuffix("_ns"): event["execute_prepare"][key] / 1e6
                                             for key in PREPARE_TIMES + PREFETCH_TIMES if key in event["execute_prepare"]}
                match["prepare_note"] = "descriptor wait/lookup are subsets of validation; prefetch_query is a subset of unlocked prefetch; outer_lock_held sums locked stages, not prefetch; wall time, not CPU"
            if event.get("descriptor_copy") is not None:
                match["copy_times_ms"] = {key.removesuffix("_ns"): event["descriptor_copy"][key] / 1e6
                                         for key in COPY_TIMES}
                match["copy_note"] = "wait sums acquisitions; max_wait identifies one visit, not an owner; includes scheduling; apply excludes guard release; residual includes bookkeeping"
            if event["kind"] == "draw_redundancy_check":
                match["check_note"] = "whole redundancy predicate before selected_draw timing; includes PSO/state/generation checks, NOT pure descriptor wait; no all-call cumulative timing"
            matches.append(match)
    matches.sort(key=lambda event: event["duration_ns"], reverse=True)
    return {"possible_overlap_count": len(matches),
            "by_kind": dict(Counter(event["kind"] for event in matches)),
            "largest_events": matches[:20],
            "note": "approximate temporal overlap, NOT causal attribution; nested/concurrent durations must not be summed"}


def summarize(header, rows, top=10, ack=None, slow_hooks=None):
    if header.get("schema") not in ("dm-perf-frames-v4", "dm-perf-frames-v5", "dm-perf-frames-v6"):
        raise ValueError("expected dm-perf-frames-v4/v5/v6")
    frequency = header.get("qpf")
    if type(frequency) is not int or frequency <= 0:
        raise ValueError("invalid QPC frequency")
    if not 1 <= top <= 50:
        raise ValueError("top must be in 1..50")
    if len(rows) != header.get("record_count") or len(rows) > 131_072:
        raise ValueError("frame record count mismatch or budget exceeded")
    for row in rows:
        if (not positive(row.get("qpc")) or type(row.get("seq")) is not int or row["seq"] < 0):
            raise ValueError("missing frame identity/QPC")
    source_count = len(rows)
    if ack is not None:
        rows = select_ack_window(header, rows, ack)
    if slow_hooks is not None:
        sh, events = slow_hooks
        if (sh.get("schema") not in SLOW_SCHEMAS or sh.get("qpf") != frequency
                or sh.get("record_count") != len(events) or len(events) > 8192
                or not positive(sh.get("epoch"))):
            raise ValueError("invalid slow-hook header/frequency/count")
        if any((row.get("present_call") or {}).get("epoch", sh["epoch"]) != sh["epoch"] for row in rows):
            raise ValueError("slow-hook epoch mismatch")
        for event in events:
            if (not positive(event.get("observed_end_qpc")) or not positive(event.get("thread_id"))
                    or not positive(event.get("duration_ns"))
                    or type(event.get("alignment_uncertainty_ns")) is not int
                    or event["alignment_uncertainty_ns"] < 0
                    or event.get("kind") not in ("descriptor_copy", "execute_prepare", "execute_original",
                                                 "execute_complete", "execute_hook", "selected_draw",
                                                 "selected_draw_lock_wait", "selected_draw_locked",
                                                 "execute_query_prefetch", "execute_prepare_locked", "draw_redundancy_check")
                    or (event["kind"] == "draw_redundancy_check" and sh["schema"] != SLOW_SCHEMAS[-1])):
                raise ValueError("invalid slow-hook event")
            validate_prepare_detail(sh, event)
            validate_descriptor_detail(sh, event)
    intervals = []
    explained = 0
    reasons = Counter()
    cross_thread = 0
    call_samples = [parts for row in rows if (parts := call_parts(row, frequency)) is not None]
    for left, right in zip(rows, rows[1:]):
        if right["qpc"] <= left["qpc"]:
            detail = {"available": False, "reason": "nonincreasing_anchor_order"}
        else:
            detail = decompose(left, right, frequency, header["schema"])
        explained += int(detail["available"])
        cross_thread += int(detail.get("cross_thread", False))
        if not detail["available"]:
            reasons[detail["reason"]] += 1
        intervals.append({
            "start_seq": left["seq"], "end_seq": right["seq"],
            "start_qpc": left["qpc"], "end_qpc": right["qpc"],
            "duration_ms": (right["qpc"] - left["qpc"]) * 1000 / frequency,
            "diagnostic": detail,
        })
    intervals.sort(key=lambda row: row["duration_ms"], reverse=True)
    slowest = intervals[:top]
    if slow_hooks is not None:
        for interval in slowest:
            interval["slow_hooks"] = slow_hook_overlaps(*slow_hooks, frequency, interval)
    distributions = {}
    if call_samples:
        for key in call_samples[0]:
            values = sorted(sample[key] for sample in call_samples)
            distributions[key] = {"p50": values[(len(values) - 1) * 50 // 100],
                                  "p99": values[(len(values) * 99 + 99) // 100 - 1], "max": values[-1]}
    return {
        "schema": "dm-present-timing-readout/v2", "acceptance": False,
        "scope": ("exact ACK sample window" if ack else "retained Present anchors including settle")
                 + "; diagnostic wall time, not GPU or thread CPU time",
        "between_hooks_note": "includes game/other hooks/scheduling and the small return epilogue; not proof of game-only CPU cost",
        "source_schema": header["schema"], "qpf": frequency,
        "source_wrapped": header.get("wrapped"), "source_record_count": source_count,
        "record_count": len(rows), "interval_count": len(intervals),
        "decomposed_cross_thread_intervals": cross_thread,
        "decomposed_intervals": explained, "unavailable_intervals": len(intervals) - explained,
        "decomposition_coverage_percent": 100 * explained / len(intervals) if intervals else None,
        "unavailable_reasons": dict(reasons), "valid_call_boundary_records": len(call_samples),
        "call_parts_ms": distributions,
        "own_exit_records": sum(isinstance((row.get("present_call") or {}).get("exit"), dict) for row in rows),
        "slow_hook_evidence": slow_hooks[0] if slow_hooks else {"available": False},
        "slowest_intervals": slowest,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("perf_frames", type=Path)
    parser.add_argument("--top", type=int, default=10)
    parser.add_argument("--flushed-ack", type=Path, help="Restrict to the exact native ACK sample window")
    args = parser.parse_args()
    if args.perf_frames.stat().st_size > 128 * 1024 * 1024:
        parser.error("perf file exceeds 128 MiB diagnostic budget")
    with args.perf_frames.open(encoding="utf-8") as source:
        header = json.loads(next(source))
        rows = []
        for line in source:
            if len(rows) >= 131_072:
                parser.error("perf file exceeds bounded frame capacity")
            rows.append(json.loads(line))
    ack = None
    if args.flushed_ack:
        if args.flushed_ack.stat().st_size > 4 * 1024 * 1024:
            parser.error("ACK exceeds 4 MiB")
        ack = json.loads(args.flushed_ack.read_text(encoding="utf-8"))
    slow_hooks = None
    slow_path = args.perf_frames.with_name("perf-slow-hooks.jsonl")
    if slow_path.exists():
        # v2..v5 add fixed-size per-call details; 8192 remains the record cap.
        if slow_path.stat().st_size > 16 * 1024 * 1024:
            parser.error("slow-hook evidence exceeds 16 MiB")
        with slow_path.open(encoding="utf-8") as source:
            sh = json.loads(next(source))
            events = []
            for line in source:
                if len(events) >= 8192:
                    parser.error("slow-hook record budget exceeded")
                events.append(json.loads(line))
            slow_hooks = (sh, events)
    print(json.dumps(summarize(header, rows, args.top, ack, slow_hooks), indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
