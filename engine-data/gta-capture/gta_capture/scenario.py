"""Scenario contract loaded by both the orchestrator and the native companion."""

from __future__ import annotations

import math
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .errors import ContractError
from .jsonio import load_json, sha256_json


SCENARIO_SCHEMA = "dm-gta-scenario/1"


@dataclass(frozen=True)
class Scenario:
    path: Path
    raw: dict

    @classmethod
    def load(cls, path) -> "Scenario":
        source = Path(path)
        value = load_json(source)
        validate_scenario(value)
        return cls(source.resolve(), value)

    @property
    def scenario_id(self) -> str:
        return self.raw["scenario_id"]

    @property
    def duration_s(self) -> float:
        return float(self.raw["replay"]["duration_s"])

    @property
    def fingerprint(self) -> str:
        return sha256_json(self.raw)


def _vec3(value: Any, label: str) -> None:
    if (not isinstance(value, list) or len(value) != 3
            or any(not isinstance(x, (int, float)) or isinstance(x, bool)
                   or not math.isfinite(float(x)) for x in value)):
        raise ContractError(f"{label} must be a numeric vec3")


def validate_native_subject_route(route: Any, *, player: dict, anchor: list,
                                  duration_s: float, n_frames: int) -> None:
    """Check the legacy authored constant-speed route's internal consistency.

    Route declarations alone cannot bind a camera plan to the Story player: a
    curved or stationary anchor track could carry the same three scalar fields.
    Check every emitted world-space waypoint against the authored straight-route
    assumption. The native task's speed parameter does not guarantee physical
    metres per second. Actual moving-camera runs use recorded_route instead;
    this check proves only that the old planned geometry is self-consistent.
    """
    if (not isinstance(route, dict)
            or route.get("schema") != "dm-gta-subject-route/1"
            or route.get("control") != player["control"]
            or route.get("route_kind") != player["route_kind"]):
        raise ContractError("placed subject route requires the implemented native straight task")
    for key in ("distance_m", "speed_mps", "heading_deg"):
        item = route.get(key)
        if (not isinstance(item, (int, float)) or isinstance(item, bool)
                or not math.isfinite(float(item))
                or abs(float(item) - float(player[key])) > 1e-9):
            raise ContractError(f"placed subject route {key} differs from the scenario player")
    route_duration = route.get("duration_s")
    if (not isinstance(route_duration, (int, float)) or isinstance(route_duration, bool)
            or not math.isfinite(float(route_duration))
            or abs(float(route_duration) - duration_s) > 1e-9):
        raise ContractError("placed subject route duration differs from the scenario")
    if (not isinstance(n_frames, int) or isinstance(n_frames, bool) or n_frames <= 0
            or abs(n_frames - duration_s * 30) > 1e-6):
        raise ContractError("placed subject route requires an exact 30fps timeline")
    _vec3(anchor, "placed subject route anchor")
    waypoints = route.get("waypoints")
    if not isinstance(waypoints, list) or len(waypoints) != n_frames:
        raise ContractError("placed subject route must have one waypoint per camera frame")
    heading = math.radians(float(player["heading_deg"]))
    forward = (-math.sin(heading), math.cos(heading), 0.0)
    for frame, waypoint in enumerate(waypoints):
        if not isinstance(waypoint, dict):
            raise ContractError(f"placed subject route waypoint {frame} is malformed")
        t = waypoint.get("t")
        if (not isinstance(t, (int, float)) or isinstance(t, bool)
                or not math.isfinite(float(t)) or abs(float(t) - frame / 30) > 1e-8):
            raise ContractError(f"placed subject route waypoint {frame} has a different timeline")
        position = waypoint.get("position")
        _vec3(position, f"placed subject route waypoint {frame} position")
        travelled = min(float(player["distance_m"]), float(player["speed_mps"]) * frame / 30)
        expected = [float(anchor[axis]) + forward[axis] * travelled for axis in range(3)]
        if math.dist(position, expected) > 1e-5:
            raise ContractError(
                f"placed subject route waypoint {frame} differs from the native straight route")
        aim_height = waypoint.get("look_at_height_m")
        if (not isinstance(aim_height, (int, float)) or isinstance(aim_height, bool)
                or not math.isfinite(float(aim_height)) or float(aim_height) < 0):
            raise ContractError(f"placed subject route waypoint {frame} has invalid aim height")


def validate_scenario(value: Any) -> None:
    if not isinstance(value, dict) or value.get("schema") != SCENARIO_SCHEMA:
        raise ContractError(f"scenario.schema must equal {SCENARIO_SCHEMA!r}")
    scenario_id = value.get("scenario_id")
    if not isinstance(scenario_id, str) or not scenario_id.strip():
        raise ContractError("scenario.scenario_id must be a non-empty string")
    if value.get("mode") != "story_mode":
        raise ContractError("scenario.mode must be story_mode")
    if value.get("radio") != "off":
        raise ContractError("scenario.radio must be off")
    # The native side hard-requires a non-empty GTA weather type; rejecting it
    # here keeps dry-run and real capture on the same contract.
    weather = value.get("weather")
    if (not isinstance(weather, str) or not weather
            or not all(ch == "_" or "A" <= ch <= "Z" for ch in weather)):
        raise ContractError("scenario.weather must be an uppercase GTA weather type")
    seed = value.get("seed")
    if (not isinstance(seed, int) or isinstance(seed, bool)
            or seed < 0 or seed > 9_007_199_254_740_991):
        raise ContractError("scenario.seed must be an exactly representable non-negative integer")
    _vec3(value.get("location"), "scenario.location")
    clock = value.get("clock")
    if (not isinstance(clock, str) or len(clock) != 5 or clock[2] != ":"
            or not clock[:2].isdigit() or not clock[3:].isdigit()
            or not 0 <= int(clock[:2]) <= 23 or not 0 <= int(clock[3:]) <= 59):
        raise ContractError("scenario.clock must use HH:MM")
    source_epoch = value.get("source_epoch")
    if not isinstance(source_epoch, str) or not source_epoch.strip():
        raise ContractError("scenario.source_epoch must be a non-empty string")
    # Fail at validation time, not four exported lanes later in metadata.
    try:
        datetime.strptime(source_epoch, "%Y-%m-%d %H:%M:%S.%f")
    except ValueError as exc:
        raise ContractError("scenario.source_epoch must use YYYY-MM-DD HH:MM:SS.mmm") from exc
    player = value.get("player")
    if not isinstance(player, dict):
        raise ContractError("scenario.player must be an object")
    expected_strings = {
        "model": "current_story_player",
        "control": "native_task_go_straight",
        "route_kind": "forward_from_anchor",
    }
    for key, expected in expected_strings.items():
        if player.get(key) != expected:
            raise ContractError(f"scenario.player.{key} must equal {expected!r}")
    for key in ("distance_m", "speed_mps", "heading_deg"):
        item = player.get(key)
        if (not isinstance(item, (int, float)) or isinstance(item, bool)
                or not math.isfinite(float(item))):
            raise ContractError(f"scenario.player.{key} must be finite")
    if not 0 < float(player["distance_m"]) <= 100:
        raise ContractError("scenario.player.distance_m is outside the supported POC range")
    if not 0 < float(player["speed_mps"]) <= 10:
        raise ContractError("scenario.player.speed_mps is outside the supported POC range")
    if not 0 <= float(player["heading_deg"]) < 360:
        raise ContractError("scenario.player.heading_deg is outside the supported POC range")
    replay = value.get("replay")
    if not isinstance(replay, dict):
        raise ContractError("scenario.replay must be an object")
    duration = replay.get("duration_s")
    warmup = replay.get("warmup_s")
    if (not isinstance(duration, (int, float)) or isinstance(duration, bool)
            or not math.isfinite(float(duration)) or duration <= 0):
        raise ContractError("scenario.replay.duration_s must be positive")
    if (not isinstance(warmup, (int, float)) or isinstance(warmup, bool)
            or not math.isfinite(float(warmup)) or warmup < 0):
        raise ContractError("scenario.replay.warmup_s must be non-negative")
    if replay.get("fps") != 30:
        raise ContractError("scenario.replay.fps must be 30")
    exclusions = value.get("effect_exclusions")
    if not isinstance(exclusions, list) or not all(isinstance(x, str) for x in exclusions):
        raise ContractError("scenario.effect_exclusions must be a string array")
    population = value.get("population")
    if population is None:
        population_mode = "deterministic_empty"
    elif not isinstance(population, dict):
        raise ContractError("scenario.population must be an object")
    else:
        population_mode = population.get("population_mode")
        if population_mode != "ambient_observation_only":
            raise ContractError(
                "scenario.population.population_mode must equal "
                "'ambient_observation_only'")
        for key in ("ped_density", "scenario_ped_density", "vehicle_density",
                    "random_vehicle_density", "parked_vehicle_density"):
            item = population.get(key)
            if (not isinstance(item, (int, float)) or isinstance(item, bool)
                    or not math.isfinite(float(item)) or not 0.0 <= float(item) <= 1.0):
                raise ContractError(f"scenario.population.{key} must be within [0,1]")
        if (float(population["ped_density"]) < 0.75
                or float(population["scenario_ped_density"]) < 0.75
                or float(population["vehicle_density"]) < 0.75
                or float(population["random_vehicle_density"]) < 0.75):
            raise ContractError(
                "ambient_observation_only requires busy pedestrian/vehicle densities >=0.75")

    required_exclusions = {"radio", "online", "loading_transition", "police_dispatch"}
    if population_mode == "deterministic_empty":
        required_exclusions.update({"random_peds", "random_vehicles"})
    elif {"random_peds", "random_vehicles"} & set(exclusions):
        raise ContractError(
            "ambient_observation_only cannot exclude random_peds/random_vehicles")
    missing = required_exclusions - set(exclusions)
    if missing:
        raise ContractError("scenario.effect_exclusions missing: " + ", ".join(sorted(missing)))
    dynamics = value.get("dynamics")
    if not isinstance(dynamics, list) or len(dynamics) != 1:
        raise ContractError("scenario.dynamics must contain exactly the controlled Story player")
    for i, actor in enumerate(dynamics):
        if not isinstance(actor, dict) or not isinstance(actor.get("id"), str):
            raise ContractError(f"scenario.dynamics[{i}] must have an id")
        if (actor.get("type") != "humanoid"
                or actor.get("control") != "native_task_go_straight"
                or actor.get("route") != "player"):
            raise ContractError(f"scenario.dynamics[{i}] is outside the implemented POC contract")
