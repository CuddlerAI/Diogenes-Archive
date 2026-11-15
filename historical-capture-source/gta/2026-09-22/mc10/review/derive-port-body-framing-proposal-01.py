"""Offline planning proposal only; never writes old inputs or creates a tape."""
from pathlib import Path
import copy
import hashlib
import json
import math
import shutil
import sys

PACKAGE = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(PACKAGE))
from gta_capture.moving_plan import _dependencies, _clip_anchor_track, _framing_report, _recorded_slope_proxy
from gta_capture.jsonio import file_record, sha256_file, write_json_atomic
_dependencies()
import numpy as np
from trajgen.mathutil import quat_from_yaw_elev_roll, yaw_elev_from_forward, vsub, euler_tuple, wrap180, clamp
from trajgen.metrics import physics, decouple
from trajgen.covis import CamView, evaluate
from trajgen.scene import Scene
from engine_qc.clip_checker import check_clip

RUN = PACKAGE / 'out/mc10-20260908/scene02-port/run-02'
SOURCE = RUN / 'config/recorded-plan'
TARGET = Path(__file__).resolve().parent / 'port-run02-framing-proposal-01'
SHIFTS = {'1': -0.60, '2': -0.50, '3': -0.35}


def body_envelope(records, subject, low, high):
    positions = np.array([f['camera_position'] for f in records])
    q = np.array([f['camera_rotation_quaternion'] for f in records])
    q /= np.linalg.norm(q, axis=1)[:, None]
    x, y, z, w = q.T
    right = np.array([1-2*(y*y+z*z), 2*(x*y+z*w), 2*(x*z-y*w)]).T
    up = np.array([2*(x*y-z*w), 1-2*(x*x+z*z), 2*(y*z+x*w)]).T
    forward = np.array([2*(x*z+y*w), 2*(y*z-x*w), 1-2*(x*x+y*y)]).T
    intr = records[0]['camera_intrinsics']
    minimum = np.full(len(records), float('inf'))
    maximum_v = np.full(len(records), -float('inf'))
    for dx in (-.35, .35):
        for dz in (-.35, .35):
            for dy in (low, high):
                points = subject.copy()
                points[:, 0] += dx
                points[:, 2] += dz
                points[:, 1] = subject[:, 1] - subject[0, 1] + dy
                delta = points - positions
                depth = np.sum(delta * forward, axis=1)
                u = intr['cx'] + intr['fx'] * np.sum(delta * right, axis=1) / depth
                v = intr['cy'] - intr['fy'] * np.sum(delta * up, axis=1) / depth
                margin = np.minimum.reduce([u, 1920-u, v, 1080-v])
                margin[depth <= .05] = -float('inf')
                minimum = np.minimum(minimum, margin)
                maximum_v = np.maximum(maximum_v, v)
    return {'min_edge_margin_px': float(minimum.min()),
            'worst_output_frame': int(minimum.argmin()),
            'in_frustum_ratio': float(np.mean(minimum >= 0)),
            'bottom_max_px': float(maximum_v.max()),
            'box_vertical_relative_to_entity_origin': [low, high],
            'horizontal_half_width': .35,
            'measurement_scope': 'hypothetical_body_envelope_not_measured_pixels_or_bones'}


def main():
    if TARGET.exists():
        raise RuntimeError('Proposal already exists; refusing overwrite')
    parent = json.loads((SOURCE / 'plan.json').read_text(encoding='utf8'))
    originals = {str(i): json.loads((SOURCE / f'action_camera_{i}.json').read_text(encoding='utf8')) for i in range(1, 5)}
    inputs = {str(p): file_record(p) for p in SOURCE.iterdir() if p.is_file()}
    for p in (RUN/'config/camera-tape.json', RUN/'config/scenario.json', RUN/'evidence/native-record-result.json',
              RUN/'source/master-player-telemetry.jsonl', RUN/'source/master-replay.clip',
              RUN/'evidence/frame-events-lane1.jsonl', RUN/'evidence/frame-events-lane2.jsonl'):
        inputs[str(p)] = file_record(p)
    if inputs[str(RUN/'source/master-replay.clip')]['sha256'] != 'e9328cf9061815d49c9b2be2419e87d1f39348c9c74499c24c762a6abdb8fdf8':
        raise RuntimeError('Wrong master')
    if inputs[str(RUN/'source/master-player-telemetry.jsonl')]['sha256'] != 'f77b8281268f00bcd0b68c0931e042e97337f5f731dc7666e46e151e45e0893d':
        raise RuntimeError('Wrong telemetry')
    subject = np.array([a['p'] for a in _clip_anchor_track(parent)])
    records = copy.deepcopy(originals)
    for lane, shift in SHIFTS.items():
        previous = None
        for frame, rec in enumerate(records[lane]):
            target = (subject[frame, 0], subject[frame, 1]-subject[0, 1]+.925+shift, subject[frame, 2])
            yaw, elev = yaw_elev_from_forward(vsub(target, rec['camera_position']))
            rec['camera_rotation_quaternion'] = [round(x, 12) for x in quat_from_yaw_elev_roll(yaw, elev, 0)]
            if lane == '1':
                rec['camera_rotation_oula'] = [round(x, 6) for x in euler_tuple(yaw, elev, 0)]
                # Planning convention only, exactly as moving_plan. Never copied into native capture input evidence.
                rec['mouse_dx'] = round(clamp(wrap180(yaw-previous[0])/6, -1, 1), 6) if previous else 0.0
                rec['mouse_dy'] = round(clamp(-(elev-previous[1])/6, -1, 1), 6) if previous else 0.0
                previous = (yaw, elev)
    changed_fields = {}
    for lane in records:
        changed_fields[lane] = sorted({k for before, after in zip(originals[lane], records[lane]) for k in before if before[k] != after[k]})
        allowed = {'camera_rotation_quaternion'} | ({'camera_rotation_oula', 'mouse_dx', 'mouse_dy'} if lane == '1' else set())
        if not set(changed_fields[lane]) <= allowed:
            raise RuntimeError('Unexpected data modification')
    systems = {str(i): json.loads((SOURCE/f'systeminfo_{i}.json').read_text()) for i in range(1,5)}
    framing = {lane: _framing_report(rec, subject, systems[lane], ground_y=0) for lane, rec in records.items()}
    sensitivity = {label: {lane: {
        'original_proxy': body_envelope(rec, subject, 0, 1.85),
        'feet_minus1': body_envelope(rec, subject, -1, .85),
        'union_of_all_offsets_minus1point2_to_zero': body_envelope(rec, subject, -1.2, 1.85)
    } for lane, rec in data.items()} for label, data in [('parent', originals), ('proposal', records)]}
    tracks = {int(lane): ([tuple(f['camera_position']) for f in rec], [tuple(f['camera_rotation_quaternion']) for f in rec]) for lane,rec in records.items()}
    times = [f/30 for f in range(0,1800,15)]
    views = {lane: [CamView(p=tracks[lane][0][f], q=tracks[lane][1][f], **records[str(lane)][f]['camera_intrinsics'], width=1920, height=1080) for f in range(0,1800,15)] for lane in (1,2,3)}
    horizontal = evaluate(Scene('gta-forward-planar-proxy','open_plaza',(-300,0,-300),(300,100,300),0),views,times).to_dict()
    horizontal.update(geometry_source='synthetic_planar_proxy', measured_in_gta=False)
    slope_views, plane_fit = _recorded_slope_proxy(views, subject, ground_y=0)
    covis = evaluate(Scene('gta-recorded-entity-slope-proxy','open_plaza',(-300,0,-300),(300,100,300),0),slope_views,times).to_dict()
    covis.update(geometry_source='synthetic_recorded_path_slope_proxy', plane_fit=plane_fit, measured_in_gta=False)
    derivation = {'schema':'dm-gta-low-camera-aim-proposal/1', 'parent_plan':file_record(SOURCE/'plan.json'),
        'generator':file_record(Path(__file__)), 'aim_shift_gta_units_by_lane':SHIFTS,
        'aim_height_relative_to_smoothed_entity_origin':{k:.925+v for k,v in SHIFTS.items()},
        'camera_positions_and_six_translation_tracks_unchanged':True, 'focals_and_intrinsics_unchanged':True,
        'subject_route_and_observation_unchanged':True, 'changed_record_fields':changed_fields,
        'reason':'Observed raw lane2 frame1205 shoe reaches last image row; origin-as-feet proxy is inadequate',
        'feet_offset_measured':False, 'terrain_measured':False, 'pixel_crop_fixed_verified':False,
        'assumptions':'Envelope includes possible feet offsets -1.2..0 with 1.85-height/0.7-width box; inferred sensitivity, not bone calibration',
        'planning_mouse_semantics':'Recomputed synthetic planned angular control convention only; no physical input observation. Native captured writer input remains unchanged and cannot become a genuine mouse stream.',
        'calibration_approved':False, 'delivery_eligible':False, 'non_trainable':True}
    plan = copy.deepcopy(parent)
    plan['covis_windows'] = [{'t': t, 'pair_min_overlap': v} for t,v in covis['pair_min_overlap_ts']]
    plan['framing_derivation'] = derivation
    plan['forward_adaptation']['subject_framing'] = framing
    plan['forward_adaptation']['planning_plane_fit'] = plane_fit
    provenance = json.loads((SOURCE/'provenance.json').read_text())
    provenance['framing_derivation'] = derivation
    provenance['source'] = plan['forward_adaptation']
    provenance['self_check'] = {'physics':{str(k):physics(*v,1/30).to_dict() for k,v in tracks.items()},
        'subject_framing':framing, 'body_offset_sensitivity':sensitivity['proposal'],
        'decouple':decouple(tracks,1/30).to_dict(),'covis_ratio':covis['covis_ratio']}
    destination=TARGET/'recorded-plan'
    destination.mkdir(parents=True)
    for lane, rec in records.items():
        if lane == '4': shutil.copyfile(SOURCE/f'action_camera_{lane}.json', destination/f'action_camera_{lane}.json')
        else: write_json_atomic(destination/f'action_camera_{lane}.json', rec)
        shutil.copyfile(SOURCE/f'systeminfo_{lane}.json', destination/f'systeminfo_{lane}.json')
    hp=write_json_atomic(destination/'covis-horizontal-proxy.json',horizontal)
    covis['horizontal_reference']={'path':'covis-horizontal-proxy.json','covis_ratio':horizontal['covis_ratio'], 'result':horizontal, 'sha256':sha256_file(hp), 'note':'Unchanged entity-origin plane assumption; no measured feet/terrain'}
    plan['forward_adaptation']['horizontal_proxy_diagnostic']=covis['horizontal_reference']
    write_json_atomic(destination/'covis.json',covis)
    write_json_atomic(destination/'plan.json',plan)
    write_json_atomic(destination/'provenance.json',provenance)
    qc=check_clip(destination,'mc',include_video=False)
    report={'verdict':qc.verdict,'counts':qc.counts,'results':[x.to_dict() for x in qc.results],
            'scope':'Future planning proposal only; real old native input FAIL and all formal limitations unchanged'}
    write_json_atomic(destination/'forward-plan-qc.json',report)
    for p, record in inputs.items():
        if file_record(Path(p)) != record: raise RuntimeError('Old sealed source changed: '+p)
    review={'schema':'dm-gta-body-framing-proposal-review/1','old_inputs':inputs,'derivation':derivation,
        'sensitivity':sensitivity,'qc':report,'covis_ratio':covis['covis_ratio'],'horizontal_covis_ratio':horizontal['covis_ratio'],
        'all_old_inputs_rehashed_unchanged':True,'new_tape_created':False,'game_calls_performed':False,
        'final_native_full_body_visibility_verified':False}
    write_json_atomic(TARGET/'review.json',review)
    print(json.dumps({'proposal':str(TARGET),'qc':report['counts'],'verdict':report['verdict'],
        'sensitivity':sensitivity['proposal'],'covis':covis['covis_ratio'],'horizontal':horizontal['covis_ratio']},ensure_ascii=False))


if __name__=='__main__': main()
