from diogenes_phase0.data.gaming500 import frame_events_to_actions


def _key(code, char, down):
    return {"type": "key", "keyCode": code, "characters": char, "isDown": down}


def _move(x, y):
    return {"type": "mouse_move", "x": x, "y": y}


def test_key_hold_maintenance():
    lines = [
        {"frame": 0, "video_t_ms": 0, "events": [_key(87, "w", True)]},
        {"frame": 1, "video_t_ms": 33, "events": []},
        {"frame": 2, "video_t_ms": 67, "events": [_key(87, "w", False)]},
        {"frame": 3, "video_t_ms": 100, "events": []},
    ]
    rows, _ = frame_events_to_actions(lines, "windows")
    assert [r["keys"] for r in rows] == [[87], [87], [], []]


def test_mouse_delta_from_absolute():
    lines = [
        {"frame": 0, "events": [_move(100, 100)]},          # first move: no prior pos
        {"frame": 1, "events": [_move(130, 90)]},            # dx=30 dy=-10
        {"frame": 2, "events": [_move(140, 90), _move(160, 95)]},  # summed: dx=30 dy=5
        {"frame": 3, "events": []},
    ]
    rows, _ = frame_events_to_actions(lines, "windows")
    assert (rows[0]["pointerDx"], rows[0]["pointerDy"]) == (0.0, 0.0)
    assert (rows[1]["pointerDx"], rows[1]["pointerDy"]) == (30.0, -10.0)
    assert (rows[2]["pointerDx"], rows[2]["pointerDy"]) == (30.0, 5.0)
    assert rows[1]["pointerBins"][0] > 0 and rows[1]["pointerBins"][1] < 0


def test_clicks_map_to_mouse_vks():
    lines = [
        {"frame": 0, "events": [{"type": "click", "button": "left", "isDown": True,
                                 "x": 5, "y": 5}]},
        {"frame": 1, "events": [{"type": "click", "button": "left", "isDown": False,
                                 "x": 5, "y": 5}]},
    ]
    rows, _ = frame_events_to_actions(lines, "windows")
    assert rows[0]["keys"] == [1] and rows[1]["keys"] == []


def test_pointer_lock_heuristic():
    center = [_move(960, 540)]
    wander = [_move(960 + i, 540 + i) for i in range(1, 4)]
    lines = []
    for i in range(60):  # every other move returns to dead center
        lines.append({"frame": 2 * i, "events": list(center)})
        lines.append({"frame": 2 * i + 1, "events": [wander[i % 3]]})
    _, quality = frame_events_to_actions(lines, "windows")
    assert quality["suspect_pointer_lock"]

    lines = [{"frame": i, "events": [_move(100 + 3 * i, 200 + 2 * i)]}
             for i in range(200)]
    _, quality = frame_events_to_actions(lines, "windows")
    assert not quality["suspect_pointer_lock"]


def test_unscanned_keys_dropped():
    lines = [{"frame": 0, "events": [_key(160, None, True),   # shift
                                     _key(65, "a", True)]}]
    rows, _ = frame_events_to_actions(lines, "windows")
    assert rows[0]["keys"] == [65]
