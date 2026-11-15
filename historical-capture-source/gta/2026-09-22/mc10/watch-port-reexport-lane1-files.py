"""Bounded filesystem-only followup for one already armed export, no game control."""
import hashlib
import json
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
RUN = BASE / 'scene02-port/run-02-reexport-01'
DEADLINE = datetime(2026, 9, 9, 8, 45, 53, tzinfo=timezone.utc).timestamp()
SESSION = 'ebe0f3b1-56ec-48db-b414-42b1f9a44c4e'
REPLAY = 'e9328cf9061815d49c9b2be2419e87d1f39348c9c74499c24c762a6abdb8fdf8'
TAPE_SHA = '3e9c8d63c19a6a95b8c51748798169288868fd3f1966d7f2028155bc62cbcf87'
REVIEW = RUN/'review'
EARLY = REVIEW/'lane1-early-cursor-01.json'
FULL = REVIEW/'lane1-cursor-full-01.json'
CONTACT = REVIEW/'lane1-contact-01'
OUT = REVIEW/'lane1-filesystem-followup-01.json'
assert all(not p.exists() for p in [EARLY, FULL, CONTACT, OUT])
assert hashlib.sha256((RUN/'config/camera-tape.json').read_bytes()).hexdigest() == TAPE_SHA
REVIEW.mkdir(parents=True, exist_ok=True)
invocations = []
early_done = inspect_reported = False
last_progress = 0

def emit(**value):
    print(json.dumps(value, ensure_ascii=False), flush=True)

def invoke(args):
    remaining = DEADLINE - time.time()
    if remaining <= 0:
        raise TimeoutError('Twelve-minute filesystem followup limit reached')
    completed = subprocess.run([sys.executable, *map(str, args)], capture_output=True,
                               text=True, encoding='utf-8', timeout=remaining)
    invocations.append(dict(argv=[sys.executable, *map(str, args)], returncode=completed.returncode,
                            stdout=completed.stdout, stderr=completed.stderr))
    emit(tool_finished=args[0].name, returncode=completed.returncode, stdout=completed.stdout)
    if completed.returncode:
        raise RuntimeError(f'{args[0].name} returned {completed.returncode}')

status, error = 'TIME_LIMIT', None
try:
    while time.time() < DEADLINE:
        sealed = RUN/'evidence/frame-events-lane1.jsonl'
        path = sealed if sealed.exists() else RUN/'evidence/frame-events-lane1.jsonl.partial'
        data = path.read_bytes() if path.exists() else b''
        complete = data[:data.rfind(b'\n')+1]
        rows = [json.loads(x) for x in complete.splitlines() if x.strip()]
        assert [r['output_frame'] for r in rows] == list(range(len(rows)))
        assert all(r['schema'] == 'dm-gta-frame/1' and r['lane'] == 1 and r['mock'] is False and
                   r['render_succeeded'] is True and r['replay_sha256'] == REPLAY and
                   r['editor_binding']['session_id'] == SESSION for r in rows)
        if time.time() - last_progress >= 30:
            emit(phase='read_only_event_wait', complete_frames=len(rows), sealed=sealed.exists(),
                 observed_output_folder=str(Path(rows[0]['output_path']).parent) if rows else None)
            last_progress = time.time()
        if len(rows) >= 116 and not early_done:
            invoke([BASE/'known-cursor-scan-airport.py', path, '--frames', '0,114,115', '--out', EARLY])
            early_done = True
        if len(rows) >= 859 and not inspect_reported:
            emit(phase='independent_view_samples_available',
                 source_pngs=[rows[frame]['output_path'] for frame in [857,858]])
            inspect_reported = True
        if sealed.exists() and len(rows) == 1800:
            assert complete == data
            invoke([BASE/'known-cursor-scan-airport.py', sealed, '--all', '--out', FULL])
            invoke([BASE.parent/'mc2-20260908/build-review-media.py', '--run-dir', RUN,
                    '--out-dir', CONTACT, '--lane', '1'])
            status = 'COMPLETED_FIRST_LANE_READ_ONLY_REVIEW'
            break
        time.sleep(min(10, max(0, DEADLINE-time.time())))
except Exception as exc:
    status, error = 'ERROR', f'{type(exc).__name__}: {exc}'

report = dict(schema='dm-gta-filesystem-export-followup/1', run_dir=str(RUN), lane=1,
              status=status, error=error, completed_at=datetime.now(timezone.utc).isoformat(),
              fixed_deadline_utc='2026-09-09T08:45:53Z', camera_tape_sha256=TAPE_SHA,
              editor_session_id=SESSION, replay_sha256=REPLAY, invocations=invocations,
              game_ui_api_or_pipe_called=False, early_scan_completed=early_done,
              independent_view_samples_available=inspect_reported,
              images_viewed_by_this_script=False, formal_quality_gate=False,
              human_review_approval=False, delivery_eligible=False, non_trainable=True)
with OUT.open('x', encoding='utf-8', newline='\n') as f:
    json.dump(report, f, ensure_ascii=False, indent=2); f.write('\n')
emit(report=str(OUT), status=status, error=error)
raise SystemExit(0 if status == 'COMPLETED_FIRST_LANE_READ_ONLY_REVIEW' else 1)
