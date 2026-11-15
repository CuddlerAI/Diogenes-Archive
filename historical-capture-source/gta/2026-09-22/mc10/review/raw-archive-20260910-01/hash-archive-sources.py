"""Read-only SHA256 inventory of six explicit RAW roots; new local manifests only."""
import sys
sys.dont_write_bytecode=True
from pathlib import Path
import os, json, hashlib, datetime

HERE=Path(__file__).resolve().parent
BASE=HERE.parents[1]
RAW=Path('C:/capture/gta/replus-output')
TARGETS=[
 (58,'scene02-port/run-02',1),(59,'scene02-port/run-02',2),
 (60,'scene02-port/run-02-reexport-01',1),(61,'scene02-port/run-02-reexport-01',2),
 (52,'scene01-airport/run-02',1),(53,'scene01-airport/run-02',2)]
assert BASE.name=='mc10-20260908'

def stamp():return datetime.datetime.now(datetime.timezone.utc).isoformat()
def stat(p):
 s=p.stat(follow_symlinks=False)
 if getattr(s,'st_file_attributes',0)&0x400:raise ValueError(f'Reparse point forbidden: {p}')
 if not p.is_file():raise ValueError(f'Not regular file: {p}')
 return {'bytes':s.st_size,'mtime_ns':s.st_mtime_ns,'ctime_ns':s.st_ctime_ns,
         'device':s.st_dev,'file_id':s.st_ino,'nlink':s.st_nlink}
def snapshot(root,skip_visual=False):
 result={}
 for base,dirs,files in os.walk(root,followlinks=False):
  for d in dirs:
   p=Path(base)/d
   if getattr(p.stat(follow_symlinks=False),'st_file_attributes',0)&0x400:raise ValueError(f'Redirected directory: {p}')
  for name in files:
   p=Path(base)/name
   if skip_visual and p.suffix.lower() in {'.png','.jpg','.jpeg','.mp4'}:continue
   result[p.relative_to(root).as_posix()]=stat(p)
 return dict(sorted(result.items()))
def sha_file(p,initial=None):
 before=stat(p)
 if initial is not None and before!=initial:raise ValueError(f'Source changed before read: {p}')
 h=hashlib.sha256(); count=0
 with p.open('rb') as f:
  while b:=f.read(4*1024*1024):h.update(b);count+=len(b)
 after=stat(p)
 if before!=after or count!=before['bytes']:raise ValueError(f'Source changed while read: {p}')
 return {'original_path':str(p),'bytes':count,'sha256':h.hexdigest(),
         'stat_before':before,'stat_after':after}
def small_record(p):
 d=sha_file(p);return {'path':str(p),'bytes':d['bytes'],'sha256':d['sha256']}
def write_json(p,d):
 with p.open('x',encoding='utf-8',newline='\n') as f:json.dump(d,f,ensure_ascii=False,indent=2);f.write('\n')
def snap_sha(s):return hashlib.sha256(json.dumps(s,sort_keys=True,separators=(',',':')).encode()).hexdigest()

companions={}
for runrel in dict.fromkeys(t[1] for t in TARGETS):
 root=BASE/runrel; dest=HERE/'companions'/runrel.replace('/','--');dest.mkdir(parents=True,exist_ok=False)
 start=stamp();before=snapshot(root,skip_visual=True);rows=[]
 with (dest/'files.sha256.jsonl').open('x',encoding='utf-8',newline='\n') as f:
  for rel,s in before.items():
   row=sha_file(root/rel,s);row['relative_path']=rel
   f.write(json.dumps(row,ensure_ascii=False,separators=(',',':'))+'\n');rows.append(row)
 after=snapshot(root,skip_visual=True)
 if before!=after:raise ValueError(f'Companion source changed: {root}')
 summary={'schema':'dm-gta-archive-companion-inventory/1','status':'PASS','source_root':str(root),
   'started_at_utc':start,'completed_at_utc':stamp(),'files':len(rows),'bytes':sum(r['bytes'] for r in rows),
   'file_manifest':small_record(dest/'files.sha256.jsonl'),'source_stable':True,
   'snapshot_before_sha256':snap_sha(before),'snapshot_after_sha256':snap_sha(after),
   'scope':'All regular files under this run except derived PNG/JPG/MP4 visual review media; includes source clips/audio, configs, events, receipts and non-image review reports.',
   'not_a_complete_toolchain_archive':True,'formal_approval_granted':False}
 write_json(dest/'manifest.json',summary);companions[runrel]=small_record(dest/'manifest.json')
 print(json.dumps({'companion_complete':runrel,'files':len(rows),'bytes':summary['bytes']},ensure_ascii=False),flush=True)

completed=[]
for number,runrel,lane in TARGETS:
 root=RAW/f'render_{number:04d}';dest=HERE/root.name;dest.mkdir(exist_ok=False)
 start=stamp();event=BASE/runrel/'evidence'/f'frame-events-lane{lane}.jsonl'; event_record=small_record(event)
 expected={}
 with event.open('r',encoding='utf-8-sig') as f:
  for line in f:
   r=json.loads(line)
   p=Path(r['output_path'])
   if p.parent!=root:raise ValueError(f'Event references another render: {event}')
   if r.get('mock') is not False or r.get('render_succeeded') is not True:raise ValueError('Not successful native evidence')
   rel=p.relative_to(root).as_posix()
   if rel in expected:raise ValueError('Duplicate event output')
   expected[rel]={'sha256':r['output_sha256'],'bytes':r['output_bytes'],'output_frame':r['output_frame']}
 if len(expected)!=1800 or sorted(v['output_frame'] for v in expected.values())!=list(range(1800)):
  raise ValueError('Need all 1800 native events')
 before=snapshot(root);png_rel={rel for rel in before if rel.lower().endswith('.png')}
 if png_rel!=set(expected):raise ValueError('PNG directory differs from event set')
 count=total=png_count=png_bytes=0
 with (dest/'files.sha256.jsonl').open('x',encoding='utf-8',newline='\n') as f:
  for rel,s in before.items():
   row=sha_file(root/rel,s);row['relative_path']=rel
   if rel in expected:
    e=expected[rel]
    if row['sha256']!=e['sha256'] or row['bytes']!=e['bytes']:raise ValueError(f'PNG differs from native event: {root/rel}')
    row['native_event_expected']=e;row['native_event_match']=True
    png_count+=1;png_bytes+=row['bytes']
   f.write(json.dumps(row,ensure_ascii=False,separators=(',',':'))+'\n')
   count+=1;total+=row['bytes']
   if count%600==0:print(json.dumps({'progress':root.name,'files_hashed':count,'bytes_hashed':total}),flush=True)
 after=snapshot(root)
 if before!=after:raise ValueError(f'RAW source changed during full read: {root}')
 if small_record(event)!=event_record:raise ValueError('Events changed during RAW hash')
 summary={'schema':'dm-gta-raw-archive-inventory/1','status':'PASS','source_root':str(root),
   'started_at_utc':start,'completed_at_utc':stamp(),'files':count,'bytes':total,
   'png_files':png_count,'png_bytes':png_bytes,'native_event':event_record,
   'native_event_png_sha256_all_match':True,'source_stable':True,
   'snapshot_before_sha256':snap_sha(before),'snapshot_after_sha256':snap_sha(after),
   'file_manifest':small_record(dest/'files.sha256.jsonl'),'companion_manifest':companions[runrel],
   'archive_relative_root':f'capture/gta/replus-output/{root.name}',
   'restore_root':str(root),'remote_upload_performed':False,'remote_verified':False,
   'deletion_performed':False,'formal_approval_granted':False,
   'scope':'Exact bytes including PNG, audio and assemble files; failure/superseded attempt retains original status. Current local byte hashes do not imply remote verification.'}
 write_json(dest/'manifest.json',summary);completed.append(small_record(dest/'manifest.json'))
 print(json.dumps({'render_complete':root.name,'files':count,'bytes':total,'manifest':completed[-1]},ensure_ascii=False),flush=True)

write_json(HERE/'index.json',{'schema':'dm-gta-raw-archive-index/1','status':'PASS','created_at_utc':stamp(),
 'render_manifests':completed,'companion_manifests':companions,'raw_files':10812,'png_files':10800,
 'raw_bytes':sum(json.loads(Path(r['path']).read_text('utf-8'))['bytes'] for r in completed),
 'all_sources_stable_during_hash':True,'remote_verified':False,'deletion_performed':False,'formal_approval_granted':False,
 'restore_instruction':'Use each manifest source_root / relative_path. Keep companion run paths separate. Do not rewrite original sealed event paths or merge reused render roots.',
 'builder':small_record(Path(__file__))})
print(json.dumps({'all_complete':small_record(HERE/'index.json')},ensure_ascii=False),flush=True)
