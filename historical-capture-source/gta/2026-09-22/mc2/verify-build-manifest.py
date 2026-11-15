import argparse
import json
from pathlib import Path
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt
from gta_capture.jsonio import sha256_file

p=argparse.ArgumentParser()
p.add_argument('--manifest',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
a=p.parse_args()
if a.out.exists(): raise RuntimeError('preserve earlier verification reports')
m=DependencyManifest.load(a.manifest)
result=_check_capture_build_receipt(m).to_dict()
result['manifest_sha256']=sha256_file(a.manifest)
with a.out.open('x',encoding='utf-8') as f: json.dump(result,f,indent=2)
print(json.dumps(result))
raise SystemExit(0 if result['status']=='PASS' else 1)
