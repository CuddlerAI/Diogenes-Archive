"""Synthetic file safety tests only; no game, media claims, network or real staging."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('staging_under_test', BASE / 'stage-finished-scene-20260920.py')
stage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stage)


class SafetyTests(unittest.TestCase):
    def setUp(self):
        self.test_root = BASE / '_single-scene-staging-tests'
        self.test_root.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix='SYNTHETIC-', dir=self.test_root)
        self.area = Path(self.temp.name).resolve()
        self.source = self.area / 'SYNTHETIC-source.txt'
        self.source.write_bytes(b'SYNTHETIC FILE NOT VIDEO OR REAL EVIDENCE')
        self.row = {'object_relative_path': 'synthetic/source.txt', **stage.seal(self.source)}
        self.out = self.area / 'r2-staging-20260920-99/finished-synthetic'
        self.destination = stage.REMOTE_ROOT + 'finished-20260920-synthetic'

    def tearDown(self):
        assert self.area.is_relative_to(self.test_root.resolve())
        self.temp.cleanup()

    def test_frozen_existing_output_is_never_overwritten(self):
        self.out.mkdir(parents=True)
        marker = self.out / 'original'
        marker.write_bytes(b'KEEP')
        with self.assertRaisesRegex(ValueError, 'already exists'):
            stage.freeze(self.out, [self.row], {'synthetic': True}, self.destination, {})
        self.assertEqual(marker.read_bytes(), b'KEEP')

    def test_escaping_object_path_rejected_before_output_creation(self):
        for relative in ('../escape', '/absolute', 'C:/outside', 'a\\escape'):
            with self.subTest(relative=relative), self.assertRaisesRegex(ValueError, 'Unsafe object'):
                stage.freeze(self.out, [dict(self.row, object_relative_path=relative)], {}, self.destination, {})
            self.assertFalse(self.out.exists())

    def test_changed_source_never_gets_success_receipt(self):
        self.source.write_bytes(b'CHANGED')
        with self.assertRaisesRegex(ValueError, 'changed before copy'):
            stage.freeze(self.out, [self.row], {}, self.destination, {})
        self.assertFalse((self.out / 'STAGING-RECEIPT.json').exists())
        self.assertEqual(self.source.read_bytes(), b'CHANGED')

    def test_frozen_copy_is_independent_and_has_no_approval(self):
        # Deliberately synthetic file-only test; this does not call collect or
        # pretend this tiny file is a completed five-video scene.
        result = stage.freeze(self.out, [self.row], {'synthetic_test_only': True}, self.destination, {})
        self.assertEqual(result['status'], 'prepared_not_uploaded')
        receipt = json.loads((self.out / 'STAGING-RECEIPT.json').read_bytes())
        self.assertEqual(receipt['copied_video_count'], 0)
        self.assertFalse(receipt['upload_executed'])
        self.assertFalse(receipt['delivery_eligible'])
        target = self.out / 'synthetic/source.txt'
        self.assertNotEqual(target.stat().st_ino, self.source.stat().st_ino)
        self.source.write_bytes(b'FUTURE ORIGINAL EDIT')
        self.assertEqual(target.read_bytes(), b'SYNTHETIC FILE NOT VIDEO OR REAL EVIDENCE')

    def test_target_cannot_overlap_run_or_escape_task_batch(self):
        run = self.area / 'run'
        review = self.area / 'review/scene-review.json'
        preview = self.area / 'preview'
        for out in (run / 'output', self.area.parent / 'outside', self.area / 'not-r2/output'):
            with self.subTest(out=out), self.assertRaises(ValueError):
                stage.safe_paths(run, review, preview, out, self.destination, base=self.area)

    def test_destination_rejects_bucket_root_or_unrelated_prefix(self):
        for dest in ('r2:diogenes-tempbucket', stage.REMOTE_ROOT, stage.REMOTE_ROOT + '../escape',
                     'r2:another-bucket/finished-20260920-synthetic'):
            with self.subTest(destination=dest), self.assertRaisesRegex(ValueError, 'Destination'):
                stage.safe_paths(self.area / 'run', self.area / 'review/scene-review.json',
                                 self.area / 'preview', self.out, dest, base=self.area)


if __name__ == '__main__':
    unittest.main(verbosity=2)
