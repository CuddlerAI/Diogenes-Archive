import unittest

from gta_capture.errors import ContractError
from gta_capture.stage_take import SpecHeader, build_take_scenario, check_route, check_take_settings, check_turn

SPEC = SpecHeader(ticks=3600, actors=("chat_a", "chat_b", "pacer", "tourist"), rules=(), props=())
ROUTE = "13 -1.84 1.51 5 chat_a; 23 2.05 3.12 5 tourist w; 32.5 -0.2 1.9 14 -"


class RouteTest(unittest.TestCase):
    def test_a_route_reads_as_its_steps(self):
        steps = check_route(ROUTE + ";", SPEC, taken="pacer")   # a trailing ';' is nothing
        self.assertEqual(steps, [(13.0, -1.84, 1.51, 5.0, "chat_a", "", "walk"), (23.0, 2.05, 3.12, 5.0, "tourist", "w", "walk"),
                                 (32.5, -0.2, 1.9, 14.0, "-", "", "walk")])
        # the keys any that walk (the customer, 2026-10-16: not W alone), "-" the nearest; then tap or step
        self.assertEqual(check_route("13 1 2 5 chat_a sd; 13 1 2 5 - tap; 14 1 2 5 - a step; 15 1 2 5 - - tap", SPEC, taken="pacer"),
                         [(13.0, 1.0, 2.0, 5.0, "chat_a", "sd", "walk"), (13.0, 1.0, 2.0, 5.0, "-", "", "tap"),
                          (14.0, 1.0, 2.0, 5.0, "-", "a", "step"), (15.0, 1.0, 2.0, 5.0, "-", "", "tap")])
        # a key timetable (the customer, 2026-10-16: a big turn walked round as W, W+D, S+D, S), the mouse untouched
        self.assertEqual(check_route("1 2.4 -8.2 8 - w:2,wd:1,sd:1,s:1; 12 0 -9 8 - wd:0.5", SPEC, taken="pacer"),
                         [(1.0, 2.4, -8.2, 8.0, "-", "w:2,wd:1,sd:1,s:1", "timetable"), (12.0, 0.0, -9.0, 8.0, "-", "wd:0.5", "timetable")])
        # D's seventh person walks one too, and may look at anybody of the stage
        self.assertEqual(check_route("0 1 2 5 pacer d", SPEC, taken=None, kind="D"), [(0.0, 1.0, 2.0, 5.0, "pacer", "d", "walk")])

    def test_what_a_route_may_not_be(self):
        for bad, why in (("", "non-empty"), ("13 1 2 5", "five fields"), ("13 1 2 5 chat_a w extra", "the keys"),
                         ("13 1 2 5 chat_a ws", "the keys"), ("13 1 2 5 chat_a WA", "the keys"), ("13 1 2 5 chat_a step w", "the keys"),
                         ("13 1 2 5 chat_a tap tap", "the keys"), ("13 1 2 5 chat_a q", "the keys"),
                         ("13 1 2 5 - w:2 tap", "the keys"), ("13 1 2 5 - w:2,", "the keys"), ("13 1 2 5 - w:0.05", "the keys"),
                         ("13 1 2 5 - q:1", "the keys"), ("13 1 2 5 - w:11", "the keys"), ("13 1 2 5 - w:x", "the keys"),
                         ("13 1 2 5 - w:10,s:10,w:10,s:1", "the keys"), ("13 1 2 5 - w:2 w", "the keys"),
                         ("13 x 2 5 chat_a", "numbers"), ("13 1 2 -1 chat_a", "out of range"),
                         ("13 40 2 5 chat_a", "out of range"), ("20 1 2 5 -; 13 1 2 5 -", "time order"),
                         ("13 1 2 5 nobody", "nobody to look at"), ("13 1 2 5 pacer", "nobody to look at")):
            with self.subTest(bad=bad), self.assertRaisesRegex(ContractError, why):
                check_route(bad, SPEC, taken="pacer")

    def test_the_group_gives_take_c_its_route(self):
        group = {"group_id": "g", "duration_s": 121, "environment": {"location": [0, 0, 0], "clock": "12:00", "weather": "EXTRASUNNY",
                                 "source_epoch": "2026-09-23 12:00:00.000", "warmup_s": 20},
                 "seed": 1, "takeover": {"actor": "pacer", "at_s": 11, "route": ROUTE}}
        take = build_take_scenario(group=group, kind="C", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"]
        self.assertEqual((take["takeover_at_s"], take["route"]), (11, ROUTE))
        check_take_settings("C", take, SPEC)
        del group["takeover"]["route"]   # no route: the program wanders, as before
        take = build_take_scenario(group=group, kind="C", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"]
        self.assertNotIn("route", take)
        take["route"] = "13 1 2 5 nobody"
        with self.assertRaises(ContractError):
            check_take_settings("C", take, SPEC)


    def test_the_group_sets_the_same_wind_in_every_take(self):
        # the user, 2026-10-16: wind -- the flags and the ferris wheel moving; set the same in every take
        group = {"group_id": "g", "duration_s": 121, "environment": {"location": [0, 0, 0], "clock": "12:00", "weather": "EXTRASUNNY",
                                 "source_epoch": "2026-09-23 12:00:00.000", "warmup_s": 20, "wind": {"speed": 3.5, "direction_deg": 120}},
                 "seed": 1, "takeover": {"actor": "pacer", "at_s": 11}, "seventh": {"model": "a_m_y_beach_02"}}
        for kind in ("AB", "C", "D"):
            sc = build_take_scenario(group=group, kind=kind, spec_name="stage-spec.txt", spec_sha256="0" * 64)
            self.assertEqual((sc["wind_speed"], sc["wind_direction_deg"]), (3.5, 120.0))
        for bad in ({"speed": 13}, {"speed": -1}, {"direction_deg": 10}, 3.0):
            group["environment"]["wind"] = bad
            with self.subTest(bad=bad), self.assertRaises(ContractError):
                build_take_scenario(group=group, kind="AB", spec_name="stage-spec.txt", spec_sha256="0" * 64)
        del group["environment"]["wind"]   # not given: no wind, as till 2026-10-16
        self.assertNotIn("wind_speed", build_take_scenario(group=group, kind="AB", spec_name="stage-spec.txt", spec_sha256="0" * 64))

    def test_how_the_standing_body_turns(self):
        # the customer, 2026-10-10: turning on the spot was too fast -- turn slower, through smaller angles
        for good in ("game", "steps", "glide", "glide 50 100", "glide 90 150", " glide  45 90 ", "walk", "walk 40", "walk 40 straight", "walk straight"):
            with self.subTest(good=good):
                self.assertEqual(check_turn(good), good)
        for bad, why in (("", "game, steps, glide"), ("fast", "game, steps, glide"), ("steps 30", "no numbers"),
                         ("glide 50", "game, steps, glide"), ("glide 50 x", "a rate and an acceleration"),
                         ("glide 120 100", "10-90 deg/s"), ("glide 50 300", "20-150"), (45, "game, steps, glide"),
                         ("walk x", "camera's deg/s"), ("walk 5", "10-90 deg/s"), ("walk 40 1", "game, steps, glide"),
                         ("walk straight 40", "game, steps, glide")):
            with self.subTest(bad=bad), self.assertRaisesRegex(ContractError, why):
                check_turn(bad)
        group = {"group_id": "g", "duration_s": 121, "environment": {"location": [0, 0, 0], "clock": "12:00", "weather": "EXTRASUNNY",
                                 "source_epoch": "2026-09-23 12:00:00.000", "warmup_s": 20},
                 "seed": 1, "takeover": {"actor": "pacer", "at_s": 11, "route": ROUTE},
                 "seventh": {"model": "a_m_y_beach_02"}, "turn": "glide 50 100"}
        for kind in ("C", "D"):
            take = build_take_scenario(group=group, kind=kind, spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"]
            self.assertEqual(take["turn"], "glide 50 100")
            check_take_settings(kind, take, SPEC)
        ab = build_take_scenario(group=group, kind="AB", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"]
        self.assertNotIn("turn", ab)   # the free camera flies; nobody stands
        with self.assertRaisesRegex(ContractError, "nobody stands"):
            check_take_settings("AB", dict(ab, turn="steps"), SPEC)
        # the walk style (2026-10-16) leaves a standing body to the keys: only a route's taps turn it, so D needs one too
        group["turn"] = "walk 60"
        check_take_settings("C", build_take_scenario(group=group, kind="C", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"], SPEC)
        with self.assertRaisesRegex(ContractError, "turn walk needs a route"):
            check_take_settings("D", build_take_scenario(group=group, kind="D", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"], SPEC)
        group["seventh"]["route"] = "0 1 2 5 - d; 0 -2 1 8 - - tap"
        take = build_take_scenario(group=group, kind="D", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"]
        self.assertEqual(take["route"], "0 1 2 5 - d; 0 -2 1 8 - - tap")
        check_take_settings("D", take, SPEC)
        with self.assertRaisesRegex(ContractError, "nobody walks a route"):
            check_take_settings("AB", {"route": "0 1 2 5 -"}, SPEC)
        del group["turn"]   # not given: the game's own turn, as recorded until 2026-10-13
        self.assertNotIn("turn", build_take_scenario(group=group, kind="C", spec_name="stage-spec.txt", spec_sha256="0" * 64)["take"])


if __name__ == "__main__":
    unittest.main()
