"""Operator CLI for dry-run now and GTA Story Mode bring-up later."""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

from . import __version__
from .backend import DryRunBackend, PipeBackend
from .audio import (normalize_replus_wav, radio_off_evidence_valid,
                    validate_wav)
from .errors import GtaCaptureError
from .jsonio import load_json, sha256_file, write_json_atomic
from .native_tape import write_native_tape
from .orchestrator import load_run_summary, run_capture
from .preflight import (DependencyManifest, load_render_profile_binding,
                        run_preflight)
from .protocol import DEFAULT_PIPE, JsonLinePipeClient
from .qc_bridge import package_run, run_engine_qc
from .scenario import Scenario
from .stage_audio import clean_take_audio
from .stage_check import check_stage_script, dump_plan_world
from .stage_delivery import deliver_group, parse_time_anchors
from .stage_group import cross_worldline_gate, prepare_group, prepare_take, take_result
from .stage_export import (check_game_settings, check_lane_pixels, default_game_settings, encode_lane, encode_quad,
                           load_lane_result, render_profile, take_export_inputs)
from .stage_take import StageScenario, check_take_result
from .stage_tape import build_take_tape
from .staged import finalize_staged_poc
from .tape import (GTA_COORDINATE_CONVENTION, import_trajgen_clip, load_tape, make_demo_tape,
                   make_gta_poc_tape, write_tape)
from .calibration import build_projection_evidence, load_samples
from .traj_adapter import Placement, place_trajgen_clip
from .story_projection import capture_sample


def _print(value) -> None:
    print(json.dumps(value, ensure_ascii=False, indent=2))


def _require_live_record_binding(probe: dict, expected_replay_sha256: str) -> None:
    """Reject an Editor export unless this live controller still owns the master."""
    generation = probe.get("controller_generation")
    if (not isinstance(generation, int) or isinstance(generation, bool)
            or generation < 1):
        raise GtaCaptureError("native companion did not report a valid controller generation")
    if probe.get("record_binding_ready") is not True:
        raise GtaCaptureError(
            "native companion lost the master binding after the ScriptHook relaunch")
    if probe.get("master_replay_sha256") != expected_replay_sha256:
        raise GtaCaptureError(
            "live controller master binding differs from the requested sealed replay")


def _cmd_make_demo_tape(args) -> int:
    tape = make_demo_tape(duration_s=args.duration, width=args.width, height=args.height,
                          filmback_id=args.filmback_id, clip_id=args.clip_id)
    write_tape(args.out, tape)
    _print({"ok": True, "path": str(Path(args.out).resolve()), "n_frames": tape["n_frames"]})
    return 0


def _cmd_import_trajgen(args) -> int:
    tape = import_trajgen_clip(args.clip, filmback_id=args.filmback_id,
                               clip_id=args.clip_id)
    write_tape(args.out, tape)
    _print({"ok": True, "path": str(Path(args.out).resolve()), "n_frames": tape["n_frames"]})
    return 0


def _cmd_solve_calibration(args) -> int:
    """Solve measured correspondences into approved-shaped projection evidence.

    The output still has to be pinned by path+SHA-256 in the dependency manifest
    and re-verified by preflight; emitting it here only removes the incentive to
    hand-write the file that docs/158 §11 P0 forbids.
    """
    document = load_samples(args.samples)
    evidence = build_projection_evidence(
        document, calibration_id=args.calibration_id, filmback_id=args.filmback_id,
        metric_scale=args.metric_scale, approval_status=args.approval_status)
    path = write_json_atomic(args.out, evidence)
    reprojection = evidence["reprojection"]
    _print({"ok": True, "path": str(Path(path).resolve()),
            "approval_status": evidence["approval_status"],
            "rms_px": reprojection["error_px"], "max_px": reprojection["max_px"],
            "sample_count": reprojection["sample_count"],
            "filmback_mm": evidence["solution"]["filmback_mm"],
            "filmback_spread_mm": evidence["solution"]["filmback_spread_mm"],
            "per_focal": [{"focal_mm": item["focal_mm"], "fx": item["fx"],
                           "fy": item["fy"], "filmback_mm": item["filmback_mm"],
                           "rms_px": item["rms_px"]}
                          for item in evidence["solution"]["per_focal"]],
            "next": "pin this path and its sha256 in the dependency manifest "
                    "calibration.projection_evidence, then rerun preflight --real"})
    return 0


def _cmd_place_trajgen(args) -> int:
    """Pin a trajgen clip to a surveyed Story Mode anchor (docs/158 §5.3).

    Unlike ``import-trajgen`` this emits a tape in GTA world coordinates, so it is
    the only trajgen path the real orchestrator will accept.  The subject route is
    written beside the tape because the controlled character must walk the same
    rigid image of the plan the cameras were aimed at.
    """
    tape, route = place_trajgen_clip(
        args.clip,
        placement=Placement(anchor=(args.anchor_x, args.anchor_y, args.anchor_z),
                            heading_deg=args.heading_deg, ground_y=args.ground_y),
        filmback_id=args.filmback_id, clip_id=args.clip_id)
    write_tape(args.out, tape)
    route_path = Path(args.subject_route) if args.subject_route else \
        Path(args.out).with_name("subject-route.json")
    write_json_atomic(route_path, route)
    _print({"ok": True, "path": str(Path(args.out).resolve()),
            "subject_route": str(route_path.resolve()),
            "n_frames": tape["n_frames"], "duration_s": tape["duration_s"],
            "cam4_mode": tape["source"]["cam4_mode"],
            "rigidity_residual_m": tape["source"]["rigidity_residual_m"],
            "coordinate_convention": tape["coordinate_convention"],
            "delivery_eligible": tape["source"]["delivery_eligible"]})
    return 0


def _cmd_make_gta_poc_tape(args) -> int:
    tape = make_gta_poc_tape(
        anchor=[args.anchor_x, args.anchor_y, args.anchor_z],
        duration_s=args.duration, width=args.width, height=args.height,
        filmback_id=args.filmback_id, clip_id=args.clip_id,
        route_distance_m=args.route_distance_m,
        route_speed_mps=args.route_speed_mps,
        route_heading_deg=args.route_heading_deg,
        lane_lateral_offsets_m=args.lane_lateral_offsets)
    write_tape(args.out, tape)
    _print({"ok": True, "path": str(Path(args.out).resolve()),
            "n_frames": tape["n_frames"],
            "coordinate_convention": tape["coordinate_convention"]})
    return 0


def _cmd_bind_poc_scenario(args) -> int:
    template = Scenario.load(args.template)
    value = json.loads(json.dumps(template.raw))
    value["scenario_id"] = args.scenario_id
    value["location"] = [args.anchor_x, args.anchor_y, args.anchor_z]
    Scenario.load(write_json_atomic(args.out, value))
    _print({"ok": True, "path": str(Path(args.out).resolve()),
            "scenario_id": args.scenario_id, "location": value["location"]})
    return 0


def _cmd_probe_game(args) -> int:
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        probe = client.request("probe_state")
    finally:
        client.close()
    _print({"ok": True, "hello": hello, "probe": probe})
    return 0


def _cmd_story_projection_sample(args) -> int:
    result = capture_sample(Path(args.request), Path(args.out_dir), pipe=args.pipe)
    _print({"ok": True, "observation": str(Path(args.out_dir).resolve() / "observation.json"),
            "png": result["output_path"], "approval_status": "draft",
            "editor_equivalence_verified": False})
    return 0


def _cmd_native_record_smoke(args) -> int:
    scenario = Scenario.load(args.scenario)
    run_dir = Path(args.run_dir).resolve()
    run_dir.mkdir(parents=True, exist_ok=True)
    result_path = run_dir / "evidence" / "native-record-result.json"
    if result_path.exists() or (run_dir / "source" / "master-replay.clip").exists():
        raise GtaCaptureError("native record result already exists; use a new run directory")
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        applied = client.request("apply_scenario", {
            "scenario_path": str(scenario.path),
            "scenario_sha256": sha256_file(scenario.path),
        })
        probe = client.request("probe_state")
        master = client.request("record_master", {
            "scenario_path": str(scenario.path),
            "duration_s": scenario.duration_s,
            "run_dir": str(run_dir),
        })
    finally:
        client.close()
    result = {"ok": True, "hello": hello, "scenario": applied,
              "probe": probe, "master": master}
    write_json_atomic(result_path, result)
    _print(result)
    return 0


def _cmd_stage_group_prepare(args) -> int:
    group = prepare_group(args.config, args.out)
    _print({"ok": True, "group": str(group), "next": str(group / "takes" / "AB" / "config" / "scenario.json")})
    return 0


def _cmd_stage_group_take(args) -> int:
    path = prepare_take(args.group, args.take)
    _print({"ok": True, "take": args.take, "scenario": str(path)})
    return 0


def _cmd_stage_take_record(args) -> int:
    """Records one take of a four-worldline group through the native companion."""
    scenario = StageScenario.load(args.scenario)
    run_dir = Path(args.run_dir).resolve() if args.run_dir else scenario.path.parent.parent
    result_path = run_dir / "evidence" / "stage-take-result.json"
    if result_path.exists() or (run_dir / "source" / "master-replay-1.clip").exists():
        raise GtaCaptureError("this take was already recorded; prepare a new group")
    run_dir.mkdir(parents=True, exist_ok=True)
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        applied = client.request("apply_scenario", {
            "scenario_path": str(scenario.path),
            "scenario_sha256": sha256_file(scenario.path),
        })
        take = client.request("record_master", {
            "scenario_path": str(scenario.path),
            "duration_s": scenario.duration_s,
            "run_dir": str(run_dir),
        })
    finally:
        client.close()
    problems = check_take_result(take, scenario)
    result = {"ok": not problems, "problems": problems, "hello": hello, "scenario": applied, "take": take}
    write_json_atomic(result_path, result)
    _print({k: v for k, v in result.items() if k != "take"} | {"take_kind": take.get("take_kind"),
                                                                "clips": len(take.get("clips") or [])})
    return 0 if not problems else 1


def _cmd_recorder_selftest(args) -> int:
    """A few seconds' recording under each condition a take uses: which leave a clip."""
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        result = client.request("recorder_selftest")
    finally:
        client.close()
    cases = result.get("cases") or []
    ok = bool(cases) and all(c.get("clip") for c in cases)
    _print({"ok": ok, **result})
    return 0 if ok else 1


def _cmd_stage_take_tape(args) -> int:
    group = Path(args.group).resolve()
    settings = load_json(group / "group.json")
    take_dir = group / "takes" / args.take
    # what the take was recorded with -- its sealed scenario and its result --
    # not group.json, which may have been edited since
    scenario = StageScenario.load(take_dir / "config" / "scenario.json")
    recorded = take_result(group, args.take)
    tape = build_take_tape(
        take_dir, spec_text=scenario.spec_path.read_text(encoding="utf-8"), take_kind=args.take,
        hold_s=float(recorded["hold_s"]), duration_s=scenario.duration_s,
        takeover_actor=scenario.raw["take"]["takeover_actor"] if args.take == "C" else None,
        clip_id=f"{settings['group_id']}-{args.take}")
    json_path = write_tape(take_dir / "config" / "camera-tape.json", tape)
    bin_path = write_native_tape(take_dir / "config" / "camera-tape.bin", tape, json_path=json_path)
    _print({"ok": True, "take": args.take, "tape": str(json_path), "native_tape": str(bin_path),
            "roles": tape["source"]["roles"], "interpolated_ticks": tape["source"]["interpolated_ticks"]})
    return 0


def _stage_export_lanes(args) -> list[int]:
    """--lane N, or --lanes as the queue's order: distinct lanes, lane 4 (which seals the
    take's Editor binding) last."""
    if (args.lane is None) == (args.lanes is None):
        raise GtaCaptureError("give --lane N or --lanes 1,2,3,4")
    if args.lane is not None:
        return [args.lane]
    try:
        lanes = [int(item) for item in args.lanes.split(",")]
    except ValueError:
        raise GtaCaptureError(f"--lanes {args.lanes!r} is not a comma separated list of lanes") from None
    if (not lanes or any(lane not in (1, 2, 3, 4) for lane in lanes) or len(set(lanes)) != len(lanes)
            or (4 in lanes and lanes[-1] != 4)):
        raise GtaCaptureError(f"--lanes {args.lanes!r}: lanes 1-4, each once, lane 4 last")
    return lanes


def _write_lane_result(evidence: Path, lane: dict, profile, *, hello, bound, probe) -> dict:
    """One exported lane's result file, once its frames were searched for the pointer."""
    if (lane.get("render_profile_sha256") != profile.sha256
            or Path(lane.get("normalized_output_root", "")) != Path(profile.output_root)):
        raise GtaCaptureError("the lane result did not keep the render profile/output root it was armed with")
    pixels = check_lane_pixels(lane)
    ok = pixels["pointer_frames"] == 0
    result = {"ok": ok, "internal": True, "delivery_eligible": False, "hello": hello, "bound": bound,
              "render_profile": {"path": str(profile.path), "sha256": profile.sha256, "output_root": profile.output_root},
              "probe": probe, "lane": lane, "pixel_check": pixels}
    if not ok:
        result["error"] = (f"the Editor's mouse pointer is drawn on {pixels['pointer_frames']} frames "
                           f"({pixels['first_pointer_frame']}..{pixels['last_pointer_frame']}): export the lane "
                           f"again with --arm-only --redo and leave the mouse alone while it renders")
    write_json_atomic(evidence / f"native-lane-{lane['lane']}-result.json", result)
    return result


def _cmd_stage_take_export(args) -> int:
    """Exports lanes of a recorded take through RE+ (internal: no build receipt).

    Run from Story Mode. The take is bound by content first (bind_take), then the
    lane is armed; enter the Editor with a project holding only this take's clips,
    open the export page and confirm the export -- and leave the mouse alone until
    it is done. Back in Story Mode, rerun the same command to collect the finished
    lane; collecting also searches every frame for the Editor's pointer. --redo
    (with --arm-only) sets a lane's earlier export aside to export it again.

    --lanes 1,2,3,4 arms the lanes as a queue: each next lane is armed in the Editor
    as soon as the one before it is sealed, so the operator presses Export once per
    lane without leaving the Editor, and comes back to Story Mode once to collect
    them all."""
    lanes = _stage_export_lanes(args)
    queue = args.lanes is not None
    group = Path(args.group).resolve()
    take_dir = group / "takes" / args.take
    evidence = take_dir / "evidence"
    if args.redo:
        if not args.arm_only:
            raise GtaCaptureError("--redo goes with --arm-only")
        aside = evidence / "rejected" / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        for lane in lanes:
            for name in (f"native-lane-{lane}-result.json", f"frame-events-lane{lane}.jsonl"):
                if (evidence / name).exists():
                    aside.mkdir(parents=True, exist_ok=True)
                    (evidence / name).rename(aside / name)
    else:
        done = [lane for lane in lanes if (evidence / f"native-lane-{lane}-result.json").exists()]
        if done and args.arm_only:
            raise GtaCaptureError(f"take {args.take} lane(s) {done} were exported already; --redo exports them again")
        if done:
            raise GtaCaptureError(f"take {args.take} lane(s) {done} already have a result")
    inputs = take_export_inputs(take_dir, take_result(group, args.take))
    profile = render_profile(args.render_ini)
    # before anything is armed or exported; collecting a queue exports nothing
    game_settings = check_game_settings(args.game_settings) if args.arm_only or not queue else None
    lane_request = {
        "camera_tape_path": str(inputs.tape_bin),
        "camera_tape_sha256": sha256_file(inputs.tape_json),
        "replay_path": str(inputs.replay),
        "replay_sha256": inputs.replay_sha256,
        "run_dir": str(take_dir),
        "expected_output_root": profile.output_root,
        "render_profile_sha256": profile.sha256,
    }
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        # Bind once per take: the lanes of one take share the binding (lane 4 seals
        # all four lanes' Editor sessions), and the collecting run after the Editor
        # finds its armed session still holding it.
        probe = client.request("probe_state")
        bound = None
        clip_count = 1 + len(inputs.more_clips)
        # The queue is collected as armed: bind_take is refused while one is held.
        if (args.arm_only or not queue) and (probe.get("record_binding_ready") is not True
                              or probe.get("master_replay_sha256") != inputs.replay_sha256
                              or probe.get("master_clip_count") != clip_count):
            request = {
                "replay_path": str(inputs.replay), "replay_sha256": inputs.replay_sha256,
                "source_clip_name": inputs.source_clip_name,
                "player_telemetry_path": str(inputs.player_telemetry),
                "player_telemetry_sha256": inputs.player_telemetry_sha256,
                "clip_count": clip_count,
            }
            for index, clip in enumerate(inputs.more_clips, start=2):
                request[f"clip_{index}_replay_path"] = str(clip.path)
                request[f"clip_{index}_replay_sha256"] = clip.sha256
                request[f"clip_{index}_source_clip_name"] = clip.source_clip_name
            bound = client.request("bind_take", request)
            probe = client.request("probe_state")
        _require_live_record_binding(probe, inputs.replay_sha256)
        if probe.get("master_clip_count") != clip_count:
            raise GtaCaptureError(f"the live controller binds {probe.get('master_clip_count')} of the take's "
                                  f"{clip_count} clips")
        if queue and args.arm_only:
            armed = client.request("arm_export_lanes", {"lanes": ",".join(map(str, lanes)), **lane_request})
        elif queue:
            collected = client.request("collect_export_lanes", {"run_dir": str(take_dir)})
        else:
            lane = client.request("arm_export_lane" if args.arm_only else "export_lane",
                                  {"lane": lanes[0], **lane_request})
    finally:
        client.close()

    if args.arm_only:
        result = {"ok": True, "internal": True, "delivery_eligible": False, "hello": hello, "bound": bound,
                  "render_profile": {"path": str(profile.path), "sha256": profile.sha256,
                                     "output_root": profile.output_root},
                  "game_settings": game_settings,
                  "probe": probe, "lane": armed if queue else lane, "pixel_check": None}
        name = "native-lanes-arm.json" if queue else f"native-lane-{lanes[0]}-arm.json"
        write_json_atomic(evidence / name, result)
        _print({k: v for k, v in result.items() if k not in ("hello", "probe")})
        return 0
    if not queue:
        result = _write_lane_result(evidence, lane, profile, hello=hello, bound=bound, probe=probe)
        _print({k: v for k, v in result.items() if k not in ("hello", "probe")})
        return 0 if result["ok"] else 1
    results = collected.get("lanes")
    if not isinstance(results, list) or [r.get("lane") for r in results] != lanes:
        raise GtaCaptureError(f"the lane queue answered for lanes {[r.get('lane') for r in results or []]}, "
                              f"not {lanes}")
    report = []
    for item in results:
        if item.get("error"):
            report.append({"lane": item["lane"], "ok": False, "error": item["error"]})
            continue
        result = _write_lane_result(evidence, item, profile, hello=hello, bound=None, probe=probe)
        report.append({"lane": item["lane"], "ok": result["ok"], "pixel_check": result["pixel_check"],
                       "result": str(evidence / f"native-lane-{item['lane']}-result.json")})
    ok = all(r["ok"] for r in report)
    _print({"ok": ok, "internal": True, "take": args.take, "lanes": report})
    return 0 if ok else 1


def _cmd_stage_take_encode(args) -> int:
    """The exported lanes of a take as MP4s (with the replay's audio from lane 1, its
    low chopping rumble swapped out: stage_audio), and a 2 x 2 preview once all four
    are there."""
    take_dir = Path(args.group).resolve() / "takes" / args.take
    cleaned = clean_take_audio(take_dir) if (take_dir / "source" / "lane-1-audio-candidate.wav").is_file() else None
    audio = cleaned["output"]["path"] if cleaned else None
    # the hold before stage tick 0 is cut: a video's frame 0 is tick 0
    hold = int(load_json(take_dir / "config" / "camera-tape.json")["source"]["hold_frames"])
    videos = []
    for lane in (args.lane,) if args.lane else (1, 2, 3, 4):
        out = take_dir / "capture" / f"video_{lane}.mp4"
        videos.append(str(encode_lane(load_lane_result(take_dir, lane), out, skip=hold, audio=audio, encoder=args.encoder)))
    preview = None
    if not args.lane and not args.no_preview:
        preview = str(encode_quad(videos, take_dir / "preview" / "four-camera-preview.mp4"))
    _print({"ok": True, "take": args.take, "videos": videos, "preview": preview,
            "audio_swapped": cleaned["swapped"] if cleaned else None, "internal": True})
    return 0


def _cmd_stage_group_deliver(args) -> int:
    """The group as the customer's delivery folder (docs/69, 交付说明): <out>/<delivery_name>
    with the 36 files numbered 1-12, and beside it the manifest and a README saying
    which number is which worldline and camera and what happened in each."""
    manifest = deliver_group(args.group, args.out, time_anchors=parse_time_anchors(args.time_anchor),
                             untrack_floats=[i for i in args.untrack_floats.split(",") if i])
    out = Path(args.out).resolve()
    _print({"ok": True, "group_id": manifest["group_id"], "files": len(manifest["files"]),
            "out": str(out / manifest["delivery_name"]),
            "manifest": str(out / f"{manifest['delivery_name']}.manifest.json"),
            "time": {kind: take["time_semantics"] for kind, take in manifest["takes"].items()}, "internal": True})
    return 0


def _cmd_stage_group_gate(args) -> int:
    """Passes only when the worldlines agree exactly wherever nobody intervened --
    to one float bit, as every rehearsal did. Within the looser tolerance but not
    exact is reported, and fails: it is a divergence to explain, not to ship."""
    report = cross_worldline_gate(args.group)
    _print({"ok": bool(report["exact"]), "exact": report["exact"],
            "within_tolerance": report["within_tolerance"], "problems": report["problems"],
            "same_plan": report["same_plan"], "same_calibration": report["same_calibration"],
            "report": str(Path(args.group).resolve() / "evidence" / "cross-worldline.json")})
    return 0 if report["exact"] else 1


def _cmd_stage_spec_check(args) -> int:
    """A stage script judged without the game: the cast of the plan, its cameras. The
    played person of takes C and D is not judged here (the report's not_judged)."""
    world = dump_plan_world(args.dump_tool, args.spec, args.calibration, args.ground_z)
    recorded = None
    if args.recorded:
        with open(args.recorded, encoding="utf-8") as f:
            recorded = f.readlines()
    report = check_stage_script(world, seed=args.seed, recorded_lines=recorded)
    report["spec"] = {"path": str(Path(args.spec).resolve()), "sha256": sha256_file(args.spec)}
    report["calibration"] = {"path": str(Path(args.calibration).resolve()), "sha256": sha256_file(args.calibration)}
    if args.out:
        write_json_atomic(Path(args.out), report)
    _print({"ok": report["ok"], "scope": report["scope"], "problems": report["problems"],
            "checks": {c["id"]: c["ok"] for c in report["checks"]}, "not_judged": report["not_judged"],
            "report": str(Path(args.out).resolve()) if args.out else None})
    return 0 if report["ok"] else 1


def _cmd_native_export_smoke(args) -> int:
    tape = load_tape(args.tape)
    if tape.get("coordinate_convention") != GTA_COORDINATE_CONVENTION:
        raise GtaCaptureError(
            "native export requires an explicitly calibrated GTA-native coordinate convention")
    run_dir = Path(args.run_dir).resolve()
    result_name = (f"native-lane-{args.lane}-arm.json" if args.arm_only else
                   f"native-lane-{args.lane}-result.json")
    result_path = run_dir / "evidence" / result_name
    if result_path.exists():
        raise GtaCaptureError(
            f"native lane {args.lane} result already exists; use a new run directory")
    config_dir = run_dir / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    manifest = DependencyManifest.load(args.manifest)
    binding = load_render_profile_binding(manifest)
    manifest_snapshot = config_dir / "dependency-manifest.json"
    if manifest_snapshot.is_file():
        if DependencyManifest.load(manifest_snapshot).raw != manifest.raw:
            raise GtaCaptureError(
                f"run already contains a different immutable dependency manifest: {manifest_snapshot}")
    else:
        write_json_atomic(manifest_snapshot, manifest.raw)
    tape_json = config_dir / "camera-tape.json"
    tape_native = config_dir / "camera-tape.bin"
    if tape_json.is_file():
        if load_tape(tape_json) != tape:
            raise GtaCaptureError(
                f"run already contains a different immutable camera tape: {tape_json}")
    else:
        write_tape(tape_json, tape)
    write_native_tape(tape_native, tape, json_path=tape_json)
    replay = Path(args.replay).resolve()
    if not replay.is_file():
        raise GtaCaptureError(f"replay file does not exist: {replay}")
    replay_sha256 = sha256_file(replay)
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely in Story Mode")
        probe = client.request("probe_state")
        _require_live_record_binding(probe, replay_sha256)
        lane = client.request("arm_export_lane" if args.arm_only else "export_lane", {
            "lane": args.lane,
            "camera_tape_path": str(tape_native),
            "camera_tape_sha256": sha256_file(tape_json),
            "replay_path": str(replay),
            "replay_sha256": replay_sha256,
            "run_dir": str(run_dir),
            "expected_output_root": binding.expected_output_root,
            "render_profile_sha256": binding.render_profile_sha256,
        })
        if (not args.arm_only and
                (lane.get("render_profile_sha256") != binding.render_profile_sha256
                 or Path(lane.get("normalized_output_root", "")).resolve() !=
                 Path(binding.expected_output_root).resolve())):
            raise GtaCaptureError(
                "native lane result did not preserve the sealed render profile/output root")
    finally:
        client.close()
    result = {"ok": True, "hello": hello, "probe": probe, "lane": lane}
    write_json_atomic(result_path, result)
    _print(result)
    return 0


def _cmd_editor_control(args) -> int:
    controls = {
        "up": 188,
        "down": 187,
        "left": 189,
        "right": 190,
        "accept": 201,
        "cancel": 202,
    }
    client = JsonLinePipeClient(args.pipe)
    try:
        hello = client.request("hello")
        if hello.get("story_mode") is not True or hello.get("online_session") is True:
            raise GtaCaptureError("native companion is not safely offline")
        if args.action == "activate":
            result = client.request("activate_editor")
        elif args.action == "probe":
            result = client.request("probe_editor")
        else:
            result = client.request("editor_input", {"control": controls[args.action]})
    finally:
        client.close()
    _print({"ok": True, "action": args.action, "result": result})
    return 0


def _cmd_native_audio_smoke(args) -> int:
    run_dir = Path(args.run_dir).resolve()
    result_path = run_dir / "evidence" / "native-audio-result.json"
    if result_path.exists():
        raise GtaCaptureError("native audio result already exists; use a new run directory")
    replay = Path(args.replay).resolve()
    if not replay.is_file():
        raise GtaCaptureError(f"replay file does not exist: {replay}")
    client = JsonLinePipeClient(args.pipe)
    try:
        audio = client.request("capture_audio", {
            "replay_path": str(replay),
            "replay_sha256": sha256_file(replay),
            "duration_s": args.duration,
            "run_dir": str(run_dir),
            "radio_policy": "off",
        })
    finally:
        client.close()
    audio_path = audio.get("audio_path")
    if not isinstance(audio_path, str):
        raise GtaCaptureError("companion did not return a sealed WAV path")
    reported_source_sha256 = audio.get("audio_sha256")
    normalization = normalize_replus_wav(audio_path)
    if reported_source_sha256 != normalization["source_audio_sha256"]:
        raise GtaCaptureError(
            "companion audio sha256 does not match the pre-normalization WAV")
    validation = validate_wav(audio_path, expected_duration_s=args.duration)
    validation["normalization"] = normalization
    validation["replay_sha256"] = sha256_file(replay)
    radio_evidence = audio.get("radio_off_evidence")
    validation["radio_off_evidence"] = radio_evidence
    validation["radio_off"] = radio_off_evidence_valid(
        radio_evidence, expected_replay_sha256=validation["replay_sha256"],
        expected_duration_s=args.duration)
    report_path = run_dir / "evidence" / "audio-report.json"
    write_json_atomic(report_path, validation)
    if not validation["continuous"] or not validation["non_silent"]:
        raise GtaCaptureError("sealed RE+ WAV failed continuity/non-silence validation")
    audio["source_audio_sha256"] = normalization["source_audio_sha256"]
    audio["audio_sha256"] = validation["audio_sha256"]
    audio["normalization"] = normalization
    audio["audio_report_path"] = str(report_path.resolve())
    audio["audio_report_sha256"] = sha256_file(report_path)
    audio["validation"] = validation
    write_json_atomic(result_path,
                      {"ok": True, "audio": audio})
    _print({"ok": True, "audio": audio})
    return 0


def _load_all(args):
    manifest = DependencyManifest.load(args.manifest)
    scenario = Scenario.load(args.scenario)
    tape = load_tape(args.tape, expected_duration_s=scenario.duration_s)
    return manifest, scenario, tape


def _cmd_validate(args) -> int:
    manifest, scenario, tape = _load_all(args)
    problems = manifest.policy.validate()
    _print({
        "ok": not problems,
        "manifest_schema": manifest.raw["schema"],
        "scenario_id": scenario.scenario_id,
        "camera_tape": {"clip_id": tape.get("clip_id"), "n_frames": tape["n_frames"]},
        "policy": manifest.policy.to_dict(),
        "problems": problems,
    })
    return 0 if not problems else 1


def _cmd_preflight(args) -> int:
    manifest = DependencyManifest.load(args.manifest)
    report = run_preflight(manifest, mode=args.mode)
    if args.out:
        write_json_atomic(args.out, report.to_dict())
    _print(report.to_dict())
    return 0 if report.ok else 1


def _cmd_run(args) -> int:
    manifest, scenario, tape = _load_all(args)
    backend = DryRunBackend() if args.backend == "dry-run" else PipeBackend(args.pipe)
    mode = "dry-run" if args.backend == "dry-run" else "real"
    summary = run_capture(manifest=manifest, scenario=scenario, tape=tape,
                          run_dir=args.run_dir, backend=backend, mode=mode,
                          resume=args.resume)
    _print({
        "ok": True,
        "run_id": summary["run_id"],
        "run_dir": str(Path(args.run_dir).resolve()),
        "dry_run": summary["dry_run"],
        "delivery_eligible": summary["delivery_eligible"],
        "next_gate": summary["next_gate"],
    })
    return 0


def _cmd_finalize_native_poc(args) -> int:
    manifest, scenario, tape = _load_all(args)
    result = finalize_staged_poc(manifest=manifest, scenario=scenario, tape=tape,
                                 run_dir=args.run_dir,
                                 allow_internal_calibration_gaps=
                                 args.materialize_internal_poc_with_calibration_gaps)
    _print({"ok": True, "run_id": result["run_id"],
            "run_manifest": str((Path(args.run_dir) / "run-manifest.json").resolve()),
            "gta_pr405_gate": result["gta_pr405_gate"]})
    return 0


def _cmd_inspect(args) -> int:
    _print(load_run_summary(args.run_dir))
    return 0


def _cmd_qc(args) -> int:
    code, report = run_engine_qc(args.run_dir, expect_duration_s=args.expect_duration)
    _print({"exit_code": code, "verdict": report.get("verdict"),
            "report": str((Path(args.run_dir) / "evidence" / "engine-qc.json").resolve())})
    return code


def _cmd_package(args) -> int:
    result = package_run(args.run_dir, args.out, game=args.game, group=args.group,
                         prefix=args.prefix, scene=args.scene)
    _print(result)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gta_capture",
        description="GTA V Legacy PR #405 capture orchestration (dry-run or Story Mode ASI backend)",
    )
    parser.add_argument("--version", action="version", version=f"gta_capture {__version__}")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("make-demo-tape", help="write a deterministic four-lane protocol tape")
    p.add_argument("--out", required=True)
    p.add_argument("--duration", type=float, default=10.0)
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--height", type=int, default=1080)
    p.add_argument("--filmback-id", default="dryrun-36mm-horizontal")
    p.add_argument("--clip-id", default="dryrun-poc")
    p.set_defaults(func=_cmd_make_demo_tape)

    p = sub.add_parser(
        "make-gta-poc-tape",
        help="write an absolute GTA-native four-lane tape around a Story Mode anchor")
    p.add_argument("--out", required=True)
    p.add_argument("--anchor-x", type=float, required=True)
    p.add_argument("--anchor-y", type=float, required=True)
    p.add_argument("--anchor-z", type=float, required=True)
    p.add_argument("--duration", type=float, default=10.0)
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--height", type=int, default=1080)
    p.add_argument("--filmback-id", default="gta-poc-36mm-horizontal")
    p.add_argument("--clip-id", default="gta-legacy-poc")
    p.add_argument("--route-distance-m", type=float, default=5.0)
    p.add_argument("--route-speed-mps", type=float, default=1.0)
    p.add_argument("--route-heading-deg", type=float, default=0.0)
    p.add_argument(
        "--lane-lateral-offsets", type=float, nargs=4, default=None,
        metavar=("LANE1_M", "LANE2_M", "LANE3_M", "LANE4_M"),
        help="optional per-lane offsets along the route-right axis; use surveyed clear-space values")
    p.set_defaults(func=_cmd_make_gta_poc_tape)

    p = sub.add_parser(
        "bind-poc-scenario",
        help="bind the checked POC scenario to an explicit probed Story Mode anchor")
    p.add_argument("--template", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--scenario-id", required=True)
    p.add_argument("--anchor-x", type=float, required=True)
    p.add_argument("--anchor-y", type=float, required=True)
    p.add_argument("--anchor-z", type=float, required=True)
    p.set_defaults(func=_cmd_bind_poc_scenario)

    p = sub.add_parser("import-trajgen", help="convert a trajgen clip into a GTA camera tape")
    p.add_argument("clip")
    p.add_argument("--out", required=True)
    p.add_argument("--filmback-id", required=True)
    p.add_argument("--clip-id", default=None)
    p.set_defaults(func=_cmd_import_trajgen)

    p = sub.add_parser(
        "place-trajgen",
        help="pin a trajgen clip to a surveyed GTA anchor and emit a flyable tape")
    p.add_argument("clip")
    p.add_argument("--out", required=True)
    p.add_argument("--filmback-id", required=True)
    p.add_argument("--clip-id", default=None)
    p.add_argument("--anchor-x", type=float, required=True,
                   help="GTA world X under the subject at t=0")
    p.add_argument("--anchor-y", type=float, required=True,
                   help="GTA world Y under the subject at t=0")
    p.add_argument("--anchor-z", type=float, required=True,
                   help="GTA world Z of the walkable surface at the anchor")
    p.add_argument("--heading-deg", type=float, required=True,
                   help="GTA heading applied to the whole rig (0 = +Y)")
    p.add_argument("--ground-y", type=float, default=0.0,
                   help="trajgen scene ground plane in the delivery basis (synthetic scenes: 0)")
    p.add_argument("--subject-route", default=None,
                   help="where to write the controlled-character route (default: beside --out)")
    p.set_defaults(func=_cmd_place_trajgen)

    p = sub.add_parser(
        "solve-calibration",
        help="solve measured correspondences into projection calibration evidence")
    p.add_argument("--samples", required=True,
                   help="dm-gta-calibration-samples/1 file measured on the capture machine")
    p.add_argument("--out", required=True)
    p.add_argument("--calibration-id", required=True)
    p.add_argument("--filmback-id", required=True)
    p.add_argument("--metric-scale", type=float, required=True,
                   help="approved manifest metric_scale; carried through, never fitted")
    p.add_argument("--approval-status", default="draft",
                   choices=("draft", "approved"),
                   help="defaults to draft; pass approved only after the solved numbers "
                        "were reviewed (preflight rejects anything but approved)")
    p.set_defaults(func=_cmd_solve_calibration)

    p = sub.add_parser("probe-game", help="read current Story Mode anchor and final camera")
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_probe_game)

    p = sub.add_parser("sample-story-projection",
                       help="capture one unapproved Story projection diagnostic through Igcs")
    p.add_argument("--request", required=True)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_story_projection_sample)

    p = sub.add_parser(
        "native-record-smoke",
        help="apply one scenario and record a replay before the full adapter gate is opened")
    p.add_argument("--scenario", required=True)
    p.add_argument("--run-dir", required=True)
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_native_record_smoke)

    p = sub.add_parser(
        "stage-group-prepare",
        help="start a four-worldline group: its stage and take AB's configuration")
    p.add_argument("--config", required=True, help="dm-gta-stage-group/1 file")
    p.add_argument("--out", required=True)
    p.set_defaults(func=_cmd_stage_group_prepare)

    p = sub.add_parser(
        "stage-group-take",
        help="prepare take C or D of a group (they replay the calibration take AB measured)")
    p.add_argument("--group", required=True)
    p.add_argument("--take", choices=("C", "D"), required=True)
    p.set_defaults(func=_cmd_stage_group_take)

    p = sub.add_parser(
        "stage-take-record",
        help="record one take (AB, C or D) of a four-worldline group")
    p.add_argument("--scenario", required=True, help="the take's dm-gta-scenario/2 file")
    p.add_argument("--run-dir", help="defaults to the take's directory in the group")
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_stage_take_record)

    p = sub.add_parser(
        "stage-take-export",
        help="export one lane of a recorded take through RE+ (internal, no build receipt)")
    p.add_argument("--group", required=True)
    p.add_argument("--take", required=True, choices=["AB", "C", "D"])
    p.add_argument("--lane", type=int, choices=[1, 2, 3, 4])
    p.add_argument("--lanes", help="a queue of lanes exported back to back in one Editor visit, e.g. 1,2,3,4")
    p.add_argument("--render-ini", required=True, help="the installed RE+ Render.ini")
    p.add_argument("--game-settings", default=str(default_game_settings()),
                   help="the game's settings.xml: Post FX above High is refused (it puts a depth of field on the export)")
    p.add_argument("--arm-only", action="store_true")
    p.add_argument("--redo", action="store_true", help="with --arm-only: set the lanes' earlier exports aside")
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_stage_take_export)

    p = sub.add_parser(
        "stage-take-encode",
        help="encode a take's exported lanes to MP4 (and a 2x2 preview when all four are done)")
    p.add_argument("--group", required=True)
    p.add_argument("--take", required=True, choices=["AB", "C", "D"])
    p.add_argument("--lane", type=int, choices=[1, 2, 3, 4])
    p.add_argument("--encoder", choices=["x264", "nvenc"], default="x264",
                   help="libx264 on the CPU (default), or the GPU's NVENC: quicker, and off the CPU")
    p.add_argument("--no-preview", action="store_true", help="skip the 2x2 preview (it is for looking, not delivery)")
    p.set_defaults(func=_cmd_stage_take_encode)

    p = sub.add_parser(
        "stage-group-deliver",
        help="a recorded and exported group as the docs/69 delivery folder (36 files numbered 1-12)")
    p.add_argument("--group", required=True)
    p.add_argument("--out", required=True,
                   help="the delivery root; the group lands in <out>/<delivery_name>, "
                        "its manifest and README beside it")
    p.add_argument("--time-anchor", action="append", default=[], metavar="TAKE=YYYY-MM-DD HH:mm:ss.SSS",
                   help="for a take whose input log has no wall clock (recorded before it was logged): the system "
                        "time its stage tick 0 was played at; once per take, e.g. \"C=2026-09-23 15:02:10.000\"")
    p.add_argument("--untrack-floats", default="", metavar="ID,ID,...",
                   help="the floats in the sea the takes tracked (till beach-demo-109) to leave out of multicam_sync: "
                        "the export bobs them on its own waves, not as the take logged them (2026-10-18)")
    p.set_defaults(func=_cmd_stage_group_deliver)

    p = sub.add_parser(
        "recorder-selftest",
        help="record a few seconds under each condition a take uses and report which leave a clip")
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_recorder_selftest)

    p = sub.add_parser(
        "stage-take-tape",
        help="build a recorded take's four camera lanes (fixed/orbit/dolly/B, or fixed/orbit/chase/first person)")
    p.add_argument("--group", required=True)
    p.add_argument("--take", choices=("AB", "C", "D"), required=True)
    p.set_defaults(func=_cmd_stage_take_tape)

    p = sub.add_parser(
        "stage-group-gate",
        help="compare a group's three takes wherever nobody intervened")
    p.add_argument("--group", required=True)
    p.set_defaults(func=_cmd_stage_group_gate)

    p = sub.add_parser(
        "stage-spec-check",
        help="judge a stage script without the game: nobody through anybody, the cast on its stage and in its cameras")
    p.add_argument("--spec", required=True, help="the stage spec (dm-gta-stage-spec/1)")
    p.add_argument("--calibration", required=True,
                   help="the clips as measured in the game (dm-gta-stage-calibration/1): a group's "
                        "takes/AB/source/stage-calibration.txt")
    p.add_argument("--dump-tool", required=True, help="the built gta_stage_plan_dump (native/CMakeLists.txt)")
    p.add_argument("--ground-z", type=float, help="the height of the flat ground (the spec's frame z when not given)")
    p.add_argument("--seed", type=int, default=0, help="the group's seed: worldline A's free cameras are drawn from it")
    p.add_argument("--recorded", help="a take's master-world-telemetry.jsonl to hold the prediction against")
    p.add_argument("--out", help="where to write the whole report")
    p.set_defaults(func=_cmd_stage_spec_check)

    p = sub.add_parser(
        "native-export-smoke",
        help="export one prepared Editor lane before the full adapter gate is opened")
    p.add_argument("--lane", type=int, choices=(1, 2, 3, 4), required=True)
    p.add_argument("--tape", required=True)
    p.add_argument("--replay", required=True)
    p.add_argument("--manifest", required=True)
    p.add_argument("--run-dir", required=True)
    p.add_argument(
        "--arm-only", action="store_true",
        help="validate and arm the process-lifetime lane, then return before Editor UI")
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_native_export_smoke)

    p = sub.add_parser(
        "editor-control",
        help="drive only allowlisted Rockstar Editor frontend actions for an armed lane")
    p.add_argument(
        "action", choices=("activate", "probe", "up", "down", "left", "right",
                           "accept", "cancel"))
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_editor_control)

    p = sub.add_parser(
        "native-audio-smoke",
        help="seal audio from the most recent prepared Editor export")
    p.add_argument("--replay", required=True)
    p.add_argument("--run-dir", required=True)
    p.add_argument("--duration", type=float, default=10.0)
    p.add_argument("--pipe", default=DEFAULT_PIPE)
    p.set_defaults(func=_cmd_native_audio_smoke)

    def add_inputs(p):
        p.add_argument("--manifest", required=True)
        p.add_argument("--scenario", required=True)
        p.add_argument("--tape", required=True)

    p = sub.add_parser("validate", help="validate manifest, scenario, tape, and policy")
    add_inputs(p)
    p.set_defaults(func=_cmd_validate)

    p = sub.add_parser("preflight", help="check policy and local real-capture dependencies")
    p.add_argument("--manifest", required=True)
    p.add_argument("--mode", choices=("dry-run", "real"), required=True)
    p.add_argument("--out", default=None)
    p.set_defaults(func=_cmd_preflight)

    p = sub.add_parser("run", help="execute or resume the capture state machine")
    add_inputs(p)
    p.add_argument("--run-dir", required=True)
    p.add_argument("--backend", choices=("dry-run", "pipe"), default="dry-run")
    p.add_argument("--pipe", default=r"\\.\pipe\diogenes-gta-capture-v1")
    p.add_argument("--resume", action="store_true")
    p.set_defaults(func=_cmd_run)

    p = sub.add_parser(
        "finalize-native-poc",
        help="seal four operator-gated native lanes into one PR #405 evidence run")
    add_inputs(p)
    p.add_argument("--run-dir", required=True)
    p.add_argument(
        "--materialize-internal-poc-with-calibration-gaps",
        action="store_true",
        help=("retain the three projection-calibration FAIL checks while materializing "
              "a local non-trainable evidence demo; all other preflight failures "
              "remain blocking"))
    p.set_defaults(func=_cmd_finalize_native_poc)

    p = sub.add_parser("inspect", help="render a completed run manifest")
    p.add_argument("run_dir")
    p.set_defaults(func=_cmd_inspect)

    p = sub.add_parser("qc", help="run the existing engine_qc on a real GTA capture")
    p.add_argument("run_dir")
    p.add_argument("--expect-duration", type=float, default=None)
    p.set_defaults(func=_cmd_qc)

    p = sub.add_parser("package", help="package a QC PASS run; blocked by non-trainable policy")
    p.add_argument("run_dir")
    p.add_argument("--out", required=True)
    p.add_argument("--game", default="GTA5.exe")
    p.add_argument("--group", required=True)
    p.add_argument("--prefix", default="")
    p.add_argument("--scene", default="")
    p.set_defaults(func=_cmd_package)
    return parser


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return int(args.func(args))
    except KeyError as exc:
        print(f"gta_capture: a native result/manifest is missing required field {exc}; "
              "the staged evidence is incomplete", file=sys.stderr)
        return 1
    except (GtaCaptureError, OSError, ValueError) as exc:
        print(f"gta_capture: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
