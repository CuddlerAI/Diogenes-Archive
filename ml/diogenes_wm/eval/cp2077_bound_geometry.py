"""Explicit binding and native pose inputs for selected-Present geometry.

Never infer axes or elect sources. Native replay exports retain original matrices,
and the geometry computation uses the exported production-decoded pose.
"""
from pathlib import Path
import json
import math

import numpy as np

from . import geo_probes
from .cp2077_egeo_gate import _sha256
from .cp2077_selected_inputs import canonical, read_json, require

INPUT_SCHEMA = "dm-cp2077-bound-selected-geometry-input/v1"
REPORT_SCHEMA = "dm-cp2077-bound-selected-geometry/v1"


def axis_matrix(mapping):
    matrix = np.zeros((3, 3))
    for i, label in enumerate(("x", "y", "z")):
        value = mapping[f"delivery_{label}_from"]
        sign, axis = value.split("_")
        require(sign in ("positive", "negative") and axis in ("x", "y", "z"), "invalid signed axis")
        matrix[i, ("x", "y", "z").index(axis)] = 1 if sign == "positive" else -1
    require(np.array_equal(matrix @ matrix.T, np.eye(3)), "axes are not a permutation")
    return matrix


def relative_motion(left, right):
    """Production camera→world pose, converted back to original projection coordinates."""
    require(np.array_equal(left["bound_camera_basis"], right["bound_camera_basis"])
            and left["bound_scale"] == right["bound_scale"], "bound coordinate policy changed")
    rel = geo_probes.RelPose.between(left["bound_position"], left["bound_quaternion"],
                                    right["bound_position"], right["bound_quaternion"])
    column = np.eye(4)
    column[:3, :3], column[:3, 3] = rel.rotation, rel.translation
    source_to_delivery = np.eye(4)
    source_to_delivery[:3, :3] = left["bound_camera_basis"].T * left["bound_scale"]
    return source_to_delivery @ column.T @ np.linalg.inv(source_to_delivery)


class Context:
    def __init__(self, manifest):
        self.binding_path = Path(manifest["binding"]).resolve(strict=True)
        self.replay_path = Path(manifest["native_pose_replay"]).resolve(strict=True)
        require(self.binding_path.stat().st_size <= 1024 * 1024
                and self.replay_path.stat().st_size <= 1024 * 1024, "binding/replay exceeds bound")
        self.binding_sha = _sha256(self.binding_path)
        self.replay_sha = _sha256(self.replay_path)
        self.binding, replay = read_json(self.binding_path), read_json(self.replay_path)
        b = self.binding
        require(b["schema"] == "camera-binding/cp2077-holstered-v3"
                and (b["render_width"], b["render_height"]) == (1920, 1080), "unsupported binding")
        require(replay["schema"] == "cp2077-bound-camera-replay/v2" and replay["non_trainable"] is True
                and replay["acceptance"] is False and replay["production_binding_authorized"] is False
                and replay["binding_sha256"] == self.binding_sha, "native replay binding/flags mismatch")
        require(1 <= len(replay["runs"]) <= 3, "invalid native replay run count")
        self.world = axis_matrix(b["axis_mapping"])
        self.camera = axis_matrix(b["holstered_camera"]["camera_axis_mapping"])
        self.scale = float(b["view_metres_per_unit"])
        require(math.isfinite(self.scale) and self.scale > 0, "invalid bound metric scale")
        self.runs = {r["pose_export"]["process_launch_id"]: r for r in replay["runs"]}
        require(len(self.runs) == len(replay["runs"]), "duplicate native launch")
        self.pose_files = []
        self.sessions = []

    def attach(self, session, data, metric):
        require(self.scale == metric["metres_per_world_unit"], "binding disagrees with independent metric")
        launch = data["process_launch_id"]
        require(launch in self.runs, "no native pose export for this launch")
        run = self.runs[launch]
        export = run["pose_export"]
        require(run["source_sha256"] == "sha256:" + session.files["render-camera-observation.json"]["sha256"]
                and export["raw_depth_profile_sha256"] == "sha256:" + session.files["raw-depth-demo-profile.json"]["sha256"]
                and run["warmup_presents"] == 119
                and run["resolved_poses"] == export["rows"]
                and run["exact_copied_presents"] == 119 + export["rows"], "native replay provenance mismatch")
        profile = session.json("raw-depth-demo-profile.json")
        require(profile["executable_sha256"] == self.binding["executable_sha256"], "binding build differs")
        for kind in ("view", "projection"):
            declared = []
            for source in self.binding[kind + "_sources"]:
                source = dict(source)
                if kind == "view":
                    require(source.pop("translation") == "row3", "unsupported native view translation")
                declared.append(canonical(source))
            actual = [canonical(s) for s in data["camera_admission"][kind + "_sources"]]
            require(len(set(declared)) == len(declared) and set(declared) == set(actual),
                    "full binding source family differs from original admitted family")
        path = Path(export["file"]).resolve(strict=True)
        require(path.stat().st_size == export["bytes"] and 0 < path.stat().st_size <= 64 * 1024 * 1024
                and _sha256(path) == export["sha256"], "native pose export changed or exceeds bound")
        rows = [json.loads(s) for s in path.read_bytes().splitlines()]
        require(len(rows) == export["rows"] and 1 <= len(rows) <= 4096, "native pose row count mismatch")
        poses = {}
        for row in rows:
            present = row["present_seq"]
            require(type(present) is int and present > 0 and present not in poses
                    and (not poses or present > next(reversed(poses))), "native pose timeline repeats/regresses")
            require(row["schema"] == "camera-pose-index/v2" and row["source"] == "resolved_d3d12_camera_hook"
                    and row["provenance"]["kind"] == "resolved_d3d12_camera_hook"
                    and row["provenance"]["binding"] == self.binding, "pose uses another binding/source")
            poses[present] = row
        for sample in data["samples"]:
            require(sample["present_seq"] in poses, "selected depth lacks exact native bound pose")
            self.attach_sample(sample, poses[sample["present_seq"]])
        self.pose_files.append((path, export["sha256"]))
        self.sessions.append(dict(process_launch_id=launch,
            raw_depth_profile_sha256=export["raw_depth_profile_sha256"],
            inventory_sha256=_sha256(session.inventory_path), camera_report_sha256=run["source_sha256"],
            native_pose_sha256=export["sha256"], selected_depth_frames=len(data["samples"]),
            bound_pose_coverage=len(data["samples"]), required_rgb_frames=data["required_rgb_frames"],
            exact_rgb_pairs=data["required_rgb_frames"]))

    def attach_sample(self, sample, row):
        provenance = row["provenance"]
        for key, name in (("view", "view_matrix"), ("projection", "projection_matrix")):
            original = np.array(provenance[name], dtype=np.float32).astype(np.float64)
            require(np.array_equal(original, sample[key]), "native bound pose changed original matrix")
        require(row["present_qpc"] == sample["camera_observation_qpc"], "native pose QPC differs")
        position = np.asarray(row["camera_position"], dtype=float)
        quaternion = np.asarray(row["camera_rotation_quaternion"], dtype=float)
        require(position.shape == (3,) and quaternion.shape == (4,)
                and np.isfinite(position).all() and np.isfinite(quaternion).all()
                and abs(np.linalg.norm(quaternion) - 1) < 1e-5, "invalid native bound pose")
        rotation = geo_probes.quat_to_mat(quaternion)
        view = sample["view"]
        # Float32 production decoding is checked against the exact original matrix;
        # tolerance is numerical roundoff, never a geometry/coverage threshold.
        expected_position = self.world @ (-view[:3, :3] @ view[3, :3]) * self.scale
        require(np.allclose(position, expected_position, rtol=0, atol=1e-3)
                and np.allclose(rotation, self.world @ view[:3, :3] @ self.camera.T, rtol=0, atol=1e-5),
                "native decoded pose is inconsistent with declared W/C")
        sample.update(bound_position=position, bound_quaternion=quaternion,
                      bound_camera_basis=self.camera, bound_scale=self.scale)

    def finish(self, geometry, evaluate, thresholds):
        require(len(self.sessions) == len(self.runs), "unused native replay launch")
        for index, session in enumerate(self.sessions):
            pairs = [p for p in geometry["pairs"] if p["session_index"] == index]
            sampled = {kind: [p for p in pairs if kind in p["populations"]]
                       for kind in ("static", "translation", "rotation")}
            session["gate"] = dict(evaluate(pairs, sampled, thresholds),
                                   sampled_pairs={k: len(v) for k, v in sampled.items()})
        require(_sha256(self.binding_path) == self.binding_sha and _sha256(self.replay_path) == self.replay_sha
                and all(_sha256(p) == h for p, h in self.pose_files), "bound inputs changed during evaluation")
        return dict(schema=REPORT_SCHEMA, non_trainable=True, local_only=True, upload_allowed=False,
            acceptance=False, production_binding_authorized=False, binding=self.binding,
            binding_sha256=self.binding_sha, native_replay_sha256=self.replay_sha,
            metric_evidence_sha256=geometry["metric"]["evidence_sha256"],
            sessions=self.sessions, geometry=geometry)
