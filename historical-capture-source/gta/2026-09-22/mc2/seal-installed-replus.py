import json
from pathlib import Path
from gta_capture.jsonio import sha256_file, utc_now
from gta_capture.preflight import DependencyManifest, _check_capture_build_receipt

root=Path(__file__).resolve().parent/'replus-story-idle-derived-01'
manifest=json.loads((root/'manifest-source-review.json').read_text(encoding='utf-8'))
component=Path('C:/Program Files (x86)/Steam/steamapps/common/Grand Theft Auto V/RockstarEditorPlus.asi')
assert sha256_file(component)==manifest['components']['replus_asi']['sha256']
manifest['components']['replus_asi']['path']=str(component)
target=root/'manifest-installed.json'
with target.open('x',encoding='utf-8') as f: json.dump(manifest,f,indent=2)
check=_check_capture_build_receipt(DependencyManifest.load(target)).to_dict()
report={'installed_at':utc_now(),'manifest':str(target),'manifest_sha256':sha256_file(target),
        'component':str(component),'component_sha256':sha256_file(component),
        'source_verification':check,'story_projection_runtime_tested':False,
        'editor_equivalence_verified':False,'delivery_eligible':False,
        'prior_component_backup':str(root/'parent/RockstarEditorPlus.asi')}
with (root/'deployment-receipt.json').open('x',encoding='utf-8') as f: json.dump(report,f,indent=2)
print(json.dumps(report))
assert check['status']=='PASS'
