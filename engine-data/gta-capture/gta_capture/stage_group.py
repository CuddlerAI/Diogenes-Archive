"""A four-worldline group: one stage, three takes (AB, C, D), and the gate that
says whether the worldlines agree wherever nobody intervened.

Layout of a group directory:

    group.json                     the group's settings (dm-gta-stage-group/1)
    stage-spec.txt                 the one stage every take plays
    takes/AB/config/scenario.json  + stage-spec.txt
    takes/AB/evidence/stage-take-result.json, takes/AB/source/...
    takes/C/config/...             + stage-calibration.txt, copied from AB's
    takes/D/config/...             + stage-calibration.txt, copied from AB's
    evidence/cross-worldline.json  the gate's report

Worldline B is the free camera flown during take AB: it shares AB's replay,
so B replays A frame for frame by construction.
"""

from __future__ import annotations

import json
import math
import shutil
from pathlib import Path
from typing import Any

from .errors import ContractError
from .jsonio import load_json, sha256_file, write_json_atomic
from .stage_take import (TAKE_KINDS, TAKE_RESULT, StageScenario, build_take_scenario, check_take_settings,
                         read_spec_header)

GROUP_SCHEMA = "dm-gta-stage-group/1"
GATE_SCHEMA = "dm-gta-cross-worldline/1"
# Rehearsals v4/v5: untouched actors agree to one float32 ULP (0.122 mm at the
# beach's |x|~1573). A deviation above this is a real divergence, not noise.
EXACT_M = 0.00025
# The customer-facing tolerances docs/170 proposed, reported alongside.
TOLERANCE_POS_M, TOLERANCE_BONE_M = 0.01, 0.02
# A blended change of clip runs on real time: for this many ticks after it the
# pose may differ slightly between takes (the position does not -- the mover
# snaps). The customer accepts that (#958, 2026-09-23): inside these windows a
# pose difference is reported, not failed; positions are held to EXACT_M still.
# The stage spec refuses blends slower than a quarter second (blend-in speed 4,
# 7.5 ticks), so the window covers every blend with room to spare.
BLEND_WINDOW_TICKS = 20
SLOWEST_BLEND = 4.0


def _group_config(value: Any) -> dict:
    if not isinstance(value, dict) or value.get("schema") != GROUP_SCHEMA:
        raise ContractError(f"group config must be {GROUP_SCHEMA}")
    for key in ("group_id", "stage_spec", "duration_s", "environment", "takeover", "seventh"):
        if key not in value:
            raise ContractError(f"group config is missing {key}")
    env = value["environment"]
    for key in ("location", "clock", "weather", "source_epoch"):
        if key not in env:
            raise ContractError(f"group environment is missing {key}")
    # A calibration the group brings (measured in a rehearsal, where the clips' lifts were
    # set by eye): take AB replays it instead of measuring its own. Two measurements of
    # the same clips differ by up to 6 mm (the standing reference reads 6.5 or 7.1 cm on
    # the same model, beach-demo-003 2026-09-29), more than a lift is set to.
    if "stage_calibration" in value:
        name = value["stage_calibration"]
        if not isinstance(name, str) or not name or Path(name).name != name:
            raise ContractError("group stage_calibration must be a file name in the group directory")
    return value


def prepare_group(config_path, out_dir) -> Path:
    """Starts a group: the stage and the first take's configuration."""
    config_path = Path(config_path).resolve()
    group = _group_config(load_json(config_path))
    out = Path(out_dir).resolve()
    if (out / "group.json").exists():
        raise ContractError("the group directory already holds a group")
    spec_src = (config_path.parent / group["stage_spec"]).resolve()
    header = read_spec_header(spec_src.read_text(encoding="utf-8"))   # refuses a malformed stage early
    # C's and D's own settings too: a bad one must surface before anybody plays AB
    for kind in TAKE_KINDS:
        check_take_settings(kind, build_take_scenario(group=group, kind=kind, spec_name="stage-spec.txt",
                                                      spec_sha256="0" * 64)["take"], header)
    out.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(spec_src, out / "stage-spec.txt")
    brought = {}
    if group.get("stage_calibration"):
        cal_src = (config_path.parent / group["stage_calibration"]).resolve()
        if not cal_src.is_file():
            raise ContractError(f"group stage_calibration {group['stage_calibration']} is missing")
        shutil.copyfile(cal_src, out / "stage-calibration.txt")
        brought = {"stage_calibration": "stage-calibration.txt",
                   "stage_calibration_sha256": sha256_file(out / "stage-calibration.txt")}
    write_json_atomic(out / "group.json", {**group, "stage_spec": "stage-spec.txt",
                                           "stage_spec_sha256": sha256_file(out / "stage-spec.txt"), **brought})
    prepare_take(out, "AB")
    return out


def _take_dir(group_dir: Path, kind: str) -> Path:
    if kind not in TAKE_KINDS:
        raise ContractError(f"unknown take {kind!r}")
    return group_dir / "takes" / kind


def take_result(group_dir: Path, kind: str) -> dict:
    path = _take_dir(group_dir, kind) / "evidence" / "stage-take-result.json"
    if not path.is_file():
        raise ContractError(f"take {kind} has not been recorded")
    result = load_json(path)
    if result.get("ok") is not True or (result.get("take") or {}).get("schema") != TAKE_RESULT:
        raise ContractError(f"take {kind} did not complete")
    return result["take"]


def prepare_take(group_dir, kind: str) -> Path:
    """Writes one take's configuration. C and D replay the calibration of AB: the one AB
    measured, or the one the group brought and AB replayed."""
    group_dir = Path(group_dir).resolve()
    group = load_json(group_dir / "group.json")
    config = _take_dir(group_dir, kind) / "config"
    if (config / "scenario.json").exists():
        raise ContractError(f"take {kind} is already prepared")
    config.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(group_dir / "stage-spec.txt", config / "stage-spec.txt")
    spec_sha = sha256_file(config / "stage-spec.txt")
    if spec_sha != group["stage_spec_sha256"]:
        raise ContractError("the group's stage spec changed after the group was prepared")
    cal_name = cal_sha = None
    if kind == "AB" and group.get("stage_calibration"):
        brought = group_dir / group["stage_calibration"]
        if not brought.is_file() or sha256_file(brought) != group.get("stage_calibration_sha256"):
            raise ContractError("the group's stage calibration is missing or changed after the group named it")
        shutil.copyfile(brought, config / "stage-calibration.txt")
        cal_name, cal_sha = "stage-calibration.txt", group["stage_calibration_sha256"]
    elif kind != "AB":
        ab = take_result(group_dir, "AB")
        measured = Path(ab["stage_calibration_path"])
        if not measured.is_file():
            # the result names the path on the capture machine; the group keeps its own copy
            measured = _take_dir(group_dir, "AB") / "source" / "stage-calibration.txt"
        if sha256_file(measured) != ab["stage_calibration_sha256"]:
            raise ContractError("take AB's calibration file does not match its result")
        shutil.copyfile(measured, config / "stage-calibration.txt")
        cal_name, cal_sha = "stage-calibration.txt", ab["stage_calibration_sha256"]
    scenario = build_take_scenario(group=group, kind=kind, spec_name="stage-spec.txt", spec_sha256=spec_sha,
                                   calibration_name=cal_name, calibration_sha256=cal_sha)
    # What the native side will read must pass the contract here first -- checked
    # under another name, so a refused scenario leaves no "already prepared" behind.
    path = config / "scenario.json"
    trial = config / "scenario.check.json"
    write_json_atomic(trial, scenario)
    try:
        StageScenario.load(trial)
    except Exception:
        trial.unlink(missing_ok=True)
        raise
    trial.replace(path)
    return path


# ------------------------------------------------------------------ the gate

def _dist(a, b) -> float:
    """Distance between two points; a coordinate that is not a finite number is infinitely far."""
    d = math.sqrt(sum((float(x) - float(y)) ** 2 for x, y in zip(a, b)))
    return d if math.isfinite(d) else math.inf


def load_world(path: Path) -> dict[int, dict]:
    ticks: dict[int, dict] = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            row = json.loads(line)
            if row.get("schema") != "dm-gta-world-telemetry/1":
                raise ContractError(f"{path.name}: not dm-gta-world-telemetry/1")
            k = int(row["tick"])
            if k in ticks:
                raise ContractError(f"{path.name}: tick {k} is written twice")
            actors = {a["id"]: a for a in row["actors"]}
            props = {p["id"]: p for p in row["props"]}
            ticks[k] = {"held": int(row["held"]), "actors": actors, "props": props}
    return ticks


def released_ticks(world: dict[int, dict]) -> dict[str, int]:
    """First tick each actor shows as released in the world telemetry -- by a rule,
    a takeover, or the runtime itself (a scene the pool refused)."""
    first: dict[str, int] = {}
    for k in sorted(world):
        for aid, a in world[k]["actors"].items():
            if a.get("released") and aid not in first:
                first[aid] = k
    return first


def load_exclusions(path: Path) -> tuple[dict[str, int], list[dict]]:
    """First tick each actor stopped following the plan, from the causal record."""
    first: dict[str, int] = {}
    events: list[dict] = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            e = json.loads(line)
            if e.get("schema") != "dm-gta-rule-events/1":
                raise ContractError(f"{path.name}: not dm-gta-rule-events/1")
            events.append(e)
            for a in e["affected"]:
                first[a["actor"]] = min(first.get(a["actor"], 10**9), int(e["tick"]))
    return first, events


def load_blend_windows(plan_path: Path) -> dict[str, set[int]]:
    """Per actor, the ticks just after each blended change of clip, from the rendered plan.
    A beat line is `<kind> <clip> start <S> ticks <T> ... blend <B>`: read by
    position, since a clip may be named anything -- "start" or "blend" included."""
    windows: dict[str, set[int]] = {}
    actor = None
    with open(plan_path, encoding="utf-8") as f:
        for line in f:
            w = line.split()
            if w[:1] == ["actor"] and len(w) >= 2:
                actor = w[1]
                windows.setdefault(actor, set())
            elif actor and len(w) >= 6 and w[0] in ("idle", "walk", "once", "pair") and w[2] == "start":
                if w[-2] != "blend":
                    raise ContractError(f"{plan_path.name}: a beat line does not end in its blend: {line.strip()}")
                start, blend = int(w[3]), float(w[-1])
                if 0 < blend < SLOWEST_BLEND:
                    raise ContractError(f"{plan_path.name}: a blend slower than the gate's window ({blend})")
                if start > 0 and blend > 0:
                    windows[actor].update(range(start, start + BLEND_WINDOW_TICKS))
    return windows


def _entry() -> dict:
    return {"ticks": 0, "missing_ticks": 0, "pos_m": 0.0, "bone_m": 0.0, "bone_in_blend_m": 0.0, "first_over_tick": None}


def compare(ref: dict[int, dict], other: dict[int, dict], *, exclude: dict[str, int] | None = None,
            until: int | None = None, blend_windows: dict[str, set[int]] | None = None,
            expect: tuple[tuple[str, ...], tuple[str, ...]] | None = None) -> dict[str, dict]:
    """Per actor and prop: worst position and bone deviation over the ticks both
    sampled, up to the tick it was excluded from (a prop, with its owner). A
    pose difference just after a blended change is kept apart (bone_in_blend_m).

    `expect` names every actor and prop that must be there: one missing from a
    tick either take sampled -- absent, or present without a state -- counts as
    a missing tick, and a missing tick is not exact. Nothing absent passes."""
    exclude = exclude or {}
    blend_windows = blend_windows or {}
    out: dict[str, dict] = {}
    actor_ids, prop_ids = expect if expect is not None else ((), ())
    for eid in (*actor_ids, *prop_ids):
        out[eid] = _entry()
    for k in sorted(set(ref) & set(other)):
        if until is not None and k >= until:
            break
        a, b = ref[k], other[k]
        for aid in sorted(set(actor_ids) | set(a["actors"]) | set(b["actors"])):
            if k >= exclude.get(aid, 10**9):
                continue
            ra, rb = a["actors"].get(aid), b["actors"].get(aid)
            sa, sb = (ra or {}).get("state"), (rb or {}).get("state")
            d = out.setdefault(aid, _entry())
            if not sa or not sb:
                d["missing_ticks"] += 1
                continue
            dp = _dist(sa["pos"], sb["pos"])
            db = max((_dist(x, y) for x, y in zip(sa["bones"], sb["bones"])), default=math.inf)
            in_blend = k in blend_windows.get(aid, ())
            d["ticks"] += 1
            d["pos_m"] = max(d["pos_m"], dp)
            if in_blend:
                d["bone_in_blend_m"] = max(d["bone_in_blend_m"], db)
            else:
                d["bone_m"] = max(d["bone_m"], db)
            if max(dp, 0.0 if in_blend else db) > EXACT_M and d["first_over_tick"] is None:
                d["first_over_tick"] = k
        # props: only on ticks whose pose was up for two frames or more (a prop
        # trails its hand by a frame), and only while their owner follows the plan
        if a["held"] < 2 or b["held"] < 2:
            continue
        for pid in sorted(set(prop_ids) | set(a["props"]) | set(b["props"])):
            owner = pid.split(":", 1)[0]
            if k >= exclude.get(owner, 10**9):
                continue
            pa, pb = a["props"].get(pid), b["props"].get(pid)
            sa, sb = (pa or {}).get("state"), (pb or {}).get("state")
            d = out.setdefault(pid, _entry())
            if not sa or not sb:
                d["missing_ticks"] += 1
                continue
            dp = _dist(sa["pos"], sb["pos"])
            d["ticks"] += 1
            if k in blend_windows.get(owner, ()):   # a prop follows its owner's hand through the blend
                d["bone_in_blend_m"] = max(d["bone_in_blend_m"], dp)
                continue
            d["pos_m"] = max(d["pos_m"], dp)
            d["bone_m"] = max(d["bone_m"], dp)
            if dp > EXACT_M and d["first_over_tick"] is None:
                d["first_over_tick"] = k
    for d in out.values():
        whole = d["missing_ticks"] == 0
        d["exact"] = whole and max(d["pos_m"], d["bone_m"]) <= EXACT_M
        d["within_tolerance"] = whole and d["pos_m"] <= TOLERANCE_POS_M and d["bone_m"] <= TOLERANCE_BONE_M
    return out


# The files each take's result sealed, under the group's own copy of the take.
_SEALED = (("master-world-telemetry.jsonl", "world_telemetry_sha256"), ("rule-events.jsonl", "rule_events_sha256"),
           ("input-camera-log.jsonl", "input_log_sha256"), ("stage-plan.txt", "stage_plan_sha256"),
           ("stage-calibration.txt", "stage_calibration_sha256"))


def cross_worldline_gate(group_dir) -> dict:
    group_dir = Path(group_dir).resolve()
    header = read_spec_header((group_dir / "stage-spec.txt").read_text(encoding="utf-8"))
    expect = (header.actors, tuple(f"{actor}:{model}" for actor, model in header.props))
    world, excl, events, results = {}, {}, {}, {}
    problems: list[str] = []
    for kind in TAKE_KINDS:
        results[kind] = take_result(group_dir, kind)
        src = _take_dir(group_dir, kind) / "source"
        # the files are the ones the take sealed, whole
        for name, key in _SEALED:
            if not (src / name).is_file():
                problems.append(f"take {kind}: {name} is missing")
            elif sha256_file(src / name) != results[kind].get(key):
                problems.append(f"take {kind}: {name} is not the file the take sealed")
        world[kind] = load_world(src / "master-world-telemetry.jsonl")
        excl[kind], events[kind] = load_exclusions(src / "rule-events.jsonl")
        sampled = results[kind].get("ticks_sampled")
        if len(world[kind]) != sampled:
            problems.append(f"take {kind}: {len(world[kind])} ticks of world telemetry for {sampled} sampled")
        if any(not 0 <= k < header.ticks for k in world[kind]):
            problems.append(f"take {kind}: world telemetry outside the stage's ticks")
        # every release is on the causal record: a rule, the takeover or the seventh
        # person -- the runtime letting an actor go on its own (a scene the pool
        # refused) would stand it still in every take alike, and pass
        for actor, tick in released_ticks(world[kind]).items():
            if tick < excl[kind].get(actor, 10**9):
                problems.append(f"take {kind}: {actor} was released at tick {tick} with nothing on the causal record")
    plans = {k: results[k]["stage_plan_sha256"] for k in TAKE_KINDS}
    cals = {k: results[k]["stage_calibration_sha256"] for k in TAKE_KINDS}
    report: dict[str, Any] = {
        "schema": GATE_SCHEMA,
        "exact_m": EXACT_M,
        "tolerance": {"position_m": TOLERANCE_POS_M, "bone_m": TOLERANCE_BONE_M},
        "same_plan": len(set(plans.values())) == 1,
        "same_calibration": len(set(cals.values())) == 1,
        "a_b": {"rule": "same replay: B is the free camera flown in take AB", "clips": [c["sha256"] for c in results["AB"]["clips"]]},
    }
    takeover = results["C"]["takeover"]["tick"]
    windows = load_blend_windows(_take_dir(group_dir, "AB") / "source" / "stage-plan.txt")
    report["blend_window_ticks"] = BLEND_WINDOW_TICKS
    report["common_ticks"] = {k: len(set(world["AB"]) & set(world[k])) for k in ("C", "D")}
    report["a_c_before_takeover"] = compare(world["AB"], world["C"], until=takeover, blend_windows=windows, expect=expect)
    report["a_c_untouched"] = compare(world["AB"], world["C"], exclude=excl["C"], blend_windows=windows, expect=expect)
    report["a_d_untouched"] = compare(world["AB"], world["D"], exclude=excl["D"], blend_windows=windows, expect=expect)
    # A must itself be untouched: nobody plays a person in take AB
    report["a_released"] = sorted(set(excl["AB"]) | set(released_ticks(world["AB"])))
    report["affected"] = {k: {a: t for a, t in excl[k].items()} for k in ("C", "D")}
    report["causal_record"] = {k: [{"t_s": e["t_s"], "rule": e["rule"], "say": e["say"],
                                    "affected": e["affected"]} for e in events[k]] for k in ("C", "D")}
    sections = ("a_c_before_takeover", "a_c_untouched", "a_d_untouched")
    report["problems"] = problems
    whole = report["same_plan"] and report["same_calibration"] and not report["a_released"] and not problems
    report["exact"] = whole and all(d["exact"] for s in sections for d in report[s].values())
    report["within_tolerance"] = whole and all(d["within_tolerance"] for s in sections for d in report[s].values())
    write_json_atomic(group_dir / "evidence" / "cross-worldline.json", report)
    return report
