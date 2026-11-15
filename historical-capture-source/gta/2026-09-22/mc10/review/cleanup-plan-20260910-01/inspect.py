"""Task-scoped read-only metadata inventory; does not read or hash RAW pixels."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
import os, json, hashlib, datetime, collections, importlib.util

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2]
assert OUT.name == 'out'
STAMPS = [OUT / 'mc2-20260908', OUT / 'mc10-20260908']
RAW = Path('C:/capture/gta/replus-output')
RUNS = Path('C:/capture/gta/runs')
ERRORS = []
INTER = {'.obj', '.pch', '.idb', '.ilk', '.tlog'}
refs = collections.defaultdict(set)
events = collections.defaultdict(list)
builds, scratch, manifests = [], [], []
files_scanned = 0

def key(p):
    return os.path.normcase(os.path.abspath(p))

def record(p):
    data = p.read_bytes()
    return {'path': str(p), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}

def paths_under(root):
    for base, dirs, files in os.walk(root, followlinks=False, onerror=lambda e: ERRORS.append(str(e))):
        safe = []
        for d in dirs:
            p = Path(base)/d
            try:
                if not p.is_symlink() and not (getattr(p.stat(follow_symlinks=False),'st_file_attributes',0) & 0x400):
                    safe.append(d)
            except OSError as e:
                ERRORS.append(str(e))
        dirs[:] = safe
        yield Path(base), dirs, files

def stats(root):
    global files_scanned
    total = png = count = png_count = linked = 0
    ext = collections.defaultdict(lambda: {'files':0,'bytes':0})
    intermediates=[]
    for base, dirs, files in paths_under(root):
        for name in files:
            p=base/name
            try:
                st=p.stat(follow_symlinks=False)
                if getattr(st,'st_file_attributes',0)&0x400:
                    continue
                size=st.st_size; suffix=p.suffix.lower()
                total+=size; count+=1; files_scanned+=1
                ext[suffix]['files']+=1; ext[suffix]['bytes']+=size
                if suffix=='.png': png+=size; png_count+=1
                if st.st_nlink>1: linked+=size
                if suffix in INTER:
                    intermediates.append({'path':str(p),'bytes':size,'nlink':st.st_nlink})
            except OSError as e: ERRORS.append(str(e))
    return {'path':str(root),'files':count,'logical_bytes':total,'png_files':png_count,'png_bytes':png,
            'multiply_linked_logical_bytes':linked,'by_extension':dict(sorted(ext.items())),
            '_intermediates':intermediates}

def reference_walk(v, doc):
    if isinstance(v,dict):
        p=v.get('path')
        if isinstance(p,str) and ('sha256' in v or 'bytes' in v): refs[key(p)].add(str(doc))
        for sub in v.values(): reference_walk(sub,doc)
    elif isinstance(v,list):
        for sub in v: reference_walk(sub,doc)

def map_event(p):
    byroot=collections.defaultdict(lambda:{'rows':0,'declared_bytes':0,'mock_rows':0,'first_frame':None,'last_frame':None})
    digest=hashlib.sha256(); nbytes=0; parse_errors=0
    with p.open('rb') as f:
        for line in f:
            digest.update(line); nbytes+=len(line)
            if not line.strip(): continue
            try: row=json.loads(line)
            except ValueError: parse_errors+=1; continue
            output=row.get('output_path')
            if not isinstance(output,str): continue
            q=Path(output).parent
            if q.parent != RAW: continue
            b=byroot[key(q)]
            b['rows']+=1; b['declared_bytes']+=row.get('output_bytes',0)
            b['mock_rows']+=int(row.get('mock') is not False)
            b['first_frame']=row.get('output_frame') if b['first_frame'] is None else b['first_frame']
            b['last_frame']=row.get('output_frame')
    for root, counts in byroot.items():
        events[root].append({'event_file':{'path':str(p),'bytes':nbytes,'sha256':digest.hexdigest()},
                            'partial':p.name.endswith('.partial'),'parse_errors':parse_errors,**counts})

for root in STAMPS+[RUNS]:
    for base, dirs, files in paths_under(root):
        if base.name=='media-scratch':
            item=stats(base); item.pop('_intermediates'); scratch.append(item); dirs[:]=[]; continue
        if 'CMakeCache.txt' in files:
            item=stats(base); builds.append(item); dirs[:]=[]; continue
        dirs[:]=[d for d in dirs if d not in {'source','sdk','.git','capture','analysis-runtime','__pycache__','_batch-index-tests','_review-tool-tests'}]
        for name in files:
            p=base/name
            if name.startswith('frame-events-lane') and (name.endswith('.jsonl') or name.endswith('.jsonl.partial')):
                try: map_event(p)
                except OSError as e: ERRORS.append(str(e))
            elif name.endswith('.json') and any(t in name for t in ('receipt','manifest','inventory')):
                try:
                    if p.stat().st_size<=8_000_000:
                        v=json.loads(p.read_text(encoding='utf-8-sig')); reference_walk(v,p)
                        if name=='run-manifest.json':
                            manifests.append({'file':record(p),'run_id':v.get('run_id'),
                              'mode':v.get('mode'),'dry_run':v.get('dry_run'),
                              'delivery_eligible':v.get('delivery_eligible'),
                              'capture_dir':v.get('capture_dir')})
                except (OSError,ValueError) as e: ERRORS.append(str(e))

raw=[]
for directory in sorted(RAW.iterdir()):
    if directory.is_dir() and directory.name.startswith('render_'):
        item=stats(directory); item.pop('_intermediates')
        item['event_references']=events.get(key(directory),[])
        raw.append(item)

for b in builds:
    for f in b['_intermediates']: f['direct_receipt_references']=sorted(refs.get(key(f['path']),[]))
    b['intermediate_files']=b.pop('_intermediates')
    b['unreferenced_intermediate_bytes']=sum(f['bytes'] for f in b['intermediate_files'] if not f['direct_receipt_references'])

local=[]
for root in STAMPS+[RUNS]:
    for child in sorted(root.iterdir()):
        if child.is_dir():
            row=stats(child); row.pop('_intermediates'); local.append(row)

spec=importlib.util.spec_from_file_location('prior_storage',STAMPS[1]/'review/storage-audit-01/inspect-storage.py')
module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
volumes,volume_errors=module.volumes()
doc={'schema':'dm-gta-cleanup-inventory/1','created_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
 'scope':[str(p) for p in STAMPS+[RUNS,RAW]],
 'method':'filesystem names/stat sizes plus frame-event JSONL and small receipts; no RAW PNG content read/hash',
 'bytes_semantics':'logical file sizes, not measured allocated disk space; overlapping tables are not additive',
 'volumes':volumes,'volume_errors':volume_errors,'raw_directories':raw,'media_scratch':scratch,
 'build_directories':builds,'local_top_level_directories':local,'run_manifests':manifests,
 'totals':{'raw_logical_bytes':sum(x['logical_bytes'] for x in raw),'raw_png_files':sum(x['png_files'] for x in raw),
 'scratch_logical_bytes':sum(x['logical_bytes'] for x in scratch),
 'unreferenced_build_intermediate_bytes':sum(x['unreferenced_intermediate_bytes'] for x in builds),
 'stat_calls_including_overlap':files_scanned},'errors':ERRORS,'deletion_performed':False,
 'remote_archive_verification_performed':False,'formal_approval_granted':False}
destination=HERE/'inventory.json'
with destination.open('x',encoding='utf-8') as f: json.dump(doc,f,ensure_ascii=False,indent=2); f.write('\n')
print(json.dumps({'report':str(destination),'totals':doc['totals'],'errors':len(ERRORS),
 'volumes':[{'mount_paths':v['mount_paths'],'available_bytes':v['available_bytes']} for v in volumes],
 'scratch':scratch,'top_raw':[{'path':x['path'],'bytes':x['logical_bytes'],'png':x['png_files'],'event_sources':[e['event_file']['path'] for e in x['event_references']]} for x in sorted(raw,key=lambda x:x['logical_bytes'],reverse=True)[:8]],
 'top_local':[{'path':x['path'],'bytes':x['logical_bytes']} for x in sorted(local,key=lambda x:x['logical_bytes'],reverse=True)[:8]]},ensure_ascii=False,indent=2))
