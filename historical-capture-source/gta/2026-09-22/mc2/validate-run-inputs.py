import json
import sys
from pathlib import Path
from gta_capture.preflight import DependencyManifest
from gta_capture.scenario import Scenario
from gta_capture.staged import _validate_real_inputs
from gta_capture.tape import load_tape

root = Path(sys.argv[1]).resolve()
_validate_real_inputs(
    DependencyManifest.load(root / 'config/dependency-manifest.json'),
    Scenario.load(root / 'config/scenario.json'),
    load_tape(root / 'config/camera-tape.json'), root,
    allow_internal_calibration_gaps=True)
report = json.loads((root / 'evidence/preflight.json').read_text(encoding='utf-8'))
print(json.dumps({'route_and_source_binding_validated': True,
                  'preflight_counts': {state: sum(c['status'] == state for c in report['checks'])
                                       for state in ('PASS', 'FAIL', 'SKIP')},
                  'retained_failures': [c['id'] for c in report['checks'] if c['status'] == 'FAIL'],
                  'internal_artifact_materialization': report['internal_artifact_materialization']}, indent=2))
