"""Immutable native inputs for the local CP2077 selected-Present geometry check."""
from __future__ import annotations

import json
from pathlib import Path

import numpy as np

from .cp2077_egeo_gate import EXACT_EXE_SHA256, _sha256, _validate_metric_report
from .cp2077_camera_report import read_camera_report, V2, V3


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def canonical(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


class Session:
    def __init__(self, inventory_path: Path):
        self.inventory_path = inventory_path.resolve(strict=True)
        inventory = read_json(self.inventory_path)
        require(inventory["schema"] == "local-immutable-session-sha/v1", "unknown SHA inventory")
        self.root = Path(inventory["session"]).resolve(strict=True)
        self.files = {row["path"]: row for row in inventory["files"]}
        require(len(self.files) == len(inventory["files"]), "duplicate inventory path")

    def path(self, name: str) -> Path:
        require(name in self.files, f"unbound evidence: {name}")
        path = self.root / name
        require(not path.is_symlink() and path.is_file()
                and path.resolve().is_relative_to(self.root), "evidence path escapes session")
        return path

    def verify(self, name: str) -> Path:
        path = self.path(name)
        row = self.files[name]
        require(path.stat().st_size == row["bytes"] and _sha256(path) == "sha256:" + row["sha256"],
                f"original evidence changed: {name}")
        return path

    def json(self, name: str):
        require(self.files[name]["bytes"] <= 320 * 1024 * 1024, "JSON exceeds camera-report bound")
        return read_json(self.verify(name))

    def lines(self, name: str):
        path = self.verify(name)
        require(path.stat().st_size <= 64 * 1024 * 1024, "index exceeds bound")
        if name.endswith(".zst"):
            import zstandard
            with path.open("rb") as handle, zstandard.ZstdDecompressor().stream_reader(handle) as reader:
                raw = reader.read(64 * 1024 * 1024 + 1)
        else:
            raw = path.read_bytes()
        require(len(raw) <= 64 * 1024 * 1024, "expanded index exceeds bound")
        return [json.loads(line) for line in raw.splitlines()]

    def verify_all(self):
        actual = {p.relative_to(self.root).as_posix() for p in self.root.rglob("*") if p.is_file()}
        require(actual == set(self.files), "original session file set changed")
        for name in sorted(self.files):
            self.verify(name)
        return {"inventory_sha256": _sha256(self.inventory_path), "files": len(self.files),
                "bytes": sum(row["bytes"] for row in self.files.values()), "all_original_sha_match": True}


def before_copy(draw: dict) -> bool:
    for left, right in (("execute_batch", "depth_copy_execute_batch"),
                        ("list_index_in_batch", "depth_copy_list_index_in_batch")):
        a, b = draw.get(left), draw.get(right)
        require(type(a) is int and type(b) is int, "unknown draw/copy order")
        if a != b:
            return a < b
    a, b = draw.get("draw_ordinal"), draw.get("depth_copy_draws_before_in_list")
    require(type(a) is int and type(b) is int and draw.get("pass_ordinal") is not None,
            "unknown intra-list draw/copy order")
    return a <= b


def selection_stride(summary: dict) -> int:
    schema = summary.get("schema")
    require(schema in ("dm-cp2077-raw-selected-summary/v1", "dm-cp2077-raw-selected-summary/v2"),
            "unknown selected feedback summary")
    if schema.endswith("/v1"):
        return 1
    require(summary.get("feedback_schema") == 4 and summary.get("depth_selection") == "even_cfr_30fps/v1"
            and summary.get("generation") == summary.get("resource_epoch") == 1,
            "even-CFR selection policy/epoch mismatch")
    return 2


def validate_even_cfr_metadata(summary: dict, arm: dict, frames_name: str,
                               meta: dict, frame_count: int) -> None:
    require(selection_stride(summary) == 2, "expected explicit even-CFR policy")
    launch = summary.get("process_launch_id")
    require(isinstance(launch, str) and len(launch) == 32
            and all(c in "0123456789abcdef" for c in launch), "invalid even-CFR launch")
    require(frames_name == f"rgb-recorder/raw-rgb-{launch}/runs/r0000/segments/00000/frames.jsonl.zst",
            "even-CFR RGB index belongs to another run")
    selected = arm.get("selected_present", {})
    require(arm.get("schema") == "dm-cp2077-raw-depth-demo-arm-request/v2"
            and arm.get("process_launch_id") == launch and arm.get("capture_contract") == "issue735_local"
            and arm.get("duration_ms") == 64000 and arm.get("target_depth_hz") == 15
            and type(arm.get("manual_stop")) is bool
            and selected.get("schema") == "dm-cp2077-raw-selected-request/v2"
            and selected.get("feedback_schema") == 4 and selected.get("depth_selection") == "even_cfr_30fps/v1"
            and selected.get("holstered") is True and selected.get("non_trainable") is True
            and selected.get("bootstrap", {}).get("resource_epoch") == 1, "even-CFR Arm policy mismatch")
    require(all(type(meta.get(k)) is int and meta[k] == v for k, v in
                (("fps_num", 30), ("fps_den", 1), ("width", 1920), ("height", 1080), ("n_frames", frame_count))),
            "even-CFR requires matching native 30 FPS RGB metadata")


def join_frames(frames: list[dict], depth: list[dict], *, depth_stride: int = 1) -> dict[int, int]:
    require(depth_stride in (1, 2), "unsupported depth selection stride")
    require(2 <= len(depth) <= 1024 and 2 <= len(frames) <= 4096, "input frame budget exceeded")
    by_present = {}
    for ordinal, row in enumerate(depth):
        require(row["ordinal"] == ordinal and row["present_seq"] not in by_present,
                "depth ordinal or identity repeats")
        require(row["present_qpc"] > 0 and (ordinal == 0 or (
            row["present_seq"] > depth[ordinal-1]["present_seq"]
            and row["present_qpc"] > depth[ordinal-1]["present_qpc"])), "depth timeline regressed")
        by_present[row["present_seq"]] = row
    first_rgb = {}
    for index, frame in enumerate(frames):
        seq = frame.get("present_seq")
        require(frame["session_frame"] == index and type(seq) is int and seq > 0
                and type(frame["source_qpc"]) is int and frame["source_qpc"] > 0,
                "RGB lacks an ordered full identity")
        if index:
            require(seq >= frames[index-1]["present_seq"] and
                    frame["source_qpc"] >= frames[index-1]["source_qpc"], "RGB timeline regressed")
        if frame.get("duplicate"):
            require(index > 0 and seq == frames[index-1]["present_seq"], "duplicate changed identity")
            if depth_stride == 2:
                require(frame["source_qpc"] == frames[index-1]["source_qpc"], "duplicate changed source QPC")
        if index % depth_stride:
            continue
        require(seq in by_present, "required RGB lacks exact depth identity")
        require(frame["source_qpc"] >= by_present[seq]["present_qpc"], "RGB precedes its depth")
        first_rgb.setdefault(seq, index)
    require(set(first_rgb) == set(by_present), "some original depth has no selected RGB")
    return first_rgb


def matrices_for_depth(draws: list[dict], depth: list[dict], admission: dict) -> list[dict]:
    require(admission["schema"] == "cp2077-holstered-same-launch-camera/v2"
            and admission["non_trainable"] is True
            and admission["production_camera_binding_emitted"] is False
            and admission["cross_launch_admission"] is False, "unsupported camera admission")
    allowed = {kind: {canonical(s) for s in admission[kind + "_sources"]}
               for kind in ("view", "projection")}
    by_present = {}
    camera_times = {}
    for draw in draws:
        present, qpc = draw["present_seq"], draw["present_qpc"]
        require(type(qpc) is int and qpc > 0, "invalid camera observation QPC")
        require(present not in camera_times or camera_times[present] == qpc,
                "same Present has conflicting camera observation times")
        camera_times[present] = qpc
        if draw.get("selected_present_identity_passed"):
            require(before_copy(draw), "post-copy draw was admitted")
            require(draw.get("copy_before_clear_submitted") is True, "missing copy submission")
            if draw.get("pipeline_depth_write") is True:
                by_present.setdefault(draw["present_seq"], []).append(draw)
    samples = []
    ordered_times = sorted(camera_times.items())
    require(all(a[1] < b[1] for a, b in zip(ordered_times, ordered_times[1:])),
            "camera observation timeline regressed")
    previous_time = {p: ordered_times[i-1][1] if i else 0 for i, (p, _) in enumerate(ordered_times)}
    for raw in depth:
        candidates = []
        for draw in by_present.get(raw["present_seq"], []):
            # note_present() stamps the raw index at callback entry; before_present() samples
            # a second QPC for the camera tracker later in the SAME callback. Preserve both.
            require(previous_time[raw["present_seq"]] < raw["present_qpc"] <= draw["present_qpc"],
                    "raw Present entry QPC is outside its camera observation interval")
            require(draw["pipeline_depth_enable"] is True, "writer has no depth test")
            context = draw["pass_context"]
            require(context["pipeline"]["depth_write_mask"] == 1
                    and context["pipeline"]["dsv_format"] == 20
                    and context["dsv_flags"] & 1 == 0, "writer PSO/DSV mismatch")
            viewport = np.array(context["viewport"]["single_bits"], dtype="<u4").view("<f4")
            require(context["viewport"]["count"] == 1
                    and np.array_equal(viewport, [0, 0, 1920, 1080, 0, 1]), "unexpected viewport")
            selected = {}
            for kind in ("view", "projection"):
                selected[kind] = [m for m in draw["matrices"] if m["matrix_kind"] == kind]
                require(bool(selected[kind]) and all(canonical(m["stable_source"]) in allowed[kind]
                        for m in selected[kind]), "writer is outside frozen camera family")
            for v in selected["view"]:
                for p in selected["projection"]:
                    if v["stable_source"]["descriptor_table"] != p["stable_source"]["descriptor_table"]:
                        continue
                    require(v["copy_receipt"] == p["copy_receipt"]
                            and v["copy_receipt"]["schema"] == "dm-d3d12-executed-cbv-copy/v2"
                            and v["copy_receipt"]["queue_tag"] == draw["queue_tag"], "physical camera copy mismatch")
                    require(v["translation_slot"] == "row3", "unsupported view convention")
                    V, P = (np.asarray(m["matrix"], dtype=np.float32).astype(np.float64) for m in (v, p))
                    require(V.shape == P.shape == (4, 4) and np.isfinite(V).all() and np.isfinite(P).all(),
                            "non-finite camera matrix")
                    # The native world-to-camera basis can have negative parity. Its original
                    # V is retained; equal parity cancels in inv(V_left) @ V_right.
                    require(np.max(np.abs(V[:3, :3] @ V[:3, :3].T - np.eye(3))) < 1e-4
                            and abs(abs(np.linalg.det(V[:3, :3])) - 1) < 1e-4
                            and np.max(np.abs(V[:, 3] - [0, 0, 0, 1])) < 1e-4,
                            "non-rigid camera view")
                    candidates.append((V, P))
        require(bool(candidates), f"no admitted depth writer at Present {raw['present_seq']}")
        V, P = candidates[0]
        require(all(np.array_equal(V, v) and np.array_equal(P, p) for v, p in candidates),
                "admitted writer matrices conflict")
        require(P[2, 3] == 1 and P[3, 3] == 0 and P[0, 0] > 0 and P[1, 1] > 0 and P[3, 2] > 0,
                "unsupported physical projection")
        samples.append(dict(raw, view=V, projection=P, matching_writer_draws=len(candidates),
                            camera_observation_qpc=camera_times[raw["present_seq"]]))
    require(len({int(np.sign(np.linalg.det(s["view"][:3, :3]))) for s in samples}) == 1,
            "camera basis parity changed within the session")
    return samples


def load_session(session: Session, metric_reference: dict):
    profile = session.json("raw-depth-demo-profile.json")
    require(profile["executable_sha256"] == EXACT_EXE_SHA256
            and profile["resident_probe_sha256"] == "sha256:" + metric_reference["current_probe_sha256"],
            "metric reference does not cover this game/probe")
    require(session.files["raw-depth-demo-profile.json"]["sha256"] == metric_reference["current_profile_sha256"],
            "metric reference profile differs")
    summary = session.json("raw-selected-summary.json")
    depth_stride = selection_stride(summary)
    require(summary["sealed"] is True
            and summary["fault"] is None and summary["unresolved"] == 0
            and summary["decision_queue_depth"] == summary["ack_queue_depth"] == 0
            and summary["non_trainable"] is True and summary["recording_ready"] is False,
            "selected feedback was not completely sealed")
    report = read_camera_report(session.verify("render-camera-observation.json"))
    require(report["schema"] in (V2, V3)
            and report["process_launch_id"] == summary["process_launch_id"], "camera launch mismatch")
    observation = report["observation"]
    for key, value in observation.items():
        if any(word in key for word in ("overflow", "drops", "exhausted")) and type(value) is int:
            require(value == 0, f"camera evidence lost observations: {key}")
    archive = report["observation_archive"]
    require(archive["file"] == "render-camera-draws.jsonl"
            and archive["sha256"] == "sha256:" + session.files[archive["file"]]["sha256"]
            and observation["local_archive"]["frozen"] is True
            and observation["local_archive"]["enqueue_failures"] == 0, "camera archive is incomplete")
    raw = session.lines("raw-depth-index.jsonl")
    for row in raw:
        require(row["schema"] == "dm-cp2077-raw-depth-frame-index/v1"
                and (row["width"], row["height"], row["row_pitch"], row["byte_len"])
                == (1920, 1080, 7680, 8294400)
                and row["pixel_format"] == "DXGI_FORMAT_D32_FLOAT_LE"
                and row["source_candidate_id"] == profile["selector"]["candidate_id"], "raw depth layout/source mismatch")
    names = [name for name in session.files if name.endswith("/segments/00000/frames.jsonl.zst")]
    require(len(names) == 1, "expected one native RGB segment")
    frames_name = names[0]
    frames = session.lines(frames_name)
    if depth_stride == 2:
        validate_even_cfr_metadata(summary, session.json("raw-depth-demo-arm-request.json"),
            frames_name, session.json(frames_name.removesuffix("frames.jsonl.zst") + "meta.json"), len(frames))
    first_rgb = join_frames(frames, raw, depth_stride=depth_stride)
    required_frames = (len(frames) + depth_stride - 1) // depth_stride
    if depth_stride == 2:
        require(summary["required_depth_rgb_frames"] == required_frames
                and summary["skipped_depth_rgb_frames"] == len(frames) - required_frames,
                "even-CFR required/skipped counters disagree")
    require(summary["selected_rgb_frames"] == len(frames)
            and summary["distinct_selected_depth"] == summary["written_depth_frames"] == len(raw)
            and summary["readback_submitted"] == summary["readback_completed"] == len(raw)
            and summary["aliases"] == required_frames - len(raw), "selected counters disagree")
    samples = matrices_for_depth(observation["observations"], raw, summary["camera_admission"])
    for sample in samples:
        sample["session_frame"] = first_rgb[sample["present_seq"]]
        require(sample["camera_observation_qpc"] <= frames[sample["session_frame"]]["source_qpc"],
                "RGB source predates its observed camera/watermark callback")
    frequency = session.json("raw-depth-telemetry.json")["qpc_frequency"]
    require(type(frequency) is int and frequency > 0, "invalid QPC frequency")
    return {"samples": samples, "frames": frames, "frequency": frequency,
            "process_launch_id": summary["process_launch_id"], "camera_admission": summary["camera_admission"],
            "depth_stride": depth_stride, "required_rgb_frames": required_frames,
            "video": session.verify(frames_name.removesuffix("frames.jsonl.zst") + "video.mp4")}


def load_metric(path: Path):
    reference = read_json(path)
    require(reference["schema"] == "local-cp2077-verified-metric-reference/v1"
            and reference["operator_reuse_confirmed"] is True
            and reference["production_camera_binding"] is False, "metric reuse is not established")
    root = Path(reference["calibration_session"]).resolve(strict=True)
    evidence = root / "cp2077-metric-evidence.json"
    require(_sha256(evidence) == "sha256:" + reference["calibration_evidence_sha256"], "metric evidence changed")
    metric = read_json(evidence)
    _validate_metric_report(root, metric)
    return reference, {"reference_sha256": _sha256(path), "evidence_sha256": _sha256(evidence),
                       "metres_per_world_unit": metric["adopted_metres_per_world_unit"]}
