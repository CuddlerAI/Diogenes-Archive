"""Install the reviewed controller locally, preserving its verified parent bytes."""
import copy
import json
import os
from pathlib import Path
import shutil
import subprocess

from gta_capture.jsonio import load_json, sha256_file, write_json_atomic
from gta_capture.preflight import DependencyManifest, run_preflight

ROOT = Path(__file__).resolve().parent
STAGE = ROOT / 'controller-derived-v2-final'
candidate_path = STAGE / 'manifest-source-review.json'
candidate = load_json(candidate_path)
receipt = load_json(Path(candidate['components']['capture_build_receipt']['path']))
parent = load_json(Path(receipt['parent']['manifest']['path']))
old_record = receipt['parent']['controller_backup']
source = Path(candidate['components']['diogenes_controller_asi']['path'])
target = Path(parent['components']['diogenes_controller_asi']['path'])
new_manifest = STAGE / 'manifest-installed.json'
if new_manifest.exists():
    raise RuntimeError('Refusing to overwrite an existing installed manifest')
check = subprocess.run(['powershell.exe', '-NoProfile', '-Command',
                        'if (Get-Process GTA5 -ErrorAction SilentlyContinue) { exit 1 }'],
                       capture_output=True)
if check.returncode:
    raise RuntimeError('GTA5 must be closed before changing its controller')
if (sha256_file(target) != old_record['sha256']
        or sha256_file(Path(old_record['path'])) != old_record['sha256']
        or sha256_file(source) != candidate['components']['diogenes_controller_asi']['sha256']):
    raise RuntimeError('Installed, backup, or candidate controller bytes changed')
review = run_preflight(DependencyManifest.load(candidate_path), mode='real')
review_dict = review.to_dict()
allowed_before = {'calibration.binding', 'calibration.projection_evidence',
                  'calibration.projection_contract', 'dependencies.asi_allowlist'}
failed = {row['id'] for row in review_dict['checks'] if row['status'] == 'FAIL'}
if failed != allowed_before:
    raise RuntimeError('Unexpected pre-install preflight results: ' + str(failed))
installed = copy.deepcopy(candidate)
installed['components']['diogenes_controller_asi']['path'] = str(target)
temporary = target.with_suffix('.asi.mc2-staged')
if temporary.exists():
    raise RuntimeError('Staging destination already exists')
shutil.copyfile(source, temporary)
if sha256_file(temporary) != candidate['components']['diogenes_controller_asi']['sha256']:
    raise RuntimeError('Staged controller copy differs')
os.replace(temporary, target)
write_json_atomic(new_manifest, installed)
post = run_preflight(DependencyManifest.load(new_manifest), mode='real').to_dict()
write_json_atomic(STAGE / 'preflight-installed.json', post)
failed_after = {row['id'] for row in post['checks'] if row['status'] == 'FAIL'}
expected_after = allowed_before - {'dependencies.asi_allowlist'}
if failed_after != expected_after:
    shutil.copyfile(Path(old_record['path']), target)
    write_json_atomic(STAGE / 'installation-result.json', {
        'installed': False, 'rolled_back': True, 'failed': sorted(failed_after)})
    raise RuntimeError('Post-install preflight failed; original controller restored')
result = {'installed': True, 'runtime_tested': False,
          'manifest': str(new_manifest), 'controller_sha256': sha256_file(target),
          'parent_backup': old_record, 'remaining_failures': sorted(failed_after)}
write_json_atomic(STAGE / 'installation-result.json', result)
print(json.dumps(result, indent=2))
