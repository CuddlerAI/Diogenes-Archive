import copy
from pathlib import Path
import tempfile
import unittest

import numpy as np

from diogenes_wm.eval import cp2077_bound_geometry as bound
from diogenes_wm.eval.cp2077_egeo_gate import Thresholds, _sha256
from diogenes_wm.eval.cp2077_selected_geometry import evaluate, relative_motion, sample_populations
from diogenes_wm.eval.geo_probes import quat_to_mat


def sample(position, quaternion, world, camera, scale):
    view = np.eye(4)
    view[:3, :3] = world.T @ quat_to_mat(quaternion) @ camera
    view[3, :3] = -(world.T @ position / scale) @ view[:3, :3]
    return dict(view=view, projection=np.eye(4), camera_observation_qpc=400,
                bound_position=position, bound_quaternion=quaternion,
                bound_camera_basis=camera, bound_scale=scale)


class BoundGeometryTests(unittest.TestCase):
    def test_native_quaternion_motion_recovers_source_motion_with_both_reflection_policies(self):
        swap = np.array([[1, 0, 0], [0, 0, 1], [0, 1, 0]])
        flip = np.diag([1, -1, 1])
        for world, camera in ((swap, np.eye(3)), (np.eye(3), flip)):
            a = sample(np.array([4., -7., 2.]), np.array([0., np.sin(.3), 0., np.cos(.3)]), world, camera, 2.5)
            b = sample(np.array([5., -6., 3.]), np.array([np.sin(.2), 0., 0., np.cos(.2)]), world, camera, 2.5)
            np.testing.assert_allclose(relative_motion(a, b), np.linalg.inv(a['view']) @ b['view'], atol=1e-12)
            # The new path must consume the supplied production pose, not silently re-decode V.
            changed = copy.deepcopy(b)
            changed['bound_position'][0] += 1
            self.assertGreater(np.linalg.norm(relative_motion(a, changed) - relative_motion(a, b)), .1)

    def test_mixed_bound_and_legacy_or_changed_basis_refused(self):
        a = sample(np.zeros(3), np.array([0., 0., 0., 1.]), np.eye(3), np.eye(3), 1.)
        with self.assertRaises(ValueError):
            relative_motion(a, dict(view=np.eye(4)))
        b = copy.deepcopy(a)
        b['bound_scale'] = 2
        with self.assertRaises(ValueError):
            relative_motion(a, b)

    def test_binding_checks_original_matrices_pose_and_qpc_without_fitting(self):
        context = object.__new__(bound.Context)
        context.world = np.array([[1, 0, 0], [0, 0, 1], [0, 1, 0]])
        context.camera, context.scale = np.eye(3), 1.
        s = sample(np.array([4., 7., 2.]), np.array([0., 0., 0., 1.]), context.world, context.camera, 1.)
        row = dict(camera_position=s['bound_position'].tolist(),
                   camera_rotation_quaternion=s['bound_quaternion'].tolist(), present_qpc=400,
                   provenance=dict(view_matrix=s['view'].tolist(), projection_matrix=s['projection'].tolist()))
        context.attach_sample(copy.deepcopy(s), row)
        for change in ('position', 'quaternion', 'qpc', 'matrix'):
            bad = copy.deepcopy(row)
            if change == 'position':
                bad['camera_position'][1] += .1
            elif change == 'quaternion':
                bad['camera_rotation_quaternion'] = [0., 1., 0., 0.]
            elif change == 'qpc':
                bad['present_qpc'] += 1
            else:
                bad['provenance']['view_matrix'][3][0] += .1
            with self.subTest(change=change), self.assertRaises(ValueError):
                context.attach_sample(copy.deepcopy(s), bad)

    def test_session_cannot_borrow_missing_rotation_from_another_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory)/'input.json'
            p.write_text('{}', encoding='utf-8')
            ctx = object.__new__(bound.Context)
            ctx.binding_path = ctx.replay_path = p
            ctx.binding_sha = ctx.replay_sha = _sha256(p)
            ctx.binding, ctx.pose_files = {}, []
            ctx.runs, ctx.sessions = {'a': {}, 'b': {}}, [{}, {}]
            pairs = []
            for session, kind in ((0, 'translation'), (1, 'rotation')):
                for _ in range(40):
                    pairs.append(dict(session_index=session, populations=[kind], baseline_l1=10.,
                        valid_fraction=.8, variants={'observed': {'improvement': 2.}, 'inverse': {'improvement': .5}}))
                for _ in range(5):
                    pairs.append(dict(session_index=session, populations=['static'], baseline_l1=.1))
            result = ctx.finish(dict(pairs=pairs, metric={'evidence_sha256': 'metric'}), evaluate, Thresholds())
            self.assertTrue(any('rotation' in s for s in result['sessions'][0]['gate']['failures']))
            self.assertTrue(any('translation' in s for s in result['sessions'][1]['gate']['failures']))
            self.assertFalse(result['production_binding_authorized'])

    def test_axis_permutation_must_not_repeat_an_axis(self):
        with self.assertRaises(ValueError):
            bound.axis_matrix(dict(delivery_x_from='positive_x', delivery_y_from='negative_x', delivery_z_from='positive_z'))

    def test_small_launch_keeps_its_existing_population_instead_of_being_starved(self):
        population = {'translation': [(0, i, i+1) for i in range(217)] + [(1, i, i+1) for i in range(9)]}
        pooled = sample_populations(population, 120)['translation']
        independent = sample_populations(population, 120, 2)['translation']
        self.assertLess(sum(p[0] == 1 for p in pooled), 5)
        self.assertEqual(sum(p[0] == 1 for p in independent), 9)
        self.assertEqual(sum(p[0] == 0 for p in independent), 40)


if __name__ == '__main__':
    unittest.main()
