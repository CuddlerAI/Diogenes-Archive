"""Read-only replay of v2 CFR decisions and raw-depth window collisions.

The priority result is an offline upper bound with known depth rows, not a live
policy or a new capture. A slot still emits one source, with its original QPC.
"""
import argparse
from collections import Counter, deque
import hashlib
import json
from pathlib import Path


def maximum_distinct_selection(windows):
    """Bipartite matching: one original identity per window, counted once."""
    owner = {}
    for start in range(len(windows)):
        queue = deque([start]); seen = {start}; parent = {}
        available = None
        while queue and available is None:
            window = queue.popleft()
            for identity in sorted(windows[window]):
                if identity in parent:
                    continue
                parent[identity] = window
                if identity not in owner:
                    available = identity
                    break
                next_window = owner[identity]
                if next_window not in seen:
                    seen.add(next_window); queue.append(next_window)
        if available is not None:
            identity = available
            while True:
                window = parent[identity]
                prior = next((i for i,w in owner.items() if w==window), None)
                owner[identity] = window
                if prior is None:
                    break
                del owner[prior]
                identity = prior
    return len(owner)


def analyze(trace, depth):
    if not trace or trace[0].get('schema') != 'capture-cadence-trace/v2':
        raise ValueError('requires capture-cadence-trace/v2')
    if len(trace)>65538 or not trace[0].get('raw_watermark'):
        raise ValueError('requires bounded raw-watermark diagnostics')
    footer = trace[-1]
    if footer.get('kind') != 'footer' or not footer.get('complete') or footer.get('dropped_events') or footer.get('clock_errors'):
        raise ValueError('incomplete or lossy trace cannot establish collision counts')
    if not depth or len(depth) > 1024:
        raise ValueError('requires 1..1024 original depth rows')
    if any(b['present_seq']<=a['present_seq'] or b['present_qpc']<=a['present_qpc'] for a,b in zip(depth,depth[1:])):
        raise ValueError('original depth rows must have strictly increasing identities and QPC')
    by_low = {}
    for row in depth:
        low = row['present_seq'] & 65535
        if low in by_low:
            raise ValueError('depth low16 is not unique; no unwrapping is permitted')
        by_low[low] = row
    if max(r['present_seq'] for r in depth) - min(r['present_seq'] for r in depth) >= 65535:
        raise ValueError('depth span is ambiguous')
    freq = trace[0]['context']['qpc_frequency']
    if type(freq) is not int or freq<=0:
        raise ValueError('invalid original QPC frequency')
    intervals = sorted(b['present_qpc']-a['present_qpc'] for a,b in zip(depth,depth[1:]) if b['present_qpc']>a['present_qpc'])
    if not intervals:
        raise ValueError('need measured positive depth intervals')
    lead, lag = freq//16, max(freq//16, 3*intervals[len(intervals)//2])
    samples = {}
    for r in trace:
        if r['kind'] == 'raw_watermark':
            if r['source_seq'] in samples:
                raise ValueError('raw identity source repeated')
            samples[r['source_seq']] = r

    def depth_id(seq):
        r = samples.get(seq)
        if not r or r.get('status') != 'decoded_low16_only':
            return None
        d = by_low.get(r.get('raw_present_low16'))
        if d and -lead <= r['source_qpc']-d['present_qpc'] <= lag:
            return d['present_seq']
        return None

    selected = {}
    candidates = {}
    previous = None
    prior_frame = -1
    checked = 0
    collisions = []
    replacements = []
    retained_depth = set()
    depth_options = []
    sealed_sources = {}
    for r in trace:
        if r['kind'] == 'observe':
            slot = r['slot']; frame = slot['run_frame']; seq = r['source_seq']
            if seq not in samples or samples[seq]['source_qpc']!=r['source_qpc']:
                raise ValueError('observe lacks matching original ROI source/QPC evidence')
            future = r['source_qpc'] >= slot['end_qpc']
            if future != r['future']:
                raise ValueError('recorded future classification differs from source QPC')
            if future:
                if r.get('replaced_source_seq') is not None:
                    raise ValueError('future observation replaced a current source')
                continue
            old = selected.get(frame)
            if old != r.get('replaced_source_seq'):
                raise ValueError('recorded replacement differs from latest-candidate replay')
            selected[frame] = seq
            candidates.setdefault(frame, []).append(seq)
            if old is not None:
                old_id, new_id = depth_id(old), depth_id(seq)
                replacements.append({'run_frame':frame,'replaced_source':old,'replacement_source':seq,
                    'replaced_depth_present':old_id,'replacement_depth_present':new_id,
                    'both_original_source_qpcs_inside_slot':all(slot['start_qpc']<=samples[s]['source_qpc']<slot['end_qpc'] for s in [old,seq]),
                    'kind':'both_have_depth' if old_id is not None and new_id is not None else 'depth_replaced_by_no_depth' if old_id is not None else 'no_depth_replaced'})
        elif r['kind'] == 'seal':
            frame = r['slot']['run_frame']
            if frame != prior_frame+1:
                raise ValueError('noncontiguous sealed CFR windows')
            prior_frame = frame
            choice = selected.get(frame, previous)
            if choice != r['source_seq'] or r['duplicate'] != (frame not in selected):
                raise ValueError('sealed identity or duplicate differs from replay')
            if choice not in samples or samples[choice]['source_qpc']!=r['source_qpc']:
                raise ValueError('sealed source QPC differs from the original selected pixels')
            if r['superseded'] != max(0, len(candidates.get(frame, []))-1):
                raise ValueError('superseded count differs from replay')
            previous = choice; checked += 1
            sealed_sources[frame] = choice
            ident = depth_id(choice)
            if ident is not None:
                retained_depth.add(ident)
            options = {depth_id(s) for s in candidates.get(frame, [])} - {None}
            depth_options.append(options)
            if len(options)>1:
                collisions.append({'run_frame':frame,'depth_present_options':sorted(options),
                    'source_options':candidates[frame],'slot':r['slot'],'minimum_unretained':len(options)-1})
    outputs = [r for r in trace if r['kind']=='output_submitted']
    if len(outputs) != checked:
        raise ValueError('seals and actual encoding submissions differ')
    if len({r['run_frame'] for r in outputs}) != checked:
        raise ValueError('duplicate output submission')
    for r in outputs:
        if sealed_sources.get(r['run_frame']) != r['source_seq']:
            raise ValueError('selected source was not the source submitted')
    maximum = maximum_distinct_selection(depth_options)
    if maximum < len(retained_depth):
        raise ValueError('candidate evidence cannot explain already retained identities')
    return {'schema':'dm-cp2077-cfr-window-replay/v1','non_trainable':True,'native_acceptance_run':False,
        'sealed_windows_reproduced':checked,'recorded_matching_depth_identities':len(retained_depth),
        'known_depth_priority_counterfactual_upper_bound':maximum,
        'counterfactual_additional_matches_at_most':maximum-len(retained_depth),
        'replacement_counts':dict(Counter(r['kind'] for r in replacements)),
        'replacement_details':replacements,'collision_windows':collisions,
        'window_collision_excess_candidates':sum(r['minimum_unretained'] for r in collisions),
        'minimum_distinct_depth_identities_unretained':len(set().union(*depth_options))-maximum,
        'depth_rows':len(depth),'lead_ticks':lead,'lag_ticks':lag,
        'limits':['Fixed observed arrivals, slot membership and stopping boundary; not a resimulation of OS/GPU scheduling.',
                  'Priority is an offline oracle using immutable depth rows; no equivalent online signal is assumed.',
                  'One output per CFR window; no future source, timestamp change, frame synthesis or acceptance gate change.',
                  'Missing ROI identity is unknown. A replacement is not by itself a bug in the generic latest-source CFR policy.']}


def read_bounded(path, limit):
    if path.stat().st_size > limit:
        raise ValueError('input exceeds diagnostic bound')
    data = path.read_bytes()
    return data, [json.loads(line) for line in data.splitlines() if line.strip()]


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--trace',required=True,type=Path)
    parser.add_argument('--depth-index',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    a=parser.parse_args()
    output=a.output.resolve()
    if output.is_relative_to(a.depth_index.resolve().parent) or output in [a.trace.resolve(),a.depth_index.resolve()]:
        raise ValueError('output must be outside the raw session and must not overwrite an input')
    td,tr=read_bounded(a.trace,64*1024*1024);dd,dr=read_bounded(a.depth_index,4*1024*1024)
    result=analyze(tr,dr)
    result['inputs']={str(p.resolve()):hashlib.sha256(data).hexdigest() for p,data in [(a.trace,td),(a.depth_index,dd)]}
    if any(hashlib.sha256(Path(p).read_bytes()).hexdigest()!=sha for p,sha in result['inputs'].items()):
        raise ValueError('input changed during replay')
    with output.open('x',encoding='utf-8',newline='\n') as f:
        json.dump(result,f,ensure_ascii=False,indent=2);f.write('\n')


if __name__=='__main__':
    main()
