import unittest

from gta_capture.stage_audio import RATE, chop_scores, chop_windows, swap_low_band

try:
    import numpy as np
except ImportError:   # the sound is cleaned where it is encoded, on the capture machine
    np = None


def band_power(x, lo, hi):
    spectrum = np.abs(np.fft.rfft(x.mean(axis=1))) ** 2
    f = np.fft.rfftfreq(len(x), 1 / RATE)
    return spectrum[(f >= lo) & (f < hi)].sum()


@unittest.skipUnless(np is not None, "numpy is not installed")
class SwapLowBandTest(unittest.TestCase):
    """40 s of surf-like noise with a far-off rotor from 15 s to 27 s: 16 Hz harmonics
    on 64-128 Hz, the way beach-demo-003 AB's chop looked (80 Hz the strongest)."""

    @classmethod
    def setUpClass(cls):
        rng = np.random.default_rng(7)
        t = np.arange(40 * RATE) / RATE
        noise = rng.normal(0, 0.01, (len(t), 2))
        rotor = sum(a * np.sin(2 * np.pi * f * t) for f, a in ((64, 0.5), (80, 1.0), (96, 0.8), (112, 0.4), (128, 0.3)))
        rotor *= 0.02 * ((t >= 15) & (t < 27))
        cls.x = noise + rotor[:, None]
        cls.scores = chop_scores(cls.x)

    def test_finds_the_chopping_seconds_and_pads_them(self):
        self.assertTrue(all(self.scores[s] > 6 for s in range(16, 26)), self.scores.round(1))
        self.assertTrue(all(self.scores[s] < 5 for s in list(range(0, 14)) + list(range(28, 40))), self.scores.round(1))
        self.assertEqual(chop_windows(self.scores), [(12, 30)])

    def test_the_swap_takes_the_chop_out_and_leaves_the_rest(self):
        y = swap_low_band(self.x, [(12, 30)], [(0, 12), (30, 40)])   # longer than either quiet stretch: spliced
        after = chop_scores(y)
        self.assertTrue(all(after[s] < 5 for s in range(12, 30)), after.round(1))
        # the seconds that did not chop stay as they were, sample for sample
        np.testing.assert_allclose(y[:11 * RATE], self.x[:11 * RATE], atol=1e-9)
        np.testing.assert_allclose(y[31 * RATE:], self.x[31 * RATE:], atol=1e-9)
        # and inside the swapped stretch everything above the crossfade is the recorded sound
        seg = slice(16 * RATE, 26 * RATE)
        self.assertAlmostEqual(band_power(y[seg], 600, 20000) / band_power(self.x[seg], 600, 20000), 1.0, places=2)
        self.assertLess(band_power(y[seg], 40, 200), 0.05 * band_power(self.x[seg], 40, 200))


    def test_a_chop_running_to_the_end_is_swapped_to_the_end(self):
        # beach-demo-003 C: the rotor went on to the last second, and the swap faded back
        # into it there (8.1 after, against 9.3 before)
        rng = np.random.default_rng(11)
        t = np.arange(20 * RATE + RATE // 2) / RATE
        rotor = sum(a * np.sin(2 * np.pi * f * t) for f, a in ((64, 0.5), (80, 1.0), (96, 0.8), (112, 0.4), (128, 0.3)))
        x = rng.normal(0, 0.01, (len(t), 2)) + (0.02 * rotor * (t >= 12))[:, None]
        windows = chop_windows(chop_scores(x))
        self.assertEqual(windows, [(9, 20)])
        after = chop_scores(swap_low_band(x, windows, [(0, 9)]))
        self.assertTrue(all(after[s] < 5 for s in range(9, 20)), after.round(1))


if __name__ == "__main__":
    unittest.main()
