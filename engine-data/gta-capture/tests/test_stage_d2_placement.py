"""D2 position placement must never replace its target or weaken the checks."""
import copy
import math
import unittest

from gta_capture import stage_camera_paths as paths
from gta_capture.errors import ContractError
from gta_capture.tape import _look_at_euler


class D2PlacementTests(unittest.TestCase):
    def inputs(self):
        focus = [[7.0 + math.sin(k / 30.0), -5.0, 2.84 + 0.03 * math.sin(k / 15.0)] for k in range(120)]
        return dict(frame=(0., 0., 1.85, 0.), sand_z=1.85, focus=focus,
                    look=[[p[0], p[1], p[2] + 0.3] for p in focus],
                    people=[[[0., 1., 2.84]] for _ in focus], walls=[])

    def test_anchor_is_bounded_but_focus_aim_and_input_observations_are_not_moved(self):
        kwargs = self.inputs()
        saved = copy.deepcopy(kwargs)
        world = paths._D2PlacementWorld(**kwargs)
        self.assertEqual(kwargs, saved)
        self.assertEqual(world.focus, saved["focus"])
        self.assertEqual(world.look, saved["look"])
        for k, anchor in enumerate(world.placement_anchor):
            self.assertLess(math.hypot(anchor[0], anchor[1] - 1.6), 4.)
            self.assertGreater(math.hypot(world.focus[k][0], world.focus[k][1] - 1.6), 4.)
            self.assertEqual(world.people[k], saved["people"][k] + [saved["focus"][k]])
            pos, rot = world.pose(k, 31., 8., 2.4, 0., 0.)
            self.assertAlmostEqual(math.dist(pos[:2], anchor[:2]), 8.)
            self.assertAlmostEqual(pos[2], 1.85 + 2.4)
            self.assertEqual(rot, _look_at_euler(pos, saved["look"][k]))

    def test_exact_target_clearance_and_original_motion_checks_are_still_enforced(self):
        world = paths._D2PlacementWorld(**self.inputs())
        self.assertIs(type(world).trouble, paths.PathWorld.trouble)
        self.assertIs(type(world).hidden, paths.PathWorld.hidden)
        self.assertIn("from a person", world.trouble(0, (world.focus[0], [0., 0., 0.]), world.fresh_runs()))
        # A deliberately violent yaw sequence must fail the original gate;
        # smoothing the positional anchor gives it no exemption.
        poses = [([0., -8., 4.], [0., 0., 30. if k % 2 else 0.]) for k in range(120)]
        plain = paths.PathWorld(frame=world.frame, sand_z=world.sand_z, focus=world.focus, look=world.look,
                                people=world.people, walls=world.walls, room=world.room)
        problems, report = paths.check_path(poses, world=world, focal_mm=50)
        self.assertEqual((problems, report), paths.check_path(poses, world=plain, focal_mm=50))
        self.assertTrue(any(p.startswith("ang_jerk p95") for p in problems), problems)

    def test_special_placement_cannot_be_selected_for_another_lane_or_lens(self):
        args = self.inputs(); args.pop("walls")
        for extra in ({"key": "AB2"}, {"focal_mm": 24}, {"room": "centroid_wide"}, {"placement": "unknown"}):
            call = dict(key="D2", seed=3, room="centroid_tele", focal_mm=50,
                        placement="d2_player_anchor", **args)
            call.update(extra)
            with self.subTest(extra=extra), self.assertRaisesRegex(ContractError, "restricted to C2/D2"):
                paths.build_free_path(**call)

    def test_changed_target_height_or_nonfinite_root_is_rejected(self):
        args = self.inputs(); args["look"][5][2] += 0.1
        with self.assertRaisesRegex(ContractError, "unchanged seventh root"):
            paths._D2PlacementWorld(**args)
        args = self.inputs(); args["focus"][5][0] = float("nan")
        with self.assertRaisesRegex(ContractError, "finite XYZ"):
            paths._D2PlacementWorld(**args)


if __name__ == "__main__":
    unittest.main()
