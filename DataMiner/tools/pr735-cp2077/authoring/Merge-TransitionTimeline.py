"""Read-only merge of one launch's native and controller QPC evidence. No frame edits."""
import argparse
import hashlib
import json
from pathlib import Path


def merge(native_path, controller_paths):
    inputs = []
    def read(path, limit):
        if path.stat().st_size > limit:
            raise ValueError(f"Evidence exceeds bound: {path.name}")
        data = path.read_bytes()
        if len(data) > limit:
            raise ValueError("Evidence grew past bound")
        inputs.append(dict(path=str(path.resolve()), bytes=len(data), sha256=hashlib.sha256(data).hexdigest()))
        return data.decode('utf-8-sig')
    native = json.loads(read(native_path, 8 * 1024 * 1024))
    if native['schema'] != 'dm-cp2077-authoring-timeline/v1':
        raise ValueError('Unsupported native trace')
    launch, frequency = native['process_launch_id'], native['qpc_frequency']
    if not isinstance(frequency, int) or frequency <= 0:
        raise ValueError('Invalid QPC frequency')
    events = []
    for sample in native['trace']['samples']:
        events.append(dict(qpc=sample['qpc'], event='native_source_sample', details=sample))
    for submitted in native['trace']['guide_submissions']:
        events.append(dict(qpc=submitted['qpc'], event='guide_gpu_submitted', details=submitted))
    for phase in native['phases']:
        for field, name in [('window_open_observed_qpc', 'native_window_open'), ('window_deadline_qpc', 'native_window_deadline')]:
            events.append(dict(qpc=phase[field], event=name, details=phase))
    for path in controller_paths:
        for line in read(path, 8 * 1024 * 1024).splitlines():
            event = json.loads(line)
            if event['schema'] != 'dm-cp2077-authoring-controller-event/v1' or event['process_launch_id'] != launch or event['qpc_frequency'] != frequency:
                raise ValueError('Controller launch/clock mismatch')
            events.append(event)
    if len(events) > 15000 or any(not isinstance(e['qpc'], int) or e['qpc'] <= 0 for e in events):
        raise ValueError('Invalid/bounded timeline')
    events.sort(key=lambda e: e['qpc'])
    origin = events[0]['qpc'] if events else 0
    for event in events:
        event['relative_seconds'] = (event['qpc'] - origin) / frequency
    # Refuse a changing input rather than combining two live revisions.
    for item in inputs:
        if hashlib.sha256(Path(item['path']).read_bytes()).hexdigest() != item['sha256']:
            raise ValueError('Input changed while merging')
    return dict(schema='dm-cp2077-authoring-merged-timeline/v1', process_launch_id=launch,
                qpc_frequency=frequency, origin_qpc=origin, non_trainable=True,
                in_game_menu_state_verified=False, operator_saw_guidance_verified=False,
                inputs=inputs, events=events)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--native', type=Path, required=True)
    parser.add_argument('--controller', type=Path, nargs='+', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = merge(args.native, args.controller)
    with args.output.open('x', encoding='utf-8') as handle:
        json.dump(result, handle, ensure_ascii=False, indent=2)
