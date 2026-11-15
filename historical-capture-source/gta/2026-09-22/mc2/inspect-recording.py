import argparse
import json
from pathlib import Path
from gta_capture.recorded_route import build_recorded_subject_route
from gta_capture.jsonio import write_json_atomic

p = argparse.ArgumentParser()
p.add_argument('run_dir', type=Path)
args = p.parse_args()
route = build_recorded_subject_route(
    args.run_dir / 'evidence/native-record-result.json',
    args.run_dir / 'config/scenario.json', look_at_height_m=0.925)
result = {'native_command': route['native_command'], 'diagnostics': route['diagnostics'],
          'sampled_route': route['waypoints'][::30],
          'z_range': [min(x['position'][2] for x in route['waypoints']),
                      max(x['position'][2] for x in route['waypoints'])]}
write_json_atomic(args.run_dir / 'evidence/recorded-route-inspection.json', result)
print(json.dumps(result, indent=2))
