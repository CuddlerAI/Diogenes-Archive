"""Invoke exactly one existing MC2 stage, with strict checks and a unique log.

Examples (use the bundled Python 3.10+ runtime):
  invoke-mc2-stage.py --run-dir RUN arm --lane 2
  invoke-mc2-stage.py --run-dir RUN collect --lane 2
  invoke-mc2-stage.py --run-dir RUN audio --duration 60
  invoke-mc2-stage.py --run-dir RUN finalize --materialize-internal-poc-with-calibration-gaps

This helper does not drive UI, record a new master, loop through lanes, change
acceptance criteria, or automatically perform audio/finalize. The operator must
complete all four lanes before explicitly requesting audio or finalize.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import sys
import traceback
import uuid


HERE = Path(__file__).resolve().parent
PACKAGE_ROOT = HERE.parents[1]


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run-dir", required=True, type=Path)
    stages = parser.add_subparsers(dest="stage", required=True)
    for stage in ("arm", "collect"):
        item = stages.add_parser(stage)
        item.add_argument("--lane", type=int, choices=(1, 2, 3, 4), required=True)
    item = stages.add_parser("audio")
    item.add_argument("--duration", type=int, choices=(60,), required=True)
    item = stages.add_parser("finalize")
    item.add_argument("--materialize-internal-poc-with-calibration-gaps", action="store_true")
    return parser.parse_args()


def stage_command(args, run):
    command = [sys.executable, "-X", "utf8", "-u", "-m", "gta_capture"]
    manifest = str(run / "config/dependency-manifest.json")
    tape = str(run / "config/camera-tape.json")
    replay = str(run / "source/master-replay.clip")
    if args.stage in ("arm", "collect"):
        command += ["native-export-smoke", "--lane", str(args.lane),
                    "--manifest", manifest, "--tape", tape, "--replay", replay,
                    "--run-dir", str(run)]
        if args.stage == "arm":
            command.append("--arm-only")
    elif args.stage == "audio":
        command += ["native-audio-smoke", "--replay", replay,
                    "--run-dir", str(run), "--duration", str(args.duration)]
    else:
        command += ["finalize-native-poc", "--manifest", manifest,
                    "--scenario", str(run / "config/scenario.json"),
                    "--tape", tape, "--run-dir", str(run)]
        if args.materialize_internal_poc_with_calibration_gaps:
            command.append("--materialize-internal-poc-with-calibration-gaps")
    return command


def main():
    args = parse_args()
    if sys.version_info < (3, 10):
        raise RuntimeError("Use the bundled Python 3.10+ runtime")
    run = args.run_dir.resolve(strict=True)
    if not run.is_dir():
        raise RuntimeError("--run-dir must be an existing prepared run directory")
    logs = run.parent / "logs"
    logs.mkdir(exist_ok=True)
    lane_label = f"-lane{args.lane}" if args.stage in ("arm", "collect") else ""
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    log_path = logs / f"{run.name}-{args.stage}{lane_label}-{stamp}-{uuid.uuid4().hex[:8]}.log"
    env = os.environ.copy()
    env["PYTHONPATH"] = str(PACKAGE_ROOT) + (
        os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    env["PYTHONIOENCODING"] = "utf-8"
    env["PYTHONUNBUFFERED"] = "1"
    print(f"Log: {log_path}", flush=True)
    with log_path.open("x", encoding="utf-8", newline="\n") as log:
        def emit(value):
            log.write(value + "\n")
            log.flush()
            print(value, flush=True)

        def invoke(command):
            emit(json.dumps({"command": command, "cwd": str(PACKAGE_ROOT)}, ensure_ascii=False))
            with subprocess.Popen(command, cwd=PACKAGE_ROOT, env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True, encoding="utf-8", errors="replace") as process:
                for line in process.stdout:
                    log.write(line)
                    log.flush()
                    print(line, end="", flush=True)
                code = process.wait()
            emit(json.dumps({"exit_code": code}))
            return code

        try:
            emit(json.dumps({"run_dir": str(run), "stage": args.stage,
                             "started_at_utc": stamp, "automatic_ui": False,
                             "automatic_next_stage": False}, ensure_ascii=False))
            # Reuse the existing strict local validation unchanged. Finalize's
            # original CLI already performs its own strict input/preflight check.
            if args.stage != "finalize":
                validator = HERE / "validate-run-inputs.py"
                code = invoke([sys.executable, "-X", "utf8", "-u", str(validator), str(run)])
                if code:
                    emit("Strict validation failed; requested native stage was not invoked")
                    return code
            return invoke(stage_command(args, run))
        except Exception:
            emit(traceback.format_exc())
            return 1


if __name__ == "__main__":
    sys.exit(main())
