import importlib.util
import json
import unittest
from pathlib import Path

TOOL = Path(__file__).resolve().parents[1] / "tools" / "route_sim" / "make_tables.py"
_spec = importlib.util.spec_from_file_location("route_sim_make_tables", TOOL)
mt = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mt)

FRAME = (100.0, 200.0, 1.5, 90.0)   # stage x is world +y here, stage y is world -x


def tick(k, shift=0.0):
    """One line of world telemetry: everybody at world (100 + i, 201 + shift), root 2.5 m high, a hand 0.5 m north of the root."""
    actors = []
    for i, name in enumerate(mt.IDS + [mt.PLAYED]):
        pos = [100.0 - i, 201.0 + shift, 2.5]
        bones = [[pos[0], pos[1], pos[2] - 0.05]] + [[pos[0], pos[1], pos[2] + 0.2]] * 2 + [[pos[0], pos[1] + 0.5, 3.0]] + [[pos[0], pos[1], 1.6]] * 3
        actors.append({"id": name, "state": {"pos": pos, "h": 200.0, "bones": bones}})
    return json.dumps({"tick": k, "actors": actors})


class TablesTest(unittest.TestCase):
    def test_the_frame_comes_from_the_spec(self):
        self.assertEqual(mt.stage_frame("ticks 3600\nframe 1 2 3 4  # x y z heading\n"), (1.0, 2.0, 3.0, 4.0))
        with self.assertRaises(SystemExit):
            mt.stage_frame("ticks 3600\n")

    def test_positions_bones_and_skipped_ticks(self):
        cast, bones, pacer, sand = mt.build([tick(1), tick(3, shift=1.0)], FRAME)
        self.assertEqual((len(cast), len(bones), len(pacer)), (3600, 3600, 3600))
        self.assertEqual(len(pacer[1].split()), 21)   # the pacer's 7 bones: D's seventh sees him (route_sim KIND=D)
        self.assertAlmostEqual(sand, 2.5 - mt.ROOT_ABOVE_SAND_M)
        row = [float(v) for v in cast[1].split()]
        self.assertEqual(len(row), 13)
        # chat_a at world (100, 201): one metre along stage x, none along y; chat_b one metre south of it in world x
        self.assertAlmostEqual(row[0], 1.0, places=3); self.assertAlmostEqual(row[1], 0.0, places=3)
        self.assertAlmostEqual(row[2], 1.0, places=3); self.assertAlmostEqual(row[3], 1.0, places=3)
        self.assertAlmostEqual(row[12], 110.0, places=3)   # the played actor's heading: world 200 less the frame's 90
        # tick 0 and the skipped tick 2 keep tick 1; tick 3 moved one metre north in the world (stage x)
        self.assertEqual(cast[0], cast[1]); self.assertEqual(cast[2], cast[1])
        self.assertAlmostEqual(float(cast[3].split()[0]), 2.0, places=3)
        # a hand 0.5 m north of the root is 0.5 m along stage x, at 3.0 - sand over the sand
        hand = [float(v) for v in bones[1].split()][9:12]
        self.assertAlmostEqual(hand[0], 0.5, places=3); self.assertAlmostEqual(hand[1], 0.0, places=3)
        self.assertAlmostEqual(hand[2], 3.0 - sand, places=3)

    def test_a_take_without_the_whole_cast_is_refused(self):
        line = json.loads(tick(1)); line["actors"].pop()
        with self.assertRaises(SystemExit):
            mt.build([json.dumps(line)], FRAME)


if __name__ == "__main__":
    unittest.main()
