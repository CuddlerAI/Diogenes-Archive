"""Install this already reviewed local build while GTA is closed; preserve old ASI."""
import copy
from dataclasses import asdict
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
import subprocess
import sys

BASE = Path(__file__).resolve().parent
CAPTURE = BASE.parents[1]
sys.path.insert(0, str(CAPTURE))
from gta_capture.jsonio import load_json, sha256_file, write_json_atomic
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt, run_preflight

DERIVED = BASE / "controller-editor-wait-60min-derived-01"
CANDIDATE_MANIFEST = DERIVED / "manifest-source-review.json"
INSTALLED_MANIFEST = DERIVED / "manifest-installed.json"
REPORT = DERIVED / "installation-receipt.json"
PREFLIGHT = DERIVED / "preflight-installed.json"
GAME = Path("C:/Program Files (x86)/Steam/steamapps/common/Grand Theft Auto V")
TARGET = GAME / "DiogenesGtaController.asi"
BACKUP = BASE / "controller-editor-wait-60min-preserve-01/DiogenesGtaController-before.asi"
OLD_SHA = "7335a705929bf0c397dad2e57bab7d4a3743c0c39b1cf1895393de7ba8d8be15"
NEW_SHA = "0f1c1f819b96875e0a5c91bc67c0fed37834a65649d18eb24e448f21d6411cee"
REPLUS_SHA = "ac7d8ce5a53df9439af57c28babc0445c685c5ee705976065fbb054c1ba3eb3f"
assert all(not p.exists() for p in (INSTALLED_MANIFEST, REPORT, PREFLIGHT))
assert sha256_file(CANDIDATE_MANIFEST) == "eef9f9d57a9d418b45b763f61f111a57a31aa5d125345b92531b30d096b77fc7"
assert sha256_file(TARGET) == sha256_file(BACKUP) == OLD_SHA
assert sha256_file(GAME / "RockstarEditorPlus.asi") == REPLUS_SHA
manifest = DependencyManifest.load(CANDIDATE_MANIFEST)
candidate = manifest.component_path("diogenes_controller_asi")
assert sha256_file(candidate) == NEW_SHA
verified = _check_capture_build_receipt(manifest)
assert verified.status == "PASS", verified
count = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", "@(Get-Process GTA5 -ErrorAction SilentlyContinue).Count"], capture_output=True, text=True, check=True)
assert count.stdout.strip() == "0", "GTA must be closed before installing its ASI."
receipt = {"schema": "dm-gta-controller-local-install/1", "started_at": datetime.now(timezone.utc).isoformat(),
    "source_manifest_sha256": sha256_file(CANDIDATE_MANIFEST), "source_check": asdict(verified),
    "old_backup": str(BACKUP), "old_sha256": OLD_SHA, "new_sha256": NEW_SHA,
    "target": str(TARGET), "game_process_count_before_copy": 0, "game_launched": False,
    "runtime_verified": False, "formal_delivery_eligible": False}
try:
    shutil.copyfile(candidate, TARGET)
    assert sha256_file(TARGET) == NEW_SHA
    assert sha256_file(BACKUP) == OLD_SHA
    assert sha256_file(GAME / "RockstarEditorPlus.asi") == REPLUS_SHA
    installed = copy.deepcopy(manifest.raw)
    installed["components"]["diogenes_controller_asi"]["path"] = str(TARGET.resolve())
    write_json_atomic(INSTALLED_MANIFEST, installed)
    report = run_preflight(DependencyManifest.load(INSTALLED_MANIFEST), mode="real")
    write_json_atomic(PREFLIGHT, report.to_dict())
    failed = {item.check_id for item in report.checks if item.status == "FAIL"}
    assert failed == {"calibration.binding", "calibration.projection_evidence", "calibration.projection_contract"}, report.to_dict()
    assert next(item for item in report.checks if item.check_id == "dependencies.capture_build_receipt").status == "PASS"
    receipt.update(status="installed_and_source_verified", manifest_path=str(INSTALLED_MANIFEST),
        manifest_sha256=sha256_file(INSTALLED_MANIFEST), preflight_sha256=sha256_file(PREFLIGHT),
        pass_count=sum(item.status == "PASS" for item in report.checks), retained_calibration_failures=sorted(failed),
        replus_sha256_unchanged=REPLUS_SHA, rollback_performed=False)
except Exception as error:
    shutil.copyfile(BACKUP, TARGET)
    assert sha256_file(TARGET) == OLD_SHA
    receipt.update(status="installation_failed_rolled_back", rollback_performed=True, error=str(error))
    raise
finally:
    receipt["finished_at"] = datetime.now(timezone.utc).isoformat()
    write_json_atomic(REPORT, receipt)
    print(json.dumps(receipt, ensure_ascii=False))
