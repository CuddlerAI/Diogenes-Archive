import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("game_settings_profile", Path(__file__).resolve().parents[1] / "tools" / "game_settings_profile.py")
gsp = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gsp)

BODY = ('<?xml version="1.0" encoding="UTF-8"?>\n<Settings>\n  <graphics>\n'
        + "".join(f'    <{tag} value="{value}" />\n' for tag, value in {**gsp.EXPORT, **gsp.KEEP}.items())
        + '    <DoF value="false" />\n  </graphics>\n</Settings>\n')


class GameSettingsProfileTests(unittest.TestCase):
    def test_record_and_export_swap_only_their_own_values_and_keep_a_backup(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.xml"
            path.write_text(BODY, encoding="utf-8")
            self.assertEqual(gsp.main(["record", "--settings", str(path)]), 0)
            text = path.read_text(encoding="utf-8")
            for tag, value in gsp.RECORD.items():
                self.assertEqual(gsp.value_of(text, tag), value, tag)
            for tag, value in gsp.KEEP.items():
                self.assertEqual(gsp.value_of(text, tag), value, tag)     # resolution, Post FX, motion blur untouched
            self.assertEqual(gsp.value_of(text, "DoF"), "false")
            self.assertEqual(len(list(Path(tmp).glob("settings.xml.before-record-*"))), 1)
            self.assertEqual(gsp.main(["export", "--settings", str(path)]), 0)
            self.assertEqual(path.read_text(encoding="utf-8"), BODY)          # a round trip is byte-exact
            self.assertEqual(gsp.main(["show", "--settings", str(path)]), 0)

    def test_the_8gb_candidate_keeps_the_supersampling_and_drops_msaa(self):
        # 2026-10-13: 3840x2160 with 4x MSAA is what an 8 GB card runs out of; the x2 supersampling stays
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.xml"
            path.write_text(BODY, encoding="utf-8")
            self.assertEqual(gsp.main(["export-8gb", "--settings", str(path)]), 0)
            text = path.read_text(encoding="utf-8")
            for tag, value in {"SamplingMode": "8", "MSAA": "0", "TXAA_Enabled": "false", "ReflectionMSAA": "0",
                               "FXAA_Enabled": "true", "MaxLodScale": "1.000000", "ShadowQuality": "3"}.items():
                self.assertEqual(gsp.value_of(text, tag), value, tag)
            self.assertEqual(gsp.main(["export", "--settings", str(path)]), 0)
            self.assertEqual(path.read_text(encoding="utf-8"), BODY)

    def test_a_file_that_is_not_ours_is_left_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.xml"
            odd = BODY.replace('<ScreenWidth value="1920" />', '<ScreenWidth value="2560" />')
            path.write_text(odd, encoding="utf-8")
            self.assertEqual(gsp.main(["export", "--settings", str(path)]), 4)
            self.assertEqual(path.read_text(encoding="utf-8"), odd)
            path.write_text(BODY.replace('<MSAA value="4" />', ''), encoding="utf-8")
            self.assertEqual(gsp.main(["record", "--settings", str(path)]), 5)
            self.assertEqual(gsp.main(["record", "--settings", str(Path(tmp) / "missing.xml")]), 2)


if __name__ == "__main__":
    unittest.main()
