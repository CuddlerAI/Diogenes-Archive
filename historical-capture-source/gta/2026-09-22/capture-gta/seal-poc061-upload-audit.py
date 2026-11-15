"""Freeze the five verified POC061 transfer receipts outside capture sources."""
from pathlib import Path
import hashlib
import importlib.util
import json
import shutil
import sys

ROOT = Path(r"C:\capture\gta")
DEST = "r2:diogenes-tempbucket/pr852-gta5-legacy-pr405-non-trainable-20260828-115407-7c6c8ac1/poc061"
OUT = ROOT / "upload-audit" / "poc061"
SCRIPT = Path(r"C:\Users\researcher\Documents\Codex\Diogenes\.agents\skills\upload-r2-high-concurrency\scripts\upload_r2.py")
spec = importlib.util.spec_from_file_location("poc061_upload_inventory", SCRIPT)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

expected = {
    "lane1": (452, 1597083560, ROOT / "replus-output" / "render_0037", "raw/lane1"),
    "lane2": (452, 1544629597, ROOT / "replus-output" / "render_0038", "raw/lane2"),
    "lane3": (452, 1553068581, ROOT / "replus-output" / "render_0039", "raw/lane3"),
    "lane4": (452, 1653329067, ROOT / "replus-output" / "render_0040", "raw/lane4"),
    "bundle": (71, 262162107, ROOT / "runs" / "poc-061", "run"),
}
receipts = {}
copy_sources = []
for part, (count, size, source, suffix) in expected.items():
    directory = ROOT / "upload-reports" / f"poc061-{part}"
    report_path = directory / "upload-report.json"
    check_path = directory / "rclone-check.txt"
    payload = report_path.read_bytes()
    report = json.loads(payload)
    before = report["source_before"]
    if (report["status"] != "verified" or report["copy_exit_code"] != 0
            or report["check_exit_code"] != 0 or report["source_after"] != before
            or before["count"] != count or before["bytes"] != size
            or report["destination_after"] != {"count": count, "bytes": size}
            or report["destination"] != f"{DEST}/{suffix}"
            or Path(report["source"]).resolve() != source.resolve()):
        raise SystemExit(f"Receipt mismatch: {part}")
    current = module.dataclasses.asdict(module.inventory(source))
    if current != before:
        raise SystemExit(f"Source changed after upload: {part}")
    check_bytes = check_path.read_bytes()
    check_lines = check_bytes.decode("utf-8").splitlines()
    if len(check_lines) != count or any(not line.startswith("= ") for line in check_lines):
        raise SystemExit(f"Check output has missing/nonmatching objects: {part}")
    receipts[part] = {
        "destination": report["destination"], "status": "verified",
        "count": count, "bytes": size, "copy_exit_code": 0, "check_exit_code": 0,
        "source_unchanged_during_and_after_upload": True,
        "source_metadata_sha256": before["metadata_sha256"],
        "finished_at": report["finished_at"],
        "receipt_file": f"{part}/upload-report.json",
        "receipt_sha256": hashlib.sha256(payload).hexdigest(),
        "check_file": f"{part}/rclone-check.txt",
        "check_sha256": hashlib.sha256(check_bytes).hexdigest(),
    }
    copy_sources.append((part, report_path, check_path))

if OUT.exists():
    raise SystemExit(f"Audit already exists; do not overwrite: {OUT}")
OUT.mkdir(parents=True)
for part, report_path, check_path in copy_sources:
    (OUT / part).mkdir()
    shutil.copyfile(report_path, OUT / part / report_path.name)
    shutil.copyfile(check_path, OUT / part / check_path.name)
summary = {
    "schema": "gta-poc061-r2-handoff/1",
    "transfer_status": "verified",
    "destination_root": DEST,
    "data_object_count_excluding_audit": sum(value[0] for value in expected.values()),
    "data_bytes_excluding_audit": sum(value[1] for value in expected.values()),
    "run_id": "9097766f-413f-42a7-8116-108dceaf0fc2",
    "native_capture_commit": "7341e31bfa65c690d70b76077bf84afd975fcb41",
    "non_trainable": True, "production_upload_enabled": False,
    "formal_pr405_pass": False, "human_review_approval": False,
    "qc": {"PASS": 114, "FAIL": 1, "SKIP": 17},
    "formal_gate_missing_rule_count": 15,
    "scope": "15-second internal Story Mode diagnostic sample; transfer integrity is not quality or rights approval",
    "parts": receipts,
}
(OUT / "handoff-summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
(OUT / "README.md").write_text(
    "# POC061 R2 transfer audit\n\n"
    "All five original transfers are verified; copy/check returned zero, source inventories match, "
    "remote counts/bytes match, and a fresh source inventory still matches. "
    "The summary binds the original receipts and per-object check outputs by SHA-256.\n\n"
    "Data: 1,879 objects / 6,610,272,912 bytes, excluding this audit. "
    "capture/ contains the twelve sample files; preview/ contains the four-view MP4 and contact sheets.\n\n"
    "This is a 15-second non-trainable internal sample, NOT a final PR405-qualified dataset. "
    "QC remains 114 PASS / 1 FAIL / 17 SKIP, with 15 formal rules missing. "
    "No quality, human-review, calibration, commercial-rights or production-upload gate is granted.\n\n"
    "This audit upload is independently checked by an external upload report; "
    "the summary does not recursively claim its own transfer has completed.\n",
    encoding="utf-8")
print(json.dumps({"audit_dir": str(OUT), "data_object_count": summary["data_object_count_excluding_audit"],
                  "data_bytes": summary["data_bytes_excluding_audit"], "audit_file_count": len(list(OUT.rglob("*.*")))}, indent=2))
