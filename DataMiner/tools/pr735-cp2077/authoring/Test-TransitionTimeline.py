import importlib.util
import json
import tempfile
from pathlib import Path

spec=importlib.util.spec_from_file_location('timeline',Path(__file__).with_name('Merge-TransitionTimeline.py'))
module=importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with tempfile.TemporaryDirectory(prefix='cp2077-timeline-test-') as root:
    root=Path(root);native=root/'native.json';events=root/'controller.jsonl'
    native.write_text(json.dumps(dict(schema='dm-cp2077-authoring-timeline/v1',process_launch_id='a'*32,
        qpc_frequency=1000,trace=dict(samples=[dict(qpc=300,active_proven_count=0),dict(qpc=600,active_proven_count=1)],
        guide_submissions=[dict(qpc=500,guide=dict(Record=2))]),phases=[dict(window_open_observed_qpc=490,window_deadline_qpc=4490)])))
    event=dict(schema='dm-cp2077-authoring-controller-event/v1',process_launch_id='a'*32,qpc_frequency=1000,
               qpc=100,event='settings_observed',details=dict(observed_fov=90))
    events.write_text(json.dumps(event)+'\n')
    original=[p.read_bytes() for p in (native,events)]
    result=module.merge(native,[events])
    assert [e['qpc'] for e in result['events']]==[100,300,490,500,600,4490]
    assert result['events'][-1]['relative_seconds']==4.39
    assert not result['in_game_menu_state_verified'] and not result['operator_saw_guidance_verified']
    assert original==[p.read_bytes() for p in (native,events)]
    for field,value in [('process_launch_id','b'*32),('qpc_frequency',1001)]:
        invalid=dict(event);invalid[field]=value;events.write_text(json.dumps(invalid)+'\n')
        try:module.merge(native,[events])
        except ValueError:pass
        else:raise AssertionError('mismatched launch or clock accepted')
print('timeline order, original QPC/SHA, immutability, wrong launch/clock: PASS (synthetic)')
