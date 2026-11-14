import math

from diogenes_phase0.keys import (
    N_BINS, SCANNED_VKEYS, bin_to_class, foveated_bin, keys_to_multihot,
    map_keycode, map_mouse_button,
)


def _reference_foveated_bin(delta, max_bin=10, mu=8.0):
    """Verbatim re-implementation of data_demo/auto_label.py foveated_bin."""
    if delta == 0 or max_bin <= 0:
        return 0
    sign = 1 if delta > 0 else -1
    mag = abs(delta)
    REF = 200.0
    compressed = math.log(1 + mu * mag) / math.log(1 + mu * REF)
    return sign * max(0, min(max_bin, round(compressed * max_bin)))


def test_foveated_bin_matches_data_demo_reference():
    for d in list(range(-500, 501)) + [0.5, -0.5, 1.7, 199.9, 200.1]:
        assert foveated_bin(d) == _reference_foveated_bin(d), d


def test_bin_class_range():
    classes = {bin_to_class(foveated_bin(d)) for d in range(-1000, 1000)}
    assert min(classes) >= 0 and max(classes) < N_BINS
    assert bin_to_class(0) == 10


def test_multihot_drops_unscanned():
    hot = keys_to_multihot([87, 999, 1])
    assert sum(hot) == 2
    assert hot[SCANNED_VKEYS.index(87)] == 1
    assert hot[SCANNED_VKEYS.index(1)] == 1


def test_keycode_mapping():
    assert map_keycode(None, "w") == 87
    assert map_keycode(None, "W") == 87
    assert map_keycode(None, "3") == 51
    assert map_keycode(87, None, "windows") == 87       # identity for VK in vocab
    assert map_keycode(160, None, "windows") is None    # shift: unscanned
    assert map_keycode(13, None, "macos") == 87         # kVK_ANSI_W
    assert map_keycode(23, None, "macos") == 53         # kVK_ANSI_5
    assert map_mouse_button("left") == 1
    assert map_mouse_button("right") == 2
    assert map_mouse_button("middle") is None
