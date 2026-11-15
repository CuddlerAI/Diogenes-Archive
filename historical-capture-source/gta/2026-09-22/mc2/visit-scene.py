"""Local operator helper: visit a candidate only through the existing Story Mode API."""
import argparse
import json
import time
from pathlib import Path
from gta_capture.protocol import JsonLinePipeClient
from gta_capture.scenario import Scenario
from gta_capture.jsonio import sha256_file, write_json_atomic

p = argparse.ArgumentParser()
p.add_argument('--scenario', required=True)
p.add_argument('--out', required=True)
args = p.parse_args()
scenario = Scenario.load(args.scenario)
client = JsonLinePipeClient()
try:
    hello = client.request('hello')
    if hello.get('story_mode') is not True or hello.get('online_session') is not False:
        raise RuntimeError('Scene visit requires Story Mode and no Online session')
    before = client.request('probe_state')
    applied = client.request('apply_scenario', {
        'scenario_path': str(scenario.path), 'scenario_sha256': sha256_file(scenario.path)})
    time.sleep(4)
    after = client.request('probe_state')
finally:
    client.close()
result = {'scope': 'candidate visit, not a terrain/visibility certification',
          'before': before, 'applied': applied, 'after': after}
write_json_atomic(Path(args.out), result)
print(json.dumps(result, indent=2))
