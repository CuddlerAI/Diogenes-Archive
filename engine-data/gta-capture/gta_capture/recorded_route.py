"""Author camera targets from sealed recording-side player observations.

The native task's legacy ``speed_mps`` field is a task parameter, not a measured
physical speed. This module keeps that command intact and derives trajectory
positions only from recorded telemetry. Its frame origin remains a recording-
side estimate, never independent proof of Rockstar replay-frame alignment.
"""

from __future__ import annotations

import math
import shutil
from pathlib import Path

from .errors import ContractError
from .evidence import (PLAYER_OBSERVATION_TOLERANCE_NS, PLAYER_TELEMETRY_SCHEMA,
                       REAL_PLAYER_OBSERVATION_METHOD)
from .jsonio import file_record, load_json, read_jsonl, sha256_file
from .scenario import Scenario


RECORDED_ROUTE_KIND = "recorded_master_telemetry"
OBSERVATION_SCHEMA = "dm-gta-recorded-route-observation/1"
SMOOTHING_METHOD = "centered_box_linear_extrapolation_anchor_preserving"
MAX_FIT_RESIDUAL_M = 0.25
RECORDING_ANCHOR_TOLERANCE_M = 0.25


def _finite(value) -> bool:
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(float(value)))


def _vector(value, length=3) -> bool:
    return isinstance(value, list) and len(value) == length and all(_finite(x) for x in value)


def _sha(value) -> bool:
    return (isinstance(value, str) and len(value) == 64
            and all(c in "0123456789abcdef" for c in value))


def native_command(player: dict) -> dict:
    return {**{key: player[key] for key in
               ("control", "route_kind", "distance_m", "speed_mps", "heading_deg")},
            "speed_semantics": "native_task_parameter_not_physical_mps"}


def _open_record(record, label: str) -> Path:
    if (not isinstance(record, dict) or not isinstance(record.get("path"), str)
            or not Path(record["path"]).is_absolute() or not _sha(record.get("sha256"))
            or not isinstance(record.get("bytes"), int) or isinstance(record.get("bytes"), bool)
            or record["bytes"] <= 0):
        raise ContractError(f"recorded route {label} requires a sealed absolute file reference")
    path = Path(record["path"])
    if (not path.is_file() or path.stat().st_size != record.get("bytes")
            or sha256_file(path) != record["sha256"]):
        raise ContractError(f"recorded route {label} differs from sealed bytes/sha256")
    return path


def _load_observations(files: dict, *, expected_replay_sha256=None):
    if not isinstance(files, dict) or set(files) != {"record_result", "scenario", "telemetry", "replay"}:
        raise ContractError("recorded route requires record, scenario, telemetry and replay files")
    paths = {key: _open_record(record, key) for key, record in files.items()}
    record = load_json(paths["record_result"])
    scenario = Scenario.load(paths["scenario"])
    if not isinstance(record, dict) or record.get("ok") is not True:
        raise ContractError("recorded route requires a successful native recording result")
    master, applied = record.get("master"), record.get("scenario")
    if not isinstance(master, dict) or not isinstance(applied, dict):
        raise ContractError("recorded route is missing native master/scenario evidence")
    replay_hash = files["replay"]["sha256"]
    if (master.get("mock") is not False or master.get("recording_start_verified") is not True
            or master.get("replay_sha256") != replay_hash
            or (expected_replay_sha256 is not None and expected_replay_sha256 != replay_hash)
            or master.get("player_telemetry_sha256") != files["telemetry"]["sha256"]
            or master.get("player_observation_method") != REAL_PLAYER_OBSERVATION_METHOD):
        raise ContractError("recorded route master/telemetry belongs to a different or unverified recording")
    if (applied.get("applied") is not True or applied.get("story_mode") is not True
            or applied.get("scenario_sha256") != files["scenario"]["sha256"]
            or master.get("source_epoch") != scenario.raw["source_epoch"]
            or master.get("seed") != scenario.raw["seed"]):
        raise ContractError("recorded route native result is not bound to the scenario")
    player = scenario.raw["player"]
    for key in ("distance_m", "speed_mps", "heading_deg"):
        # The native apply_scenario receipt uses std::to_string(double), which
        # emits six decimal places for all three fields. Compare that exact
        # representation; keep the full sealed command in native_command().
        reported_value = float(f"{float(player[key]):.6f}")
        if not _finite(applied.get("route_" + key)) or applied["route_" + key] != reported_value:
            raise ContractError("recorded route native command differs from the sealed scenario")
    if (applied.get("route_control") != player["control"]
            or applied.get("route_kind") != player["route_kind"]
            or master.get("source_control") != player["control"]
            or master.get("route_kind") != player["route_kind"]):
        raise ContractError("recorded route uses an unsupported native command")
    identity = master.get("source_clip_identity")
    if (not isinstance(identity, str) or len(identity) != 16
            or any(c not in "0123456789abcdef" for c in identity)
            or identity == "0" * 16):
        raise ContractError("recorded route master clip identity is invalid")
    rows = read_jsonl(paths["telemetry"])
    n_frames = round(scenario.duration_s * 30)
    if (abs(scenario.duration_s * 30 - n_frames) > 1e-6 or len(rows) != n_frames
            or master.get("player_observation_count") != n_frames or n_frames < 2):
        raise ContractError("recorded route telemetry does not cover the complete scenario timeline")
    previous_observation = -1
    for frame, row in enumerate(rows):
        planned_ns = round(frame * 1_000_000_000 / 30)
        observed_ns = row.get("observation_t_ns")
        state = row.get("player_state")
        if (row.get("schema") != PLAYER_TELEMETRY_SCHEMA
                or row.get("player_observation_method") != REAL_PLAYER_OBSERVATION_METHOD
                or row.get("replay_sha256") != replay_hash
                or row.get("source_clip_identity") != identity
                or row.get("frame") != frame or isinstance(row.get("frame"), bool)
                or row.get("source_t_ns") != planned_ns or isinstance(row.get("source_t_ns"), bool)
                or not isinstance(observed_ns, int) or isinstance(observed_ns, bool)
                or not planned_ns <= observed_ns <= planned_ns + PLAYER_OBSERVATION_TOLERANCE_NS
                or observed_ns <= previous_observation
                or not isinstance(state, dict)
                or not all(_vector(state.get(key)) for key in ("position", "velocity", "euler_deg"))
                or not _vector(state.get("quaternion"), 4)
                or not _sha(row.get("entity_state_hash"))):
            raise ContractError(f"recorded route telemetry frame {frame} has invalid origin/time/state binding")
        previous_observation = observed_ns
    first = rows[0]["player_state"]["position"]
    start = master.get("record_start_position")
    if (not _vector(start)
            or math.dist(start, scenario.raw["location"]) > RECORDING_ANCHOR_TOLERANCE_M
            or math.dist(first, start) > RECORDING_ANCHOR_TOLERANCE_M):
        raise ContractError("recorded route does not begin at the sealed Story anchor")
    return scenario, master, rows


def smooth_recorded_positions(positions: list[list[float]], *, window_frames=15,
                              passes=3) -> list[list[float]]:
    if (not isinstance(window_frames, int) or isinstance(window_frames, bool)
            or not 1 <= window_frames <= 61 or window_frames % 2 != 1
            or not isinstance(passes, int) or isinstance(passes, bool) or not 0 <= passes <= 3
            or len(positions) < 2 or not all(_vector(p) for p in positions)):
        raise ContractError("recorded route smoothing parameters/positions are invalid")
    result = [[float(x) for x in p] for p in positions]
    half = window_frames // 2
    for _ in range(passes):
        previous = result
        def extrapolated(index):
            if index < 0:
                return [previous[0][a] + index * (previous[1][a] - previous[0][a]) for a in range(3)]
            if index >= len(previous):
                return [previous[-1][a] + (index - len(previous) + 1)
                        * (previous[-1][a] - previous[-2][a]) for a in range(3)]
            return previous[index]
        result = [[sum(extrapolated(index)[axis] for index in range(frame - half, frame + half + 1))
                   / window_frames for axis in range(3)] for frame in range(len(previous))]
    offset = [result[0][a] - positions[0][a] for a in range(3)]
    return [[round(point[a] - offset[a], 9) for a in range(3)] for point in result]


def _diagnostics(rows, smoothed):
    observed = [row["player_state"]["position"] for row in rows]
    residuals = sorted(math.dist(a, b) for a, b in zip(observed, smoothed))
    distances = [math.dist(a, b) for a, b in zip(observed, observed[1:])]
    deltas = [(b["observation_t_ns"] - a["observation_t_ns"]) / 1e9 for a, b in zip(rows, rows[1:])]
    speeds = sorted(distance / dt for distance, dt in zip(distances, deltas))
    elapsed = (rows[-1]["observation_t_ns"] - rows[0]["observation_t_ns"]) / 1e9
    return {
        "observed_path_distance_m": sum(distances),
        "observed_displacement_m": math.dist(observed[0], observed[-1]),
        "observed_average_speed_mps": sum(distances) / elapsed,
        "observed_p95_speed_mps": speeds[math.ceil(len(speeds) * 0.95) - 1],
        "fit_max_residual_m": residuals[-1],
        "fit_p95_residual_m": residuals[math.ceil(len(residuals) * 0.95) - 1],
        "player_replay_origin_verified": False,
    }


def build_recorded_subject_route(record_result_path, scenario_path, *, window_frames=15,
                                  passes=3, max_residual_m=MAX_FIT_RESIDUAL_M,
                                  look_at_height_m=0.925, artifact_dir=None) -> dict:
    record = load_json(record_result_path)
    master = record.get("master", {}) if isinstance(record, dict) else {}
    inputs = {"record_result": Path(record_result_path), "scenario": Path(scenario_path),
              "telemetry": Path(master.get("player_telemetry_path", "")),
              "replay": Path(master.get("replay_path", ""))}
    if not all(path.is_file() for path in inputs.values()):
        raise ContractError("recorded route input artifacts are missing")
    files = {key: file_record(path) for key, path in inputs.items()}
    scenario, master, rows = _load_observations(files)
    if artifact_dir is not None:
        target = Path(artifact_dir)
        target.mkdir(parents=True, exist_ok=True)
        names = {"record_result": "source-record-result.json", "scenario": "source-scenario.json",
                 "telemetry": "master-player-telemetry.jsonl", "replay": "master-replay.clip"}
        for key, path in inputs.items():
            destination = target / names[key]
            if destination.exists():
                if sha256_file(destination) != files[key]["sha256"]:
                    raise ContractError("recorded route snapshot destination already contains different bytes")
            else:
                shutil.copyfile(path, destination)
            relocated = file_record(destination)
            if any(relocated[field] != files[key][field] for field in ("sha256", "bytes")):
                raise ContractError("recorded route source changed while relocating its snapshot")
            files[key] = relocated
    positions = [row["player_state"]["position"] for row in rows]
    smoothed = smooth_recorded_positions(positions, window_frames=window_frames, passes=passes)
    route = {
        "schema": "dm-gta-subject-route/1", "route_kind": RECORDED_ROUTE_KIND,
        "control": scenario.raw["player"]["control"],
        "native_command": native_command(scenario.raw["player"]),
        "duration_s": scenario.duration_s,
        "waypoints": [{"t": frame / 30, "position": point, "look_at_height_m": look_at_height_m}
                      for frame, point in enumerate(smoothed)],
        "observation": {
            "schema": OBSERVATION_SCHEMA, "method": REAL_PLAYER_OBSERVATION_METHOD,
            "player_replay_origin_verified": False, "files": files,
            "source_clip_identity": master["source_clip_identity"],
            "selection": {"start_frame": 0, "n_frames": len(rows)},
            "smoothing": {"method": SMOOTHING_METHOD, "window_frames": window_frames,
                          "passes": passes, "max_residual_m": max_residual_m},
            "look_at_height_m": look_at_height_m,
        },
        "diagnostics": _diagnostics(rows, smoothed),
    }
    validate_recorded_subject_route(route, scenario=scenario)
    return route


def validate_recorded_subject_route(route: dict, *, scenario: Scenario,
                                     expected_replay_sha256=None) -> dict:
    if (not isinstance(route, dict) or route.get("schema") != "dm-gta-subject-route/1"
            or route.get("route_kind") != RECORDED_ROUTE_KIND
            or route.get("control") != scenario.raw["player"]["control"]
            or route.get("native_command") != native_command(scenario.raw["player"])
            or route.get("duration_s") != scenario.duration_s):
        raise ContractError("recorded route native command/duration differs from scenario")
    if not all(_finite(route["native_command"].get(key)) for key in
               ("distance_m", "speed_mps", "heading_deg")):
        raise ContractError("recorded route native command parameters must be finite numbers")
    observation = route.get("observation")
    if (not isinstance(observation, dict) or observation.get("schema") != OBSERVATION_SCHEMA
            or observation.get("method") != REAL_PLAYER_OBSERVATION_METHOD
            or observation.get("player_replay_origin_verified") is not False):
        raise ContractError("recorded route must retain its unverified replay-origin limitation")
    original, master, rows = _load_observations(
        observation.get("files"), expected_replay_sha256=expected_replay_sha256)
    if (original.raw != scenario.raw
            or observation.get("source_clip_identity") != master["source_clip_identity"]
            or observation.get("selection") != {"start_frame": 0, "n_frames": len(rows)}):
        raise ContractError("recorded route scenario/selection/identity differs from its recording")
    smoothing = observation.get("smoothing")
    if not isinstance(smoothing, dict) or smoothing.get("method") != SMOOTHING_METHOD:
        raise ContractError("recorded route smoothing method is unsupported")
    bound = smoothing.get("max_residual_m")
    height = observation.get("look_at_height_m")
    if (not _finite(bound) or not 0 < bound <= MAX_FIT_RESIDUAL_M
            or not _finite(height) or not 0 <= height <= 2.5):
        raise ContractError("recorded route fit residual/aim height exceeds the supported bound")
    smoothed = smooth_recorded_positions(
        [row["player_state"]["position"] for row in rows],
        window_frames=smoothing.get("window_frames"), passes=smoothing.get("passes"))
    diagnostics = _diagnostics(rows, smoothed)
    if diagnostics["fit_max_residual_m"] > bound:
        raise ContractError("recorded route smoothing exceeds its declared fit residual")
    waypoints = route.get("waypoints")
    if not isinstance(waypoints, list) or len(waypoints) != len(rows):
        raise ContractError("recorded route must cover every master observation")
    for frame, (waypoint, expected) in enumerate(zip(waypoints, smoothed)):
        if (not isinstance(waypoint, dict) or not _finite(waypoint.get("t"))
                or abs(waypoint["t"] - frame / 30) > 1e-8
                or not _vector(waypoint.get("position"))
                or math.dist(waypoint["position"], expected) > 1e-5
                or waypoint.get("look_at_height_m") != height):
            raise ContractError(f"recorded route waypoint {frame} differs from recomputed telemetry fit")
    declared = route.get("diagnostics")
    if not isinstance(declared, dict) or declared.get("player_replay_origin_verified") is not False:
        raise ContractError("recorded route diagnostics are missing or overclaim replay alignment")
    for key, value in diagnostics.items():
        if key == "player_replay_origin_verified":
            continue
        if not _finite(declared.get(key)) or abs(declared[key] - value) > 1e-8:
            raise ContractError("recorded route diagnostics differ from measured telemetry")
    return diagnostics
