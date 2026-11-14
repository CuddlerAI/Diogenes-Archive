#!/usr/bin/env python3
"""Offline content join of CP2077 dense (actor-root, FPP camera) pairs against rendered cameras.

Inputs come from ONE run directory (headers must carry the same stream identity):
  camera-pose-index.jsonl   rendered camera per Present, the ground truth for that frame
  player-pose-index.jsonl   formal per-Present actor-root sample (the current "late" read)
  cp2077-dense-pairs.jsonl  diagnostic dense pair entries with their observed QPC spans

The join key is the camera pose itself: a dense entry belongs to Present n when its memory
camera equals the rendered camera of n within a stated tolerance. No QPC-nearest pairing, no
interpolation, no fitted transform. The tolerance bounds the residual misalignment. Only
entries first seen no later than the Present QPC are eligible, matching what an online join
could know. Reports facts only; it changes no admission, coverage or delivery rule.

Torn pairs. One simulation step does not publish the actor root and the camera at the same
instant, so one camera content can be observed with two actor roots. Which of them belongs to
that camera depends on the engine's update order, so the script measures the order (transition
patterns) and scores every candidate rule against the intrinsic floor instead of assuming one.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path


def rows(path: Path) -> list[dict]:
    out = []
    with path.open(encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError as error:
                # A crashed run may leave a torn last line; anything else is a real fault.
                raise SystemExit(f"{path.name}:{number}: {error}")
    return out


def distance_mm(a: list[float], b: list[float]) -> float:
    return 1000.0 * math.dist(a, b)


def angle_deg(a: list[float], b: list[float]) -> float:
    """Angle between two unit quaternions; q and -q are the same rotation.

    Uses the chord length so that micro-degree residuals keep their precision (acos of a dot
    product near one does not).
    """
    norm_a = math.sqrt(sum(x * x for x in a))
    norm_b = math.sqrt(sum(x * x for x in b))
    if norm_a == 0.0 or norm_b == 0.0:
        return float("nan")
    ua = [x / norm_a for x in a]
    ub = [x / norm_b for x in b]
    sign = 1.0 if sum(x * y for x, y in zip(ua, ub)) >= 0.0 else -1.0
    chord = math.sqrt(sum((x - sign * y) ** 2 for x, y in zip(ua, ub)))
    return math.degrees(4.0 * math.asin(min(1.0, chord / 2.0)))


def percentiles(values: list[float]) -> dict:
    if not values:
        return {"count": 0}
    ordered = sorted(values)

    def at(fraction: float) -> float:
        return ordered[min(len(ordered) - 1, int(math.ceil(fraction * len(ordered))) - 1)]

    return {
        "count": len(ordered),
        "p50": at(0.50),
        "p95": at(0.95),
        "p99": at(0.99),
        "max": ordered[-1],
        "rms": math.sqrt(sum(v * v for v in ordered) / len(ordered)),
    }


def horizontal(delta: list[float], up_axis: int) -> list[float]:
    return [v for axis, v in enumerate(delta) if axis != up_axis]


def increment_error_mm(a1, a0, b1, b0, up_axis: int) -> float:
    da = horizontal([x - y for x, y in zip(a1, a0)], up_axis)
    db = horizontal([x - y for x, y in zip(b1, b0)], up_axis)
    return 1000.0 * math.dist(da, db)


RULES = ("earliest", "latest", "most_reads")
# v1: the diagnostic's own sampling thread. v2: entries observed by the production pair sampler.
VERSIONS = ("v1", "v2")


def is_schema(row: dict, kind: str) -> bool:
    return row.get("schema") in tuple(f"dm-cp2077-dense-pair{kind}/{version}" for version in VERSIONS)


def pick(group: list[dict], rule: str) -> dict:
    if rule == "earliest":
        return min(group, key=lambda e: e["first_begin_qpc"])
    if rule == "latest":
        return max(group, key=lambda e: e["first_begin_qpc"])
    return max(group, key=lambda e: (e["reads"], -e["first_begin_qpc"]))


def transition_patterns(entries: list[dict], frequency: int, contiguous_ms: float, moving_mm: float) -> dict:
    """How long an entry lived, grouped by which half of the pair changed to create it.

    A walking step alternates root-only and camera-only changes under EITHER update order, so
    counting patterns cannot tell the orders apart. Lifetimes can: the torn pair exists only
    between the two writes of one step, the consistent pair for the rest of the step. If the
    entries created by a root-only change are the short-lived ones, the engine writes the root
    first, (new root, old camera) is the torn pair, and the pair published WITH a camera
    content is the earliest entry carrying it. Short-lived camera-only entries mean the mirror
    image. Only uninterrupted stretches with a moving actor root are counted, so idle camera
    sway and truncated spans stay out.
    """
    limit = contiguous_ms * frequency / 1000.0
    spans = {"root_only_change": [], "camera_only_change": [], "both_changed": []}
    for before, middle, after in zip(entries, entries[1:], entries[2:]):
        if (
            middle["first_begin_qpc"] - before["last_end_qpc"] > limit
            or after["first_begin_qpc"] - middle["last_end_qpc"] > limit
            or distance_mm(before["player_position"], after["player_position"]) <= moving_mm
        ):
            continue
        root_changed = middle["player_words"] != before["player_words"]
        camera_changed = middle["camera_words"] != before["camera_words"]
        kind = (
            "both_changed"
            if root_changed and camera_changed
            else "root_only_change"
            if root_changed
            else "camera_only_change"
        )
        spans[kind].append(1000.0 * (middle["last_end_qpc"] - middle["first_begin_qpc"]) / frequency)
    return {
        "entry_observed_span_ms_by_how_it_was_created": {
            kind: percentiles(values) for kind, values in spans.items()
        },
        "reading": "the short-lived kind is the torn pair; root_only short => root written first => earliest rule",
    }


def analyze(
    camera_rows: list[dict],
    player_rows: list[dict],
    dense_rows: list[dict],
    position_tolerance_mm: float = 3.0,
    rotation_tolerance_deg: float = 2e-4,
    lookback_ms: float = 250.0,
    up_axis: int = 1,
    moving_mm: float = 5.0,
    contiguous_ms: float = 5.0,
) -> dict:
    header = next((r for r in dense_rows if is_schema(r, "-header")), None)
    if header is None:
        raise SystemExit("dense pair header missing")
    summary = next((r for r in dense_rows if is_schema(r, "-summary")), None)
    frequency = header["qpc_frequency"]
    identity = header["identity"]

    cameras = {}
    for row in camera_rows:
        if "present_seq" not in row:
            continue
        if row.get("identity") != identity:
            raise SystemExit("camera-pose-index identity differs from the dense pair header")
        cameras[row["present_seq"]] = row
    players = {r["present_seq"]: r for r in player_rows if "present_seq" in r}
    entries = [
        r
        for r in dense_rows
        if is_schema(r, "") and r.get("camera_position") is not None
    ]
    entries.sort(key=lambda r: r["first_begin_qpc"])
    lookback = int(lookback_ms * frequency / 1000.0)
    # The sampler's own gap histogram restarts after every failed read, so real sampling holes
    # are measured here, between the end of one entry and the start of the next.
    holes_ms = [
        1000.0 * (after["first_begin_qpc"] - before["last_end_qpc"]) / frequency
        for before, after in zip(entries, entries[1:])
    ]

    joined = {}
    groups = {}
    unmatched = []
    candidate_counts = []
    torn = []
    ages_ms = []
    span_ms = []
    residual_mm = []
    residual_deg = []
    spread_mm = []
    spread_deg = []
    for seq in sorted(cameras):
        camera = cameras[seq]
        qpc = camera["present_qpc"]
        eligible = [
            e
            for e in entries
            if e["first_begin_qpc"] <= qpc and e["last_end_qpc"] >= qpc - lookback
        ]
        scored = [
            (
                angle_deg(e["camera_rotation"], camera["camera_rotation_quaternion"]),
                distance_mm(e["camera_position"], camera["camera_position"]),
                e,
            )
            for e in eligible
        ]
        matches = [
            s for s in scored if s[0] <= rotation_tolerance_deg and s[1] <= position_tolerance_mm
        ]
        candidate_counts.append(len(matches))
        if not matches:
            nearest = min(scored, key=lambda s: (s[1], s[0]), default=None)
            unmatched.append(
                {
                    "present_seq": seq,
                    "eligible_entries": len(eligible),
                    "nearest_mm": None if nearest is None else nearest[1],
                    "nearest_deg": None if nearest is None else nearest[0],
                }
            )
            continue
        best = min(matches, key=lambda s: (s[0], s[1]))
        if len(matches) > 1:
            # Near-static cameras put several states inside the tolerance. What matters is how
            # far their actor roots are apart: that is the cost of picking the wrong one.
            roots = [m[2] for m in matches]
            spread_mm.append(
                max(distance_mm(a["player_position"], b["player_position"]) for a in roots for b in roots)
            )
            spread_deg.append(
                max(angle_deg(a["player_rotation"], b["player_rotation"]) for a in roots for b in roots)
            )
        # One camera content seen with more than one actor-root content is a torn pair.
        group = [m[2] for m in matches if m[2]["camera_words"] == best[2]["camera_words"]]
        if len({e["player_words"] for e in group}) > 1:
            torn.append(
                {"present_seq": seq, "actor_root_contents": len({e["player_words"] for e in group})}
            )
        groups[seq] = group
        joined[seq] = best[2]
        residual_deg.append(best[0])
        residual_mm.append(best[1])
        ages_ms.append(1000.0 * (qpc - best[2]["first_begin_qpc"]) / frequency)
        span_ms.append(1000.0 * (best[2]["last_end_qpc"] - best[2]["first_begin_qpc"]) / frequency)

    # Increment errors on moving anchors. The intrinsic series (actor root vs memory camera of
    # the SAME dense entry) is the floor: eye-vs-root motion, not frame misalignment.
    late = []
    by_rule = {rule: [] for rule in RULES}
    floor_by_rule = {rule: [] for rule in RULES}
    for seq in sorted(cameras):
        if seq - 1 not in cameras:
            continue
        c1, c0 = cameras[seq]["camera_position"], cameras[seq - 1]["camera_position"]
        if distance_mm(c1, c0) <= moving_mm:
            continue
        if seq in players and seq - 1 in players:
            late.append(
                increment_error_mm(
                    players[seq]["player_position"], players[seq - 1]["player_position"], c1, c0, up_axis
                )
            )
        if seq in groups and seq - 1 in groups:
            for rule in RULES:
                j1, j0 = pick(groups[seq], rule), pick(groups[seq - 1], rule)
                by_rule[rule].append(
                    increment_error_mm(j1["player_position"], j0["player_position"], c1, c0, up_axis)
                )
                floor_by_rule[rule].append(
                    increment_error_mm(
                        j1["player_position"],
                        j0["player_position"],
                        j1["camera_position"],
                        j0["camera_position"],
                        up_axis,
                    )
                )

    return {
        "schema": "dm-cp2077-dense-pair-join-analysis/v1",
        "identity": identity,
        "tolerance": {
            "position_mm": position_tolerance_mm,
            "rotation_deg": rotation_tolerance_deg,
            "lookback_ms": lookback_ms,
            "eligibility": "entry first seen at or before the Present QPC",
        },
        "presents": len(cameras),
        "dense_entries": len(entries),
        "sampler_summary": summary,
        "hole_between_consecutive_entries_ms": percentiles(holes_ms),
        "matched_presents": len(joined),
        "unmatched_presents": len(unmatched),
        "unmatched": unmatched[:64],
        "torn_pair_presents": len(torn),
        "torn": torn[:64],
        "candidates_per_present": percentiles([float(v) for v in candidate_counts]),
        "actor_root_spread_among_multiple_candidates_mm": percentiles(spread_mm),
        "actor_root_spread_among_multiple_candidates_deg": percentiles(spread_deg),
        "match_residual_mm": percentiles(residual_mm),
        "match_residual_deg": percentiles(residual_deg),
        "matched_entry_age_at_present_ms": percentiles(ages_ms),
        "matched_entry_observed_span_ms": percentiles(span_ms),
        "update_order": transition_patterns(entries, frequency, contiguous_ms, moving_mm),
        "moving_anchor_increment_error_mm": {
            "formal_late_sample": percentiles(late),
            "content_joined_by_rule": {rule: percentiles(by_rule[rule]) for rule in RULES},
            "intrinsic_floor_same_entries_by_rule": {
                rule: percentiles(floor_by_rule[rule]) for rule in RULES
            },
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--position-tolerance-mm", type=float, default=3.0)
    parser.add_argument("--rotation-tolerance-deg", type=float, default=2e-4)
    parser.add_argument("--lookback-ms", type=float, default=250.0)
    parser.add_argument("--up-axis", type=int, default=1, help="delivery up axis index")
    parser.add_argument("--moving-mm", type=float, default=5.0)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = analyze(
        rows(args.run_dir / "camera-pose-index.jsonl"),
        rows(args.run_dir / "player-pose-index.jsonl"),
        rows(args.run_dir / "cp2077-dense-pairs.jsonl"),
        args.position_tolerance_mm,
        args.rotation_tolerance_deg,
        args.lookback_ms,
        args.up_axis,
        args.moving_mm,
    )
    text = json.dumps(result, indent=2, ensure_ascii=False)
    if args.output:
        args.output.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
