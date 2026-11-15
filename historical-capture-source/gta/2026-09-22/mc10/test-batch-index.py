"""Synthetic contract fixtures only. No game, decoder, real scene or quality approval.

Tiny files deliberately are NOT media; existing-review assertions are simulated
solely to test index selection and seal handling. All fixtures and generated pages
live in labelled temporary test directories and are removed after each test.
"""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode=True
BASE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('mc10_batch_index',BASE/'build-batch-index.py')
index=importlib.util.module_from_spec(spec);spec.loader.exec_module(index)


def write(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value,ensure_ascii=False),encoding='utf8')
    return index.record(path)


def fixture(area,number):
    name=f'SYNTHETIC-TEST-{number:02d}'
    root=area/name/'run';review_dir=area/name/'review';preview_dir=area/name/'preview'
    for p in (root,review_dir,preview_dir):p.mkdir(parents=True)
    config={key:write(root/f'config/{key}.json',{'synthetic_test_only':True})
            for key in ('dependency_manifest','scenario','camera_tape','scenario_plan')}
    video=[];metadata=[];events={};probes={};contacts=[]
    for lane in range(1,5):
        p=root/f'capture/video_{lane}.mp4';p.parent.mkdir(exist_ok=True)
        p.write_bytes(f'SYNTHETIC NON-MEDIA TEST {number} LANE {lane}'.encode());video.append(index.record(p))
        for stem in ('action_camera','systeminfo'):
            metadata.append(write(root/f'capture/{stem}_{lane}.json',{'synthetic_test_only':True}))
        events[str(lane)]=write(root/f'evidence/frame-events-lane{lane}.jsonl',{'synthetic_test_only':True})
        probes[str(lane)]=write(review_dir/f'video-{lane}-ffprobe.json',{'synthetic_test_only':True})
        p=review_dir/f'camera-{lane}-contact.jpg';p.write_bytes(b'SYNTHETIC NON-IMAGE');contacts.append(index.record(p))
    master=hashlib.sha256(name.encode()).hexdigest()
    qc={'verdict':'FAIL','counts':{'PASS':148,'FAIL':1,'SKIP':14},'results':[
        {'rule':'num.input_stream','status':'FAIL','target':'clip','evidence':'SYNTHETIC retained failure'},
        {'rule':'calibration','status':'SKIP','target':'clip','evidence':'SYNTHETIC missing evidence'}]}
    gate={'ok':False,'passed_rules':['SYNTHETIC_existing_rule'],'missing_rules':['calibration','rights']}
    qc_record=write(root/'evidence/engine-qc.json',qc);gate_record=write(root/'evidence/gta-pr405-gate.json',gate)
    manifest={'schema':'dm-gta-run-manifest/1','completed_at':'synthetic-test-only','dry_run':False,
        'mode':'real-staged-operator-gated','delivery_eligible':False,'run_id':name,
        'master':{'replay_sha256':master,'mock':False,'recording_start_verified':True,'player_observation_count':1800},
        'inputs':config,'videos':video,'metadata_files':metadata,'delivery_files':video+metadata,
        'alignment':{'lane_event_sha256':{k:v['sha256'] for k,v in events.items()}},
        'preflight':{'retained_failures':['calibration.binding']}}
    manifest_record=write(root/'run-manifest.json',manifest)
    review={'schema':'dm-gta-mc10-scene-review/1','run_dir':str(root),'run_id':name,
        'integrity_status':'PASS','errors':[],'inputs':[manifest_record,qc_record,gate_record,*video,*metadata,*events.values(),*config.values()],
        'raw_pngs':{'count':7200,'all_hashes_checked':True},'contact_sheets':contacts,
        'selected_frames':[{'synthetic_test_only':True} for _ in range(64)],
        'checks':{k:'PASS' for k in ('manifest_master_and_sources','raw_fence_time_master','metadata_roundtrip',
            'sealed_reports','actual_video_decodes','64_actual_contact_frames','inputs_unchanged')},
        'videos':{str(lane):{'source':video[lane-1],'decoded_frames':1800,'width':1920,'height':1080,
            'fps':'30/1','video_duration_s':60,'probe':probes[str(lane)]} for lane in range(1,5)},
        'retained_qc':{'verdict':qc['verdict'],'counts':qc['counts']},'visual_review_status':'NOT_REVIEWED'}
    write(review_dir/'scene-review.json',review)
    p=preview_dir/'four-camera-preview.mp4';p.write_bytes(b'SYNTHETIC NON-MEDIA PREVIEW')
    outputs=[index.record(p),write(preview_dir/'preview-ffprobe.json',{'synthetic_test_only':True})]
    preview={'schema':'gta-derived-review/2','status':'completed','run_dir':str(root),'media_generated':True,
        'frame_count':1800,'fps':'30/1','replay_sha256':master,'dependency_manifest':config['dependency_manifest'],
        'preview_probe_verified':{'frame_count':1800,'width':1920,'height':1080,'fps':'30/1','duration_s':60},
        'source_videos':{str(i+1):{'file':v} for i,v in enumerate(video)},'event_files':events,'outputs':outputs}
    write(preview_dir/'review-media-manifest.json',preview)
    return {'scene_id':str(number),'scene_name':name,'distinct_scene_basis':'SYNTHETIC TEST ONLY; no actual scene',
        'selection':'selected_final','run_dir':str(root),'review_report':str(review_dir/'scene-review.json'),
        'preview_manifest':str(preview_dir/'review-media-manifest.json')}


class IndexTests(unittest.TestCase):
    def setUp(self):
        area=BASE/'_batch-index-tests';area.mkdir(exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(prefix='SYNTHETIC-TEST-',dir=area)
        self.area=Path(self.temp.name).resolve()
        (self.area/'README-SYNTHETIC.txt').write_text(__doc__,encoding='utf8')
        self.entry=fixture(self.area,1);self.out=self.area/'index'
    def tearDown(self):
        # Bound recursive cleanup to this particular generated fixture directory.
        assert self.area.is_relative_to((BASE/'_batch-index-tests').resolve())
        self.temp.cleanup()
    def update(self,key,change):
        path=Path(self.entry[key]);value=json.loads(path.read_text(encoding='utf8'));change(value);write(path,value)
    def selected(self,scenes):
        p=self.area/'selected-SYNTHETIC.json';write(p,{'schema':'dm-gta-mc10-selected-runs/1','scenes':scenes});return p
    def test_existing_quality_failure_and_skip_are_retained_without_approval(self):
        result=index.validate_entry(self.entry,self.out)
        self.assertEqual(result['qc']['verdict'],'FAIL')
        self.assertEqual([v['status'] for v in result['qc']['non_pass_results']],['FAIL','SKIP'])
        self.assertEqual(result['gate']['missing_rules'],['calibration','rights'])
        for key in ('human_review_approval','formal_quality_approval','delivery_eligible'):self.assertIs(result[key],False)
        self.assertFalse(self.out.exists())
    def test_exactly_ten_required_before_output_exists(self):
        for n in (0,1,9,11):
            with self.subTest(count=n),self.assertRaisesRegex(ValueError,'exactly ten'):
                index.build_index(self.selected([self.entry]*n),self.out)
        self.assertFalse(self.out.exists())
    def test_failed_or_candidate_selection_rejected(self):
        for selection in ('candidate','failed','',None):
            value=copy.deepcopy(self.entry);value['selection']=selection
            with self.subTest(selection=selection),self.assertRaisesRegex(ValueError,'candidate/failed'):
                index.validate_entry(value,self.out)
    def test_unfinalized_run_is_rejected(self):
        (Path(self.entry['run_dir'])/'run-manifest.json').unlink()
        with self.assertRaises(OSError):index.validate_entry(self.entry,self.out)
        self.assertFalse(self.out.exists())
    def test_dry_run_rejected(self):
        p=Path(self.entry['run_dir'])/'run-manifest.json';v=json.loads(p.read_text(encoding='utf8'));v['dry_run']=True;write(p,v)
        with self.assertRaisesRegex(ValueError,'not a finalized real'):index.validate_entry(self.entry,self.out)
    def test_mock_master_rejected(self):
        p=Path(self.entry['run_dir'])/'run-manifest.json';v=json.loads(p.read_text(encoding='utf8'));v['master']['mock']=True;write(p,v)
        with self.assertRaisesRegex(ValueError,'real complete recording'):index.validate_entry(self.entry,self.out)
    def test_review_fail_or_blocked_rejected(self):
        for status in ('FAIL','BLOCKED','RUNNING'):
            self.update('review_report',lambda x:x.update(integrity_status=status))
            with self.subTest(status=status),self.assertRaisesRegex(ValueError,'successful actual scene review'):
                index.validate_entry(self.entry,self.out)
    def test_missing_decode_check_rejected_despite_pass_label(self):
        self.update('review_report',lambda x:x['checks'].pop('actual_video_decodes'))
        with self.assertRaisesRegex(ValueError,'full scene review checks'):index.validate_entry(self.entry,self.out)
    def test_preview_validate_only_rejected(self):
        self.update('preview_manifest',lambda x:x.update(status='inputs_validated_only',media_generated=False))
        with self.assertRaisesRegex(ValueError,'preview completion receipt'):index.validate_entry(self.entry,self.out)
    def test_preview_missing_actual_file_rejected(self):
        (Path(self.entry['preview_manifest']).parent/'four-camera-preview.mp4').unlink()
        with self.assertRaises(OSError):index.validate_entry(self.entry,self.out)
    def test_missing_one_lane_event_seal_rejected(self):
        self.update('preview_manifest',lambda x:x['event_files'].pop('4'))
        with self.assertRaisesRegex(ValueError,'all four event files'):index.validate_entry(self.entry,self.out)
    def test_missing_preview_decoding_receipt_rejected(self):
        self.update('preview_manifest',lambda x:x.update(outputs=x['outputs'][:1]))
        with self.assertRaisesRegex(ValueError,'actual decoded probe report'):index.validate_entry(self.entry,self.out)
    def test_tampered_sources_or_decode_reports_rejected(self):
        paths=[Path(self.entry['run_dir'])/'capture/video_1.mp4',Path(self.entry['run_dir'])/'capture/action_camera_1.json',
               Path(self.entry['run_dir'])/'evidence/frame-events-lane1.jsonl',
               Path(self.entry['review_report']).parent/'video-1-ffprobe.json',
               Path(self.entry['preview_manifest']).parent/'preview-ffprobe.json']
        for path in paths:
            original=path.read_bytes();path.write_bytes(original+b' TAMPERED')
            try:
                with self.subTest(path=path.name),self.assertRaisesRegex(ValueError,'differs from its prior seal'):
                    index.validate_entry(self.entry,self.out)
            finally:path.write_bytes(original)
    def test_changed_formal_qc_is_not_replaced_by_old_pass_review(self):
        path=Path(self.entry['run_dir'])/'evidence/engine-qc.json';value=json.loads(path.read_text(encoding='utf8'));value['verdict']='PASS';write(path,value)
        with self.assertRaisesRegex(ValueError,'QC changed after review'):index.validate_entry(self.entry,self.out)
    def test_output_must_be_new_inside_batch(self):
        selected=self.selected([self.entry]*10)
        existing=self.area/'existing';existing.mkdir();marker=existing/'keep';marker.write_bytes(b'untouched')
        for path in (existing,BASE,BASE.parent/'outside-index',self.area/'..'/'..'/'..'/'escaped'):
            with self.subTest(path=path),self.assertRaisesRegex(ValueError,'new directory inside'):
                index.build_index(selected,path)
        self.assertEqual(marker.read_bytes(),b'untouched')
    def test_output_must_not_overlap_sources(self):
        for path in (Path(self.entry['run_dir'])/'new-index',Path(self.entry['review_report']).parent/'new-index',
                     Path(self.entry['preview_manifest']).parent/'new-index'):
            with self.subTest(path=path),self.assertRaisesRegex(ValueError,'overlaps'):
                index.validate_entry(self.entry,path)
            self.assertFalse(path.exists())
    def test_repeated_attempt_cannot_count_as_ten_scenes(self):
        with self.assertRaisesRegex(ValueError,'duplicate scene_id'):
            index.build_index(self.selected([self.entry]*10),self.out)
        self.assertFalse(self.out.exists())
    def test_reused_master_rejected_even_with_ten_distinct_scene_ids(self):
        # Isolate the batch uniqueness check after entry validation; no media assertions are tested here.
        values=[dict(index.validate_entry(self.entry,self.out),scene_id=str(i),run_id=str(i),run_dir=str(self.area/f'fake-{i}')) for i in range(10)]
        with patch.object(index,'validate_entry',side_effect=values),self.assertRaisesRegex(ValueError,'duplicate master_replay_sha256'):
            index.build_index(self.selected([self.entry]*10),self.out)
        self.assertFalse(self.out.exists())
    def test_ten_synthetic_contract_entries_generate_only_local_unapproved_index(self):
        # Contract-only happy path, not ten real videos. All generated pages are removed in tearDown.
        entries=[self.entry]+[fixture(self.area,i) for i in range(2,11)]
        entries[0]['scene_name']='<script>alert(1)</script> SYNTHETIC'
        value=index.build_index(self.selected(entries),self.out)
        registry=json.loads((self.out/'registry.json').read_text(encoding='utf8'))
        page=(self.out/'index.html').read_text(encoding='utf8')
        self.assertEqual(value['scene_count'],10);self.assertEqual(registry['original_video_count'],40)
        self.assertNotIn('<script>',page);self.assertIn('&lt;script&gt;',page)
        self.assertEqual(page.count('<video controls preload="none"'),10)
        self.assertEqual(len(list(self.out.iterdir())),3)
        self.assertFalse(registry['human_review_approval']);self.assertFalse(registry['delivery_eligible'])
        self.assertTrue(all(s['qc']['verdict']=='FAIL' for s in registry['scenes']))
        saved=(self.out/'registry.json').read_bytes()
        with self.assertRaisesRegex(ValueError,'no overwrite'):index.build_index(self.selected(entries),self.out)
        self.assertEqual((self.out/'registry.json').read_bytes(),saved)


if __name__=='__main__':unittest.main(verbosity=2)
