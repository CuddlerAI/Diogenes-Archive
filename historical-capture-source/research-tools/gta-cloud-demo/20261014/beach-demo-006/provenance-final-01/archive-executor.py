"""Serial cloud archive execution with immutable receipts and ZIP CRC verification."""
import argparse
import datetime
import getpass
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import zipfile

TMP = Path(r'C:\workspace\mc4\tmp')
PY = Path(r'C:\Program Files\Python312\python.exe')
HELPER = TMP / 'issue965-final-local-evidence-staging-20261014-01' / 'local-addendum' / 'authorized-extra-01' / 'archive-selected-evidence-v2.py'
HELPER_SHA = '64a6cc7e6ad543f01a91b0687bb543f2733644db7ec18fe48b01315b2bfbc501'
BUNDLES = ['cloud-evidence', 'local-addendum', 'cloud-static-images', 'local-failure-images']

def sha(p):
 h=hashlib.sha256()
 with p.open('rb') as f:
  while b:=f.read(1024*1024):h.update(b)
 return h.hexdigest()

def utc():return datetime.datetime.now(datetime.timezone.utc).isoformat()

def main():
 p=argparse.ArgumentParser();p.add_argument('--manifest',required=True);p.add_argument('--manifest-sha256',required=True);a=p.parse_args()
 assert getpass.getuser().lower()=='gtaops' and platform.node()=='gta-cloud-1' and Path(sys.executable).resolve()==PY.resolve()
 src=Path(a.manifest);assert src.is_absolute() and src.stat().st_size<=2*1024*1024 and sha(src)==a.manifest_sha256
 plan=json.loads(src.read_text(encoding='utf-8-sig'));assert plan['state']=='READY_FOR_ARCHIVE' and not plan.get('pending')
 assert [b['id'] for b in plan['bundles']]==BUNDLES
 assert all(b['state']=='READY_FOR_ARCHIVE' and not b.get('pending') and b['execution_host']=='cloud' for b in plan['bundles'])
 assert sha(HELPER)==HELPER_SHA
 root=TMP/'issue965-final-provenance-archives-20261015-01';root.mkdir(exist_ok=False)
 selection=root/'selection-ready.json'
 with selection.open('xb') as f:f.write(src.read_bytes())
 assert sha(selection)==a.manifest_sha256
 with (root/'archive-executor.py').open('xb') as f:f.write(Path(__file__).read_bytes())
 receipt=dict(schema='issue965-final-archive-execution/1',started_at_utc=utc(),status='INCOMPLETE',execution_host=platform.node(),helper_sha256=HELPER_SHA,executor_sha256=sha(Path(__file__)),selection_sha256=a.manifest_sha256,bundles=[],final_demo_complete=False,delivery_approved=False)
 try:
  for name in BUNDLES:
   out=root/name
   argv=[str(PY),'-B',str(HELPER),'--manifest',str(selection),'--bundle',name,'--execution-host','cloud','--out',str(out)]
   row=dict(bundle=name,argv=argv,started_at_utc=utc())
   receipt['bundles'].append(row)
   with (root/(name+'.stdout.log')).open('xb') as stdout,(root/(name+'.stderr.log')).open('xb') as stderr:
    row['exit_code']=subprocess.run(argv,stdin=subprocess.DEVNULL,stdout=stdout,stderr=stderr,shell=False).returncode
   row['finished_at_utc']=utc()
   assert row['exit_code']==0,'Archive failed: '+name
   rp=out/'PACKAGING-RECEIPT.json';r=json.loads(rp.read_text(encoding='utf-8'))
   assert r['status']=='ARCHIVE_CREATED_NOT_UPLOADED_NOT_ACCEPTANCE' and r['sources_stable'] is True
   assert r['helper_sha256']==HELPER_SHA and r['selection_sha256']==a.manifest_sha256
   zpath=Path(r['archive']['path']);assert zpath==out/'evidence.zip' and zpath.stat().st_size==r['archive']['bytes'] and sha(zpath)==r['archive']['sha256']
   with zipfile.ZipFile(zpath,'r') as z:
    assert len(z.infolist())==r['archive_member_count'] and z.testzip() is None
   row.update(packaging_receipt=r,packaging_receipt_sha256=sha(rp),zip_crc_check='PASS')
  assert sha(src)==a.manifest_sha256 and sha(HELPER)==HELPER_SHA
  receipt['status']='FOUR_BUNDLES_CREATED_VERIFIED_NOT_ACCEPTANCE'
 except Exception as e:
  receipt['error']=type(e).__name__+': '+str(e)
 finally:
  receipt['finished_at_utc']=utc()
  with (root/'ARCHIVE-RUN-RECEIPT.json').open('x',encoding='utf-8') as f:json.dump(receipt,f,ensure_ascii=False,indent=2);f.write('\n')
 print(json.dumps(dict(path=str(root),status=receipt['status'],error=receipt.get('error'),bundles=len(receipt['bundles']))))
 return 0 if receipt['status']=='FOUR_BUNDLES_CREATED_VERIFIED_NOT_ACCEPTANCE' else 1

if __name__=='__main__':raise SystemExit(main())
