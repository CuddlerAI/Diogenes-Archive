"""Offline recorded-MC aim derivation; no game calls, tape, or approval."""
from __future__ import annotations

import argparse
import copy
import json
import math
from pathlib import Path
import shutil
import sys

PACKAGE = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(PACKAGE))
from gta_capture.errors import ContractError
from gta_capture.jsonio import file_record, load_json, sha256_file, write_json_atomic
from gta_capture.moving_plan import _dependencies, _clip_anchor_track, _framing_report, _recorded_slope_proxy
from gta_capture.recorded_route import validate_recorded_subject_route
from gta_capture.scenario import Scenario

_dependencies()
import numpy as np
from trajgen.mathutil import quat_from_yaw_elev_roll, yaw_elev_from_forward, vsub, euler_tuple, wrap180, clamp
from trajgen.metrics import physics, decouple
from trajgen.covis import CamView, evaluate
from trajgen.scene import Scene
from engine_qc.clip_checker import check_clip

N_FRAMES = 1800
SHIFTS = {'1': -0.60, '2': -0.50, '3': -0.35, '4': 0.0}
BASE_AIM_HEIGHT = 0.925
JOINT_BOUNDS = (-1.2, 1.85)
SENSITIVITY_BOUNDS = {'original_proxy': (0.0, 1.85), 'feet_minus1': (-1.0, 0.85),
                      'union_of_all_offsets_minus1point2_to_zero': JOINT_BOUNDS}


def require(condition, message):
    if not condition:
        raise ContractError(message)


def finite_number(value):
    return float(value) if math.isfinite(float(value)) else None


def project_body_envelope(records, entity, system, low, high, *, include_frames=False):
    """Project eight corners per frame; entity is the local fitted origin, not feet."""
    positions = np.asarray([f['camera_position'] for f in records], dtype=float)
    q = np.asarray([f['camera_rotation_quaternion'] for f in records], dtype=float)
    lengths = np.linalg.norm(q, axis=1)
    require(np.isfinite(q).all() and np.all(lengths > 0), 'invalid planned camera quaternion')
    q = q / lengths[:, None]
    x, y, z, w = q.T
    right = np.array([1-2*(y*y+z*z), 2*(x*y+z*w), 2*(x*z-y*w)]).T
    up = np.array([2*(x*y-z*w), 1-2*(x*x+z*z), 2*(y*z+x*w)]).T
    forward = np.array([2*(x*z+y*w), 2*(y*z-x*w), 1-2*(x*x+y*y)]).T
    intr = {key: np.asarray([f['camera_intrinsics'][key] for f in records], dtype=float)
            for key in ('fx', 'fy', 'cx', 'cy')}
    require(all(np.isfinite(v).all() for v in intr.values()) and
            np.all(intr['fx'] > 0) and np.all(intr['fy'] > 0), 'invalid planned camera intrinsics')
    minimum = np.full(len(records), float('inf'))
    maximum_v = np.full(len(records), -float('inf'))
    near_ok = np.ones(len(records), dtype=bool)
    for dx in (-0.35, 0.35):
        for dz in (-0.35, 0.35):
            for dy in (low, high):
                delta = entity + np.array([dx, dy, dz]) - positions
                depth = np.sum(delta * forward, axis=1)
                valid = np.isfinite(depth) & (depth > 0.05)
                safe_depth = np.where(valid, depth, 1.0)
                u = intr['cx'] + intr['fx'] * np.sum(delta * right, axis=1) / safe_depth
                v = intr['cy'] - intr['fy'] * np.sum(delta * up, axis=1) / safe_depth
                margin = np.minimum.reduce([u, system['width']-u, v, system['height']-v])
                valid &= np.isfinite(margin)
                minimum = np.minimum(minimum, np.where(valid, margin, -float('inf')))
                maximum_v = np.maximum(maximum_v, np.where(valid, v, -float('inf')))
                near_ok &= valid
    visible = near_ok & (minimum >= 0)
    worst = np.argsort(minimum, kind='stable')[:8]
    report = {'min_edge_margin_px': finite_number(minimum.min()),
              'worst_output_frame': int(minimum.argmin()),
              'worst_frames': [{'output_frame': int(i), 'min_edge_margin_px': finite_number(minimum[i])}
                               for i in worst],
              'in_frustum_ratio': float(np.mean(visible)), 'frames_checked': len(records),
              'out_of_frustum_frames': [int(i) for i in np.flatnonzero(~visible)],
              'near_plane_failed_frames': [int(i) for i in np.flatnonzero(~near_ok)],
              'bottom_max_px': finite_number(maximum_v.max()),
              'box_vertical_relative_to_entity_origin': [low, high], 'horizontal_half_width': 0.35,
              'measurement_scope': 'hypothetical_body_envelope_not_measured_pixels_or_bones'}
    if include_frames:
        report['frame_margins'] = [{'output_frame': i, 'min_edge_margin_px': finite_number(minimum[i]),
                                  'inside': bool(visible[i])} for i in range(len(records))]
    return report


def source_inputs(args):
    run, source, target = args.record_run_dir.resolve(), args.source_plan.resolve(), args.out_dir.resolve()
    require(not target.exists(), 'output already exists; choose a new directory, never overwrite evidence')
    require(not target.is_relative_to(source), 'output must not be inside the source plan')
    scenario_path = args.scenario.resolve()
    result_path = run / 'evidence/native-record-result.json'
    scenario = Scenario.load(scenario_path)
    result = load_json(result_path)
    require(result.get('ok') is True, 'native recording is incomplete or unsuccessful')
    master = result.get('master', {})
    required = {'plan.json', 'provenance.json', 'covis.json', 'covis-horizontal-proxy.json',
                'forward-plan-qc.json'} | {f'{kind}_{lane}.json' for lane in range(1, 5)
                                         for kind in ('action_camera', 'systeminfo')}
    require(source.is_dir() and all((source / name).is_file() for name in required),
            'fresh recorded plan is incomplete; all 13 generated files are required')
    before = {str(p): file_record(p) for p in source.iterdir() if p.is_file()}
    for p in (scenario_path, result_path, run/'source/master-player-telemetry.jsonl',
              run/'source/master-replay.clip', Path(__file__).resolve()):
        before[str(p)] = file_record(p)
    parent = load_json(source/'plan.json')
    require(parent.get('mode') == 'mc' and parent.get('duration_s') == 60.0 and parent.get('fps') == 30.0
            and scenario.duration_s == 60.0 and len(parent.get('cameras', [])) == 4,
            'requires a complete 60s/30fps/four-lane recorded MC plan')
    require('framing_derivation' not in parent, 'source plan already has an aim derivation; use the fresh base')
    require(parent.get('forward_adaptation', {}).get('choreography_mode') == 'independent_windows'
            and parent.get('cam4', {}).get('mode') == 'topdown',
            'requires existing independent_windows choreography and topdown lane4')
    motion = parent.get('subject_motion', {})
    require(motion.get('route_kind') == 'recorded_master_telemetry', 'idealized route is not a real recording')
    route = motion.get('recorded_route', {})
    diagnostics = validate_recorded_subject_route(route, scenario=scenario,
                                                 expected_replay_sha256=master.get('replay_sha256'))
    files = route['observation']['files']
    require(files['record_result']['sha256'] == before[str(result_path)]['sha256']
            and files['scenario']['sha256'] == before[str(scenario_path)]['sha256'],
            'plan route is not bound to the supplied recording/scenario bytes')
    for key, path, hash_key in (('telemetry', run/'source/master-player-telemetry.jsonl', 'player_telemetry_sha256'),
                                ('replay', run/'source/master-replay.clip', 'replay_sha256')):
        require(before[str(path)]['sha256'] == master.get(hash_key) == files[key]['sha256'],
                'record-run '+key+' differs from the complete recorded route')
    for r in files.values():
        before[r['path']] = file_record(Path(r['path']))
    anchors = _clip_anchor_track(parent)
    require(len(anchors) == len(route['waypoints']) == N_FRAMES, 'exactly 1800 observed/fitted frames required')
    require(all(abs(a['t'] - i/30) <= 1e-8 for i, a in enumerate(anchors)), 'plan anchor times differ from recording')
    subject = np.asarray([a['p'] for a in anchors], dtype=float)
    world = np.asarray([a['position'] for a in route['waypoints']], dtype=float)
    place = motion.get('placement', {})
    require(place.get('ground_y') == 0 and place.get('heading_deg') == 0,
            'this bounded helper supports ground-y0/heading0 recorded plans only')
    require(np.isfinite(subject).all() and np.isfinite(world).all()
            and np.allclose(place.get('anchor'), world[0], atol=1e-8, rtol=0), 'invalid recorded placement')
    expected = subject[0] + (world-world[0])[:, [0, 2, 1]]
    require(np.allclose(subject, expected, atol=1e-8, rtol=0)
            and abs(subject[0, 1]-route['observation']['look_at_height_m']) <= 1e-8,
            'plan anchors do not map to the actual fitted world route')
    entity = subject.copy()
    entity[:, 1] = world[:, 2]-world[0, 2]
    originals = {str(i): load_json(source/f'action_camera_{i}.json') for i in range(1, 5)}
    systems = {str(i): load_json(source/f'systeminfo_{i}.json') for i in range(1, 5)}
    require(all(len(r) == N_FRAMES for r in originals.values()), 'four complete 1800-frame camera tracks required')
    require({c['id'] for c in parent['cameras']} == {1, 2, 3, 4}, 'camera IDs must be exactly1..4')
    require(all(np.isfinite(np.asarray([f['camera_position'] for f in r], dtype=float)).all()
                for r in originals.values()), 'nonfinite camera positions')
    require(np.allclose(np.asarray([f['player_position'] for f in originals['1']]), entity,
                        atol=1e-6, rtol=0), 'planned entity positions differ from fitted recorded route')
    return run, source, target, parent, originals, systems, subject, entity, world, before, diagnostics


def derive(args):
    run, source, target, parent, originals, systems, subject, entity, world, before, diagnostics = source_inputs(args)
    records = copy.deepcopy(originals)
    for lane in ('1', '2', '3'):
        previous = None
        for i, rec in enumerate(records[lane]):
            aim = entity[i] + np.array([0.0, BASE_AIM_HEIGHT + SHIFTS[lane], 0.0])
            yaw, elev = yaw_elev_from_forward(vsub(aim, rec['camera_position']))
            rec['camera_rotation_quaternion'] = [round(v, 12) for v in quat_from_yaw_elev_roll(yaw, elev, 0)]
            if lane == '1':
                rec['camera_rotation_oula'] = [round(v, 6) for v in euler_tuple(yaw, elev, 0)]
                # Existing synthetic planning convention, never physical/captured input.
                rec['mouse_dx'] = round(clamp(wrap180(yaw-previous[0])/6, -1, 1), 6) if previous else 0.0
                rec['mouse_dy'] = round(clamp(-(elev-previous[1])/6, -1, 1), 6) if previous else 0.0
                previous = yaw, elev
    changed = {}
    for lane in records:
        changed[lane] = sorted({k for a, b in zip(originals[lane], records[lane]) for k in set(a)|set(b)
                                if a.get(k) != b.get(k)})
        allowed = {'camera_rotation_quaternion'} | ({'camera_rotation_oula', 'mouse_dx', 'mouse_dy'} if lane == '1' else set())
        require(set(changed[lane]) <= allowed and (lane != '4' or not changed[lane]),
                'unexpected change to camera/player positions, speed, intrinsics, timestamps or lane4')
    framing = {lane: _framing_report(r, subject, systems[lane], ground_y=0) for lane, r in records.items()}
    sensitivity = {label: {lane: {kind: project_body_envelope(r, entity, systems[lane], *bounds)
                           for kind, bounds in SENSITIVITY_BOUNDS.items()} for lane, r in data.items()}
                   for label, data in (('parent', originals), ('proposal', records))}
    joint = {lane: project_body_envelope(r, entity, systems[lane], *JOINT_BOUNDS, include_frames=True)
             for lane, r in records.items()}
    tracks = {int(lane): ([tuple(f['camera_position']) for f in r],
                         [tuple(f['camera_rotation_quaternion']) for f in r]) for lane, r in records.items()}
    times = [i/30 for i in range(0, N_FRAMES, 15)]
    views = {lane: [CamView(p=tracks[lane][0][i], q=tracks[lane][1][i],
                    **records[str(lane)][i]['camera_intrinsics'], width=systems[str(lane)]['width'],
                    height=systems[str(lane)]['height']) for i in range(0, N_FRAMES, 15)] for lane in (1, 2, 3)}
    horizontal = evaluate(Scene('gta-forward-planar-proxy', 'open_plaza', (-300, 0, -300), (300, 100, 300), 0), views, times).to_dict()
    horizontal.update(geometry_source='synthetic_planar_proxy', measured_in_gta=False)
    slope_views, plane_fit = _recorded_slope_proxy(views, subject, ground_y=0)
    covis = evaluate(Scene('gta-recorded-entity-slope-proxy', 'open_plaza', (-300, 0, -300), (300, 100, 300), 0), slope_views, times).to_dict()
    covis.update(geometry_source='synthetic_recorded_path_slope_proxy', plane_fit=plane_fit, measured_in_gta=False)
    derivation = {'schema': 'dm-gta-low-camera-aim-proposal/1', 'parent_plan': file_record(source/'plan.json'),
        'generator': file_record(Path(__file__)), 'record_run_dir': str(run),
        'aim_shift_gta_units_by_lane': SHIFTS,
        'aim_height_relative_to_smoothed_entity_origin': {k: BASE_AIM_HEIGHT+v for k, v in SHIFTS.items() if k != '4'},
        'aim_formula_world': 'smoothed_entity_z(frame)+0.925+shift(lane); lane4 remains byte-identical',
        'camera_positions_and_six_translation_tracks_unchanged': True, 'focals_and_intrinsics_unchanged': True,
        'subject_route_and_observation_unchanged': True, 'changed_record_fields': changed,
        'reason': 'Reuse the conservative body-framing hypothesis from prior port pixel review; this scene has no implied pixel approval.',
        'feet_offset_measured': False, 'terrain_measured': False, 'pixel_crop_fixed_verified': False,
        'assumptions': 'Joint entityZ[-1.2,+1.85], horizontal half-width0.35; no measured feet, bones, transverse slope or occlusion.',
        'planning_mouse_semantics': 'Synthetic planned angular control only; never physical input. Native metadata input evidence is not read or modified.',
        'calibration_approved': False, 'delivery_eligible': False, 'non_trainable': True}
    plan = copy.deepcopy(parent)
    plan['covis_windows'] = [{'t': t, 'pair_min_overlap': v} for t, v in covis['pair_min_overlap_ts']]
    plan['framing_derivation'] = derivation
    plan['forward_adaptation']['subject_framing'] = framing
    plan['forward_adaptation']['planning_plane_fit'] = plane_fit
    require(plan['cameras'] == parent['cameras'] and plan['subject_motion'] == parent['subject_motion']
            and plan['anchors'] == parent['anchors'] and plan['cam4'] == parent['cam4'], 'plan tracks or route changed')
    provenance = load_json(source/'provenance.json')
    provenance['framing_derivation'] = derivation
    provenance['source'] = plan['forward_adaptation']
    provenance['self_check'] = {'physics': {str(k): physics(*v, 1/30).to_dict() for k, v in tracks.items()},
        'subject_framing': framing, 'body_offset_sensitivity': sensitivity['proposal'],
        'decouple': decouple(tracks, 1/30).to_dict(), 'covis_ratio': covis['covis_ratio']}
    destination = target/'recorded-plan'
    target.mkdir(parents=True, exist_ok=False)
    destination.mkdir()
    for lane, r in records.items():
        if lane == '4': shutil.copyfile(source/f'action_camera_{lane}.json', destination/f'action_camera_{lane}.json')
        else: write_json_atomic(destination/f'action_camera_{lane}.json', r)
        shutil.copyfile(source/f'systeminfo_{lane}.json', destination/f'systeminfo_{lane}.json')
    hp = write_json_atomic(destination/'covis-horizontal-proxy.json', horizontal)
    covis['horizontal_reference'] = {'path': hp.name, 'covis_ratio': horizontal['covis_ratio'], 'result': horizontal,
        'sha256': sha256_file(hp), 'note': 'Original initial entity-height plane retained; no measured feet/terrain'}
    plan['forward_adaptation']['horizontal_proxy_diagnostic'] = covis['horizontal_reference']
    write_json_atomic(destination/'covis.json', covis)
    write_json_atomic(destination/'plan.json', plan)
    write_json_atomic(destination/'provenance.json', provenance)
    qc = check_clip(destination, 'mc', include_video=False)
    qc_report = {'verdict': qc.verdict, 'counts': qc.counts, 'results': [x.to_dict() for x in qc.results],
        'placement': plan['subject_motion']['placement'], 'recorded_route_diagnostics': diagnostics,
        'scope': 'Planning metadata only. Captured-input/animation/calibration/formal-approval limitations remain unchanged.'}
    write_json_atomic(destination/'forward-plan-qc.json', qc_report)
    joint_pass = all(r['in_frustum_ratio'] == 1.0 for r in joint.values())
    unchanged = all(file_record(Path(p)) == r for p, r in before.items())
    status = 'PASS' if qc.verdict == 'PASS' and joint_pass and unchanged else 'FAIL'
    joint_path = write_json_atomic(target/'joint-body-margin.json', {'schema': 'dm-gta-planning-joint-body-margin/1',
        'verdict': 'PASS' if joint_pass else 'FAIL', 'entity_origin_source': 'validated full recorded route, time-varying world Z',
        'frames_per_lane': N_FRAMES, 'lanes': joint, 'actual_pixel_visibility_verified': False})
    review = {'schema': 'dm-gta-body-framing-proposal-review/1', 'verdict': status,
        'old_inputs': before, 'derivation': derivation, 'recorded_route_diagnostics': diagnostics,
        'sensitivity': sensitivity, 'joint_body_margin': file_record(joint_path), 'qc': qc_report,
        'covis_ratio': covis['covis_ratio'], 'horizontal_covis_ratio': horizontal['covis_ratio'],
        'entity_world_z_range': [float(world[:, 2].min()), float(world[:, 2].max())],
        'all_old_inputs_rehashed_unchanged': unchanged, 'four_lane_positions_speeds_times_intrinsics_unchanged': True,
        'camera_segments_including_24_translations_unchanged': True, 'lane4_and_system_files_byte_identical': True,
        'new_tape_created': False, 'game_calls_performed': False,
        'human_approved': False, 'final_native_full_body_visibility_verified': False, 'delivery_eligible': False,
        'output_files': {p.name: file_record(p) for p in destination.iterdir() if p.is_file()}}
    write_json_atomic(target/'review.json', review)
    print(json.dumps({'verdict': status, 'output': str(target), 'plan': str(destination), 'qc': qc.counts,
        'joint_margins_px': {k: v['min_edge_margin_px'] for k, v in joint.items()},
        'joint_worst_frames': {k: v['worst_output_frame'] for k, v in joint.items()},
        'all_old_inputs_rehashed_unchanged': unchanged}, ensure_ascii=False))
    return 0 if status == 'PASS' else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--record-run-dir', type=Path, required=True)
    parser.add_argument('--scenario', type=Path, required=True)
    parser.add_argument('--source-plan', type=Path, required=True)
    parser.add_argument('--out-dir', type=Path, required=True)
    args = parser.parse_args()
    try:
        return derive(args)
    except (ContractError, OSError, ValueError, KeyError, TypeError) as exc:
        print(json.dumps({'verdict': 'FAIL', 'error': str(exc),
                          'partial_output_must_be_retained_if_present': True}), file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
