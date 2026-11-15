"""Bounded path/probe contract tests only; no real capture review is run."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode=True
BASE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('mc10_scene_review',BASE/'review-scene.py')
review=importlib.util.module_from_spec(spec);spec.loader.exec_module(review)


class ReviewPathTests(unittest.TestCase):
    def setUp(self):
        area=BASE/'_review-tool-tests';area.mkdir(exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(prefix='case-',dir=area)
        self.area=Path(self.temp.name).resolve();self.run=self.area/'run';self.run.mkdir()
        (self.run/'run-manifest.json').write_text('{}',encoding='utf8')
    def tearDown(self):
        # Explicitly bound the recursive cleanup to this test's generated area.
        assert Path(self.temp.name).resolve().is_relative_to(BASE/'_review-tool-tests')
        self.temp.cleanup()
    def paths(self,out):return review.safe_paths(self.run,out,base=self.area)
    def test_new_sibling_is_allowed_without_creating_it(self):
        target=self.area/'review-01';root,out=self.paths(target)
        self.assertEqual(root,self.run);self.assertEqual(out,target);self.assertFalse(target.exists())
    def test_run_itself_is_rejected(self):
        with self.assertRaises(ValueError):self.paths(self.run)
    def test_run_subdirectories_cannot_receive_outputs(self):
        for name in ('capture','config','evidence','source','preview'):
            with self.subTest(name=name),self.assertRaises(ValueError):self.paths(self.run/name/'review')
            self.assertFalse((self.run/name).exists())
    def test_existing_empty_output_rejected(self):
        target=self.area/'empty';target.mkdir()
        with self.assertRaises(ValueError):self.paths(target)
    def test_existing_file_not_overwritten(self):
        target=self.area/'important';target.write_bytes(b'unchanged')
        with self.assertRaises(ValueError):self.paths(target)
        self.assertEqual(target.read_bytes(),b'unchanged')
    def test_dotdot_escape_rejected(self):
        target=self.area/'new'/'..'/'..'/'escape-review'
        with self.assertRaises(ValueError):self.paths(target)
        self.assertFalse(target.resolve().exists())
    def test_batch_root_rejected(self):
        with self.assertRaises(ValueError):self.paths(self.area)
    def test_outside_batch_rejected(self):
        with self.assertRaises(ValueError):self.paths(self.area.parent/'outside-review')
    def test_unfinalized_run_rejected_before_output_creation(self):
        (self.run/'run-manifest.json').unlink()
        with self.assertRaises(ValueError):self.paths(self.area/'review')
        self.assertFalse((self.area/'review').exists())
    def test_missing_run_rejected(self):
        with self.assertRaises(ValueError):review.safe_paths(self.area/'missing',self.area/'review',base=self.area)
    def test_mock_manifest_cannot_enter_real_review(self):
        fixture={'schema':'dm-gta-run-manifest/1','completed_at':'synthetic-test-only','dry_run':True,
                 'mode':'real-staged-operator-gated','delivery_eligible':False,'delivery_files':[{}]*12,'metadata_files':[{}]*8}
        (self.run/'run-manifest.json').write_text(json.dumps(fixture),encoding='utf8')
        with self.assertRaises(ValueError):review.finalized_manifest(self.run)
    def test_incomplete_manifest_cannot_enter_real_review(self):
        with self.assertRaises(ValueError):review.finalized_manifest(self.run)


class DecodeContractTests(unittest.TestCase):
    def setUp(self):
        self.probe={'streams':[{'codec_type':'video','codec_name':'h264','pix_fmt':'yuv420p','width':1920,'height':1080,
                        'avg_frame_rate':'30/1','r_frame_rate':'30/1','duration':'60.000000','nb_frames':'1800','nb_read_frames':'1800'},
                    {'codec_type':'audio','codec_name':'aac','sample_rate':'48000','channels':2,'duration':'60.010000'}],
                    'format':{'duration':'60.010000'}}
    def test_header_count_does_not_replace_actual_decode_count(self):
        value=copy.deepcopy(self.probe);del value['streams'][0]['nb_read_frames']
        with self.assertRaises(Exception):review.probe_contract(value)
    def test_decoded_frame_loss_is_rejected_despite_good_header(self):
        value=copy.deepcopy(self.probe);value['streams'][0]['nb_read_frames']='1799'
        with self.assertRaises(Exception):review.probe_contract(value)
    def test_wrong_resolution_is_rejected(self):
        value=copy.deepcopy(self.probe);value['streams'][0]['width']=1280
        with self.assertRaises(ValueError):review.probe_contract(value)
    def test_probe_command_always_requests_decode_single_thread_no_shell(self):
        response=subprocess.CompletedProcess([],0,json.dumps(self.probe),'')
        with patch.object(review.subprocess,'run',return_value=response) as call:
            result,_,argv=review.decode_video('ffprobe.exe','synthetic.mp4')
        self.assertEqual(result['decoded_frames'],1800)
        self.assertIn('-count_frames',argv);self.assertIn('-threads',argv)
        self.assertEqual(argv[argv.index('-threads')+1],'1')
        self.assertNotIn('shell',call.call_args.kwargs)
        self.assertEqual(call.call_args.kwargs['timeout'],300)


if __name__=='__main__':unittest.main(verbosity=2)
