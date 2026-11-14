#!/usr/bin/env python3
"""Read-only A/B pass-context tables. Observed API state never proves pixel writes."""
import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ml"))
from diogenes_wm.eval.cp2077_camera_report import read_camera_report, V2, V3


def projection_group(draw):
    values = sorted({float(m["matrix"][2][2]) for m in draw["matrices"]
                     if m["matrix_kind"] == "projection"})
    # Display labels only. These tolerances never affect matrix admission or pairing.
    if len(values) != 1:
        return ("no_projection" if not values else "multiple_projections"), values
    value = values[0]
    if not math.isfinite(value):
        raise ValueError("non-finite projection coefficient")
    if math.isclose(value, -5.00679e-6, rel_tol=0, abs_tol=1e-11):
        return "A_far_about_3995", values
    if math.isclose(value, -1.1920929e-6, rel_tol=0, abs_tol=1e-11):
        return "B_far_about_16777", values
    return "other_projection", values


def label(value):
    return "unknown" if value is None else json.dumps(value, sort_keys=True, separators=(",", ":"))


def summarize(report):
    if report.get("schema") not in (V2, V3) or report.get("non_trainable") is not True:
        raise ValueError("expected a checked non-trainable CP2077 camera observation v2/v3")
    observation = report["observation"]
    groups = defaultdict(lambda: {"draws": 0, "histograms": defaultdict(Counter)})
    draws = []
    for draw in observation["observations"]:
        group, values = projection_group(draw)
        context = draw.get("pass_context")
        if context is not None and (context.get("schema") != "dm-d3d12-draw-pass/v1"
                                    or context.get("non_trainable") is not True):
            raise ValueError("unsupported draw-pass context")
        state = context or {}
        pipeline = state.get("pipeline") or {}
        flags = state.get("dsv_flags")
        row = groups[group]
        row["draws"] += 1
        fields = {
            "context_present": context is not None,
            "projection_m22": values,
            "pipeline_depth_enable": draw.get("pipeline_depth_enable"),
            "pipeline_depth_write": draw.get("pipeline_depth_write"),
            "depth_comparison": pipeline.get("depth_comparison"),
            "depth_write_mask": pipeline.get("depth_write_mask"),
            "pso_dsv_format": pipeline.get("dsv_format"),
            "stencil_enable": pipeline.get("stencil_enable"),
            "stencil_front": pipeline.get("front"),
            "stencil_back": pipeline.get("back"),
            "depth_bounds_enable": pipeline.get("depth_bounds_enable"),
            "dsv_flags": flags,
            "dsv_read_only_depth": None if flags is None else bool(flags & 1),
            "viewport": state.get("viewport"),
            "scissor": state.get("scissor"),
            "copy_before_clear_submitted": draw.get("copy_before_clear_submitted"),
            "selected_present_identity_passed": draw.get("selected_present_identity_passed"),
        }
        for name, value in fields.items():
            row["histograms"][name][label(value)] += 1
        draws.append({
            "group": group, "projection_m22": values,
            **{key: draw.get(key) for key in (
                "present_seq", "queue_tag", "command_list_tag", "command_list_generation",
                "draw_ordinal", "pass_ordinal", "draw_ordinal_in_pass", "execute_batch",
                "list_index_in_batch", "depth_copy_execute_batch", "depth_copy_list_index_in_batch",
                "depth_copy_command_list_tag", "depth_copy_draws_before_in_list",
                "selected_depth_resource_tag", "selected_depth_resource_epoch",
                "selected_depth_descriptor_generation", "root_signature_sha256",
                "pipeline_depth_enable", "pipeline_depth_write", "copy_before_clear_submitted",
                "selected_present_identity_passed")},
            "pass_context": context,
        })
    return {
        "schema": "dm-cp2077-draw-pass-readout/v1", "non_trainable": True,
        "pixel_write_proven": False, "camera_binding_resolved": False,
        "limitations": [
            "API state is not GPU pixel-contribution evidence; no camera is selected by this report.",
            "Dynamic stencil reference, depth-bounds values, rasterizer/shader depth output are not observed here.",
            "Only exactly one viewport/scissor is retained; absent, multiple or unreadable state is not full screen.",
            "Only existing bounded camera samples are included; this is not a complete draw log.",
        ],
        "groups": dict(groups), "draws": draws,
        "loss_counters": {key: observation.get(key) for key in (
            "tracker_contention_drops", "hot_state_drops", "pending_draw_overflow",
            "present_observation_overflow", "draw_scan_budget_exhausted")},
        "scissor_timing": observation.get("hook_cpu_timing", {}).get("camera_scissor_observation"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--camera-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source, output = args.camera_report.resolve(), args.output.resolve()
    if output.is_relative_to(source.parent):
        parser.error("output must be outside the original session")
    if output.exists() or not output.parent.is_dir():
        parser.error("output must be new and its parent must exist")
    data = source.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    result = summarize(read_camera_report(source))
    if hashlib.sha256(source.read_bytes()).hexdigest() != digest:
        raise ValueError("camera report changed during analysis")
    result["source_sha256"] = digest
    result["source_unchanged"] = True
    with output.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(result, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write("\n")
    print(json.dumps({"output": str(output), "draws": len(result["draws"]),
                      "groups": {k: v["draws"] for k, v in result["groups"].items()}}))


if __name__ == "__main__":
    main()
