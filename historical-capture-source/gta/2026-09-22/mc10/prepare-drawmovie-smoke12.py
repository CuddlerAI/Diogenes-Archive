"""Offline preparation only: require a new real 12s master; never call a game API.

Reuse the reviewed port camera offsets as requested geometry, retargeted to all
360 observations of the new recording. This is neither calibration evidence nor
an MC delivery plan. The CLI still checks live master/session binding at ARM.
"""
from __future__ import annotations
import argparse
import copy
import json
from pathlib import Path
import sys

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
ROOT = BASE.parents[1]
sys.path.insert(0, str(ROOT))
from gta_capture.errors import ContractError
from gta_capture.jsonio import file_record, load_json
from gta_capture.preflight import DependencyManifest
from gta_capture.native_tape import write_native_tape
from gta_capture.recorded_route import build_recorded_subject_route, validate_recorded_subject_route
from gta_capture.scenario import Scenario
from gta_capture.tape import GTA_COORDINATE_CONVENTION, TAPE_SCHEMA, load_tape, validate_tape, write_tape

TEMPLATE = BASE / 'scene02-port/run-02-reexport-01/config/camera-tape.json'
TEMPLATE_SHA = '3e9c8d63c19a6a95b8c51748798169288868fd3f1966d7f2028155bc62cbcf87'


def checked_output(path, inputs):
    out = Path(path).resolve()
    if out == BASE or BASE not in out.parents:
        raise ContractError('output must be a new directory strictly inside this batch out directory')
    if out.exists():
        raise ContractError('output already exists; preserve attempts and choose a new directory')
    for path in inputs:
        path = Path(path).resolve()
        if path == out or out in path.parents:
            raise ContractError('output may not contain an input')
    return out


def retarget_lanes(template, new_route):
    """Requested poses only. Preserve each old camera-to-actor relative vector."""
    old_points = template['source']['subject_route']['waypoints']
    new_points = new_route['waypoints']
    if len(new_points) != 360 or len(old_points) < 360:
        raise ContractError('exactly 360 new route points and a sufficient template are required')
    lanes = {}
    for lane_id, old_lane in template['lanes'].items():
        samples = []
        for frame in range(360):
            sample = copy.deepcopy(old_lane['samples'][frame])
            sample['position'] = [round(sample['position'][a] - old_points[frame]['position'][a]
                                       + new_points[frame]['position'][a], 9) for a in range(3)]
            samples.append(sample)
        lanes[lane_id] = {'focal_mm': old_lane['focal_mm'],
                          'resolution': copy.deepcopy(old_lane['resolution']), 'samples': samples}
    return lanes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--scenario', type=Path, required=True)
    parser.add_argument('--record-run-dir', type=Path, required=True)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--out-dir', type=Path, required=True)
    args = parser.parse_args()
    run = args.record_run_dir.resolve()
    record_path = run / 'evidence/native-record-result.json'
    out = checked_output(args.out_dir, [args.scenario, record_path, args.manifest, TEMPLATE])
    scenario = Scenario.load(args.scenario)
    if scenario.duration_s != 12.0:
        raise ContractError('this diagnostic requires a complete new 12-second/360-frame recording')
    template_ref = file_record(TEMPLATE)
    if template_ref['sha256'] != TEMPLATE_SHA:
        raise ContractError('the immutable reviewed camera template changed')
    template = load_tape(TEMPLATE)
    old_route = template['source']['subject_route']
    old_scenario = Scenario.load(old_route['observation']['files']['scenario']['path'])
    validate_recorded_subject_route(old_route, scenario=old_scenario)
    for key in ('location', 'clock', 'weather', 'radio', 'player', 'dynamics', 'population'):
        if scenario.raw[key] != old_scenario.raw[key]:
            raise ContractError('use the supplied port diagnostic scenario; template scene differs: ' + key)
    # Full existing verifier checks real record, applied scenario SHA, all telemetry,
    # source clock, master SHA, complete window and actual recording start anchor.
    route = build_recorded_subject_route(record_path, scenario.path, window_frames=15, passes=3)
    replay_ref = route['observation']['files']['replay']
    if Path(replay_ref['path']).resolve() != run / 'source/master-replay.clip':
        raise ContractError('record must bind this run master-replay.clip')
    if replay_ref['sha256'] == old_route['observation']['files']['replay']['sha256']:
        raise ContractError('a new recording is required; the historical template master is not allowed')
    validate_recorded_subject_route(route, scenario=scenario, expected_replay_sha256=replay_ref['sha256'])
    DependencyManifest.load(args.manifest)  # Structural validation only, not preflight approval.
    manifest_ref = file_record(args.manifest)
    record = load_json(record_path)
    # No output directory exists until all actual input validation above has passed.
    out.mkdir(parents=True, exist_ok=False)
    route = build_recorded_subject_route(record_path, scenario.path, window_frames=15, passes=3,
                                        artifact_dir=out / 'frozen-master')
    tape = {'schema': TAPE_SCHEMA, 'clip_id': scenario.scenario_id + '-cursor-diagnostic',
            'fps_num': 30, 'fps_den': 1, 'duration_s': 12.0, 'n_frames': 360,
            'timestamp_semantics': 'source_replay_timeline',
            'filmback_id': 'cursor-diagnostic-FOV-control-only-unapproved',
            'coordinate_convention': GTA_COORDINATE_CONVENTION,
            'source': {'kind': 'drawmovie_guard_runtime_diagnostic',
                       'camera_template': template_ref, 'template_selection': {'start_frame': 0, 'n_frames': 360},
                       'camera_semantics': 'requested camera offsets/orientations/FOV from template; positions retargeted to new measured route',
                       'subject_route': route, 'native_command': route['native_command'],
                       'anchor': route['waypoints'][0]['position'], 'replay_sha256': replay_ref['sha256'],
                       'delivery_eligible': False, 'intrinsics_calibrated': False,
                       'editor_equivalence_verified': False, 'formal_quality_approved': False,
                       'person_visibility_verified': False, 'player_replay_origin_verified': False},
            'lanes': retarget_lanes(template, route)}
    validate_tape(tape, expected_duration_s=12.0)
    tape_path = out / 'camera-tape.json'
    write_tape(tape_path, tape)
    write_native_tape(out / 'camera-tape.bin', tape, json_path=tape_path)
    commands = []
    for lane in (1, 2, 3, 4):
        argv = [sys.executable, '-m', 'gta_capture', 'native-export-smoke', '--lane', str(lane),
                '--tape', str(tape_path), '--replay', replay_ref['path'], '--manifest', manifest_ref['path'],
                '--run-dir', str(run)]
        commands.append({'lane': lane, 'arm_argv': argv + ['--arm-only'], 'collect_argv': argv,
                         'executed_by_this_helper': False})
    receipt = {'schema': 'dm-gta-drawmovie-smoke-prepare/1', 'planning_only': True,
               'expected_frames_per_lane': 360, 'duration_s': 12.0,
               'inputs': {'scenario': file_record(scenario.path), 'record_result': file_record(record_path),
                          'manifest': manifest_ref, 'template': template_ref, 'helper': file_record(__file__)},
               'camera_tape': file_record(tape_path), 'native_tape': file_record(out / 'camera-tape.bin'),
               'source_replay_sha256': replay_ref['sha256'],
               'recording_process_id_from_pre_record_hello': record['hello']['process_id'],
               'pre_record_hello_is_not_new_master_binding': True,
               'master_result': record['master'], 'commands': commands,
               'minimum_reproduction_sequence': [1, 2],
               'manifest_full_preflight_required_separately': True,
               'pipe_called': False, 'new_editor_exports_verified': False,
               'formal_calibration_approval': False, 'formal_quality_approved': False,
               'delivery_eligible': False,
               'limitations': ['Recording telemetry estimates replay frame origin; actual export fences still require independent verification.',
                               'Intrinsics encode requested FOV only; no calibration approval or Story/Editor equivalence claim.',
                               'The prior relative camera framing is a request, not proof that the new pixels contain a complete person.',
                               'Do not call MC finalize or batch-index selection for this short runtime diagnostic.']}
    with (out / 'prepare-receipt.json').open('x', encoding='utf-8') as stream:
        json.dump(receipt, stream, ensure_ascii=False, indent=2)
        stream.write('\n')
    print(json.dumps({'prepared': str(out), 'n_frames': 360, 'pipe_called': False,
                      'new_editor_exports_verified': False, 'delivery_eligible': False}))


if __name__ == '__main__':
    try:
        main()
    except (ContractError, OSError, ValueError, KeyError) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(2)
