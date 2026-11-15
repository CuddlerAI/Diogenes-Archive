"""Write new ignored draft config and attributed scout observations; no game calls."""
from pathlib import Path
import copy
import hashlib
import json
import sys
sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
ROOT = BASE.parents[1]
sys.path.insert(0, str(ROOT))
from gta_capture.scenario import validate_scenario

def ref(path):
    path = Path(path).resolve()
    data = path.read_bytes()
    return {'path': str(path), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

def read(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))

def write(path, value):
    with Path(path).open('x', encoding='utf-8') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')

draft = BASE / 'drawmovie-guard-smoke12-attempt-01'
scout_output = BASE / 'review/mckenzie-root-scout-observations-01.json'
if draft.exists() or scout_output.exists():
    raise SystemExit('preserve existing outputs; choose a new attempt')
source = BASE / 'scene02-port/run-02-reexport-01/config/scenario.json'
scenario = copy.deepcopy(read(source))
scenario['scenario_id'] = 'gta-drawmovie-guard-smoke12-port-attempt01-20260909'
scenario['seed'] = 2026090912
scenario['replay']['duration_s'] = 12.0
validate_scenario(scenario)
draft.mkdir()
write(draft / 'scenario.json', scenario)
write(draft / 'draft-receipt.json', {
    'schema': 'dm-gta-drawmovie-smoke-draft/1', 'planning_only': True,
    'base_scenario': ref(source), 'scenario': ref(draft / 'scenario.json'),
    'preserved': ['location', 'clock', 'weather', 'player command', 'population', '90s warmup'],
    'changed': ['scenario_id', 'seed', 'duration_s=12'],
    'historical_anchor_not_resurveyed_in_new_process': True,
    'new_master_recorded': False, 'new_master_sha256': None, 'new_png_frames_observed': 0,
    'new_component_manifest': None, 'formal_quality_approved': False,
    'formal_calibration_approval': False, 'delivery_eligible': False,
    'record_argv_not_executed': [sys.executable, '-m', 'gta_capture', 'native-record-smoke',
        '--scenario', str(draft / 'scenario.json'), '--run-dir', str(draft / 'run')],
    'minimum_lanes_to_export_sequentially': [1, 2], 'expected_real_frames_per_lane': 360,
    'reason': 'Previous lane2 cursor first appeared at frame197 (6.566666667s), beyond the old 150-frame transfer clip.'})

scout_base = BASE / 'scouting-04-10-candidates-01'
reported = {
    ('scene04-mckenzie', 'mid'): {'description': '机库入口、油罐、灌木和围栏；根代理判断不足以作为100米路线。',
        'decision': 'candidate_corridor_not_suitable_as_observed',
        'distance_100m_measured': False},
    ('scene04-mckenzie', 'start'): {'description': '位置紧邻 BI-FLYUS 机库墙。',
        'decision': 'near_hangar_wall_observed'},
    ('scene04-mckenzie-route02', 'mid'): {'description': '草地、白栅栏、农棚或筒仓；不是跑道。',
        'decision': 'route02_rejected'}
}
points = []
for route in ('scene04-mckenzie', 'scene04-mckenzie-route02'):
    for point in ('start', 'quarter', 'mid', 'threequarter', 'end'):
        scenario_path = scout_base / route / (point + '-scenario.json')
        planned = read(scenario_path)
        item = {'candidate': route, 'point': point, 'scenario': ref(scenario_path),
                'requested_position_world': planned['location'],
                'requested_heading_deg': planned['player']['heading_deg'],
                'coordinate_semantics': 'GTA native world XYZ, Z up; entity origin is not measured ground/feet',
                'visit_status': 'not_visited_in_the_three_reported_visits',
                'observed_position_world': None, 'root_visual_observation': None}
        if (route, point) in reported:
            visit_path = scout_base / route / (point + '-visit-01.json')
            visit = read(visit_path)
            if (visit['applied']['scenario_sha256'] != item['scenario']['sha256']
                or visit['applied']['applied'] is not True or visit['after']['story_mode'] is not True
                or visit['after']['online_session'] is not False or visit['after']['mock'] is not False):
                raise SystemExit('actual visit/scenario binding failed: ' + str(visit_path))
            item.update({'visit_status': 'actual_visit_with_root_reported_UI_observation',
                         'visit': ref(visit_path), 'observed_position_world': visit['after']['player_position'],
                         'observed_heading_deg': visit['after']['player_heading_deg'],
                         'root_visual_observation': {'observer': 'root AI agent',
                             'attribution': 'explicit task message reporting root live UI observation',
                             'independently_viewed_by_receipt_writer': False, **reported[(route, point)]}})
        points.append(item)
scout_output.parent.mkdir(parents=True, exist_ok=True)
write(scout_output, {'schema': 'dm-gta-root-scout-observations/1',
    'scope': 'Only the three explicit root live UI observations, bound to actual visit/scenario files.',
    'actual_reported_visit_count': 3, 'planned_unvisited_point_count': 7, 'points': points,
    'route02_selected': False, 'route02_status': 'rejected_by_root_observation',
    'new_recordings_proven_by_these_visits': 0,
    'limits': ['Before/after probe master binding may refer to the older port clip; it is not evidence of a new McKenzie recording.',
               '100m suitability is a reported visual judgment, not a surveyed distance or terrain clearance result.',
               'Unvisited points retain only their planned coordinates; nearby visited points do not establish their safety.',
               'No screenshot bytes were provided for this receipt; no image hash or independent image review is invented.'],
    'formal_human_approval': False, 'terrain_verified': False, 'geometry_scale_approved': False,
    'formal_quality_approved': False, 'delivery_eligible': False})
print(json.dumps({'draft': str(draft), 'scout_receipt': ref(scout_output)}, ensure_ascii=False))
