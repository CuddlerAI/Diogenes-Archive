"""Validate the local Steam release-test catalogs added to DataMiner."""

import hashlib
import json
from pathlib import Path

import pytest


REPO = Path(__file__).resolve().parents[3]
CATALOGS = REPO / "DataMiner/config/catalogs/dev"
LOCAL_STEAM_GAMES = {
    "a-plague-tale-innocence-steam-release-test": ["APlagueTaleInnocence_x64.exe"],
    "total-war-warhammer-iii-steam-release-test": ["Warhammer3.exe"],
    "hades-ii-steam-release-test": ["Hades2.exe"],
    "fallout4-steam-release-test": ["Fallout4.exe"],
    "cyberpunk-2077-steam": ["Cyberpunk2077.exe"],
    # 本机取证包 dataminer-game-probe-4games（2026-08-19）。这四款按 owner 决定
    # 不钉 window_class / executable_sha256，身份只按 exe 文件名匹配；
    # 上面的参数化用例不要求那两项（Cyberpunk 那条甚至断言 window_class 为 None）。
    "deathloop-pc": ["Deathloop.exe"],
    "atomic-heart-pc": ["AtomicHeart-Win64-Shipping.exe"],
    "doom-eternal-pc": ["DOOMEternalx64vk.exe", "DOOMSandBox64vk.exe"],
    "resident-evil-village-pc": ["re8.exe"],
    # 本机取证包 game-probe-7-games-20260820。GTA V Legacy 已于 2026-09-02
    # 单独钉住 exact executable_sha256，但 window_class 仍为空；其余条目仍不钉
    # 两项。玩法仍只取步战面（极限竞速本身是驾驶游戏，取驾驶面）。
    "avatar-frontiers-of-pandora-pc": ["afop.exe"],
    "dying-light-pc": ["DyingLightGame.exe"],
    "far-cry-5-pc": ["FarCry5.exe"],
    "forza-horizon-5-pc": ["ForzaHorizon5.exe"],
    "gta5-legacy-pc": ["GTA5.exe"],
    "indiana-jones-great-circle-pc": ["TheGreatCircle.exe"],
    "titanfall-2-pc": ["Titanfall2.exe"],
    "prey-2017-pc": ["Prey.exe"],
    "red-dead-redemption-2-pc": ["RDR2.exe"],
    "starfield-pc": ["Starfield.exe"],
}
FIVE_FILES = [
    "catalog.json",
    "game-profile.json",
    "mapping.json",
    "recording-profile.json",
    "device-profile.json",
]
CATALOG_PAYLOADS = {
    "device_profiles": "device-profile.json",
    "recording_profiles": "recording-profile.json",
    "game_profiles": "game-profile.json",
    "mappings": "mapping.json",
}
EXTRA_CATALOG_PAYLOADS = {
    "fallout4-steam-release-test": {
        "depth_profiles": "depth-profile.json",
        "camera_bindings": "camera-binding.json",
    }
}
VK_TO_SCAN = {
    8: 14,
    13: 28,
    27: 1,
    49: 2,
    50: 3,
    51: 4,
    52: 5,
    53: 6,
    54: 7,
    55: 8,
    56: 9,
    57: 10,
    48: 11,
    189: 12,
    187: 13,
    9: 15,
    87: 17,
    65: 30,
    83: 31,
    68: 32,
    81: 16,
    69: 18,
    82: 19,
    84: 20,
    73: 23,
    79: 24,
    70: 33,
    71: 34,
    74: 36,
    75: 37,
    88: 45,
    67: 46,
    86: 47,
    66: 48,
    77: 50,
    188: 51,
    190: 52,
    32: 57,
    16: 42,
    17: 29,
    18: 56,
    20: 58,
    33: 73,
    34: 81,
    35: 79,
    36: 71,
    45: 82,
    46: 83,
    72: 35,
    78: 49,
    80: 25,
    85: 22,
    90: 44,
    116: 63,
    117: 64,
    119: 66,
    121: 68,
    122: 87,
    # 这张表原先只覆盖已入册游戏用到的键，新游戏一用到功能键或方向键就报「未知 VK」。
    # 补的这几个都照抄上线代码里的权威表（catalog.rs 的 SET1_TO_VK /
    # SET1_EXTENDED_TO_VK）——那张表才是客户端加载 catalog 时真正用的反查表。
    76: 38,   # L
    89: 21,   # Y
    112: 59,  # F1
    118: 65,  # F7
    219: 26,  # [
    221: 27,  # ]
    # 方向键是 E0 扩展码；下面的断言只比 scan 值、不比 extended 位，够用。
    37: 75,   # Left
    38: 72,   # Up
    39: 77,   # Right
    40: 80,   # Down
}

pytestmark = pytest.mark.skipif(not CATALOGS.exists(), reason="DataMiner catalogs missing")


@pytest.mark.parametrize(("game", "executables"), LOCAL_STEAM_GAMES.items())
def test_local_steam_catalog_is_complete_and_consistent(game, executables):
    directory = CATALOGS / game
    for filename in FIVE_FILES:
        assert (directory / filename).is_file(), f"{game} missing {filename}"

    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))
    game_profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))
    recording = json.loads(
        (directory / "recording-profile.json").read_text(encoding="utf-8")
    )

    # issue #855 PR 2 断代：活跃 bundle 一律中性 schema，`non_trainable` 整个字段
    # 缺席——录制配置不再兼职表达训练资格，可训练性只由 clip_trainability 派生。
    # "字段必须不在"比"字段为 true"更强：可选意味着 authoring 侧还能用它表达训练
    # 资格，而读者早已不再消费。
    assert catalog["schema_version"] == "dm-catalog-bundle-3"
    assert "non_trainable" not in catalog
    assert game_profile["schema_version"] == "dm-game-profile-3"
    assert game_profile["game_id"] == game
    assert "non_trainable" not in game_profile
    assert game_profile["executable_names"] == executables
    assert game_profile["mapping_id"] == mapping["mapping_id"]
    assert recording["game_id"] == game
    assert recording["profile_id"] == game_profile["recording_profile_id"]

    catalog_payloads = CATALOG_PAYLOADS | EXTRA_CATALOG_PAYLOADS.get(game, {})
    for category, filename in catalog_payloads.items():
        assert (directory / filename).is_file(), f"{game} missing {filename}"
        entries = catalog[category]
        assert len(entries) == 1, f"{game} must have exactly one {category} entry"
        entry = entries[0]
        digest = hashlib.sha256((directory / filename).read_bytes()).hexdigest()
        assert entry["content_hash"] == f"sha256:{digest}"


@pytest.mark.parametrize("game", LOCAL_STEAM_GAMES)
def test_local_steam_input_contract_matches_mapping(game):
    directory = CATALOGS / game
    game_profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))

    mapping_scans = {
        binding["scan_code"]
        for binding in mapping["bindings"]
        if binding.get("device") == "keyboard"
    }
    for vk in game_profile["input_contract"]["keyboard_vks"]:
        if vk in game_profile["input_contract"]["control_vks"]:
            continue
        assert vk in VK_TO_SCAN, f"{game}: unknown VK {vk}"
        assert VK_TO_SCAN[vk] in mapping_scans, f"{game}: VK {vk} has no mapping"

    mapping_buttons = {
        binding["button"] for binding in mapping["bindings"] if "button" in binding
    }
    assert set(game_profile["input_contract"]["mouse_buttons"]) == mapping_buttons


def test_hades_ii_live_identity_and_native_geometry_are_pinned():
    directory = CATALOGS / "hades-ii-steam-release-test"
    game_profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    recording = json.loads(
        (directory / "recording-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))
    device = json.loads(
        (directory / "device-profile.json").read_text(encoding="utf-8")
    )

    assert game_profile["version"] == "6"
    assert game_profile["window_class"] == "The Forge"
    assert game_profile["executable_sha256"] == (
        "sha256:df11c588771105d6e90885a949c512044268ba15b175220decb168da93763ecf"
    )
    assert mapping["version"] == "2026-08-25.0"
    assert (recording["width"], recording["height"]) == (2560, 1440)
    assert recording["window_mode"] == "borderless"
    assert device["version"] == "5"
    assert "build_24432219" in device["evidence_class"]


def test_cyberpunk_2077_shipped_rgb_profile_is_pinned():
    directory = CATALOGS / "cyberpunk-2077-steam"
    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))
    game_profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    recording = json.loads(
        (directory / "recording-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))
    device = json.loads(
        (directory / "device-profile.json").read_text(encoding="utf-8")
    )

    assert game_profile["version"] == "4"
    assert game_profile["display_name"] == "Cyberpunk 2077 (Steam)"
    assert game_profile["window_class"] is None
    assert game_profile["executable_sha256"] == (
        "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991"
    )
    assert (recording["width"], recording["height"]) == (1920, 1080)
    assert (recording["fps_num"], recording["fps_den"]) == (30, 1)
    assert recording["window_mode"] == "fullscreen_or_borderless"
    assert recording["hdr"] is False
    assert recording["version"] == "3"
    assert recording["field_of_view_degrees"] == 75.0
    assert recording["audio"] == {
        "mode": "process_loopback",
        "sample_rate_hz": 48000,
        "channels": 2,
        "bits_per_sample": 16,
        "sample_format": "pcm_s16le",
        "channel_mask": 3,
        "compression": "zstd",
    }
    assert mapping["taxonomy"] == {
        "content": "action_role_playing",
        "view": "first_person",
        "material": "3d",
    }
    assert "build_20383525" in device["evidence_class"]


# The pair that passed the 2026-09-18 four-tier certification. Its hashes are part of the
# certification key: any byte change voids that certificate and needs a new four-tier run.
CYBERPUNK_2077_DEPTH_PAIR = {
    "depth_profiles": (
        "depth-profile.json",
        "sha256:fa557a62a79c449ed138df7f2bbc56674c13482eea5db6e2608a7b0e9a42d151",
    ),
    "camera_bindings": (
        "camera-binding.json",
        "sha256:edc26919102745259372c2089e6f48b72bfb97d239e8f3cf53a185ed43374b6c",
    ),
}


def test_cyberpunk_2077_formal_depth_pair_is_the_certified_bytes():
    directory = CATALOGS / "cyberpunk-2077-steam"
    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))
    for key, (filename, certified_hash) in CYBERPUNK_2077_DEPTH_PAIR.items():
        assert len(catalog[key]) == 1
        actual = "sha256:" + hashlib.sha256((directory / filename).read_bytes()).hexdigest()
        assert catalog[key][0]["content_hash"] == actual == certified_hash
    # The separate diagnostic manifest is gone; the profile's evidence stays where it points.
    assert not (REPO / "DataMiner/config/cp2077-local-depth/catalog.json").exists()
    profile = json.loads((directory / "depth-profile.json").read_text(encoding="utf-8"))
    evidence = REPO / "DataMiner" / profile["metric_scale"]["evidence_path"]
    assert profile["metric_scale"]["evidence_sha256"] == (
        "sha256:" + hashlib.sha256(evidence.read_bytes()).hexdigest()
    )
    plan = json.loads((CATALOGS.parent / "release-plan.json").read_text(encoding="utf-8"))
    entry = next(entry for entry in plan["releases"] if entry["gameId"] == "cyberpunk-2077-steam")
    released = {payload["kind"]: payload["contentHash"] for payload in entry["payloads"]}
    assert released["depth_profile"] == CYBERPUNK_2077_DEPTH_PAIR["depth_profiles"][1]
    assert released["camera_binding"] == CYBERPUNK_2077_DEPTH_PAIR["camera_bindings"][1]


def test_fallout4_discovery_is_name_only_while_depth_and_capture_stay_pinned():
    directory = CATALOGS / "fallout4-steam-release-test"
    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))
    game_profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    recording = json.loads(
        (directory / "recording-profile.json").read_text(encoding="utf-8")
    )
    device = json.loads(
        (directory / "device-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))
    depth_profile = json.loads(
        (directory / "depth-profile.json").read_text(encoding="utf-8")
    )
    camera_binding = json.loads(
        (directory / "camera-binding.json").read_text(encoding="utf-8")
    )

    assert catalog["catalog_version"] == "2026-08-25.0"
    assert depth_profile["readback_slots"] == 6
    assert game_profile["version"] == "6"
    assert game_profile["window_class"] is None
    assert game_profile["executable_sha256"] is None
    expected_depth_hash = (
        "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61"
    )
    assert depth_profile["executable_sha256"] == expected_depth_hash
    assert camera_binding["executable_sha256"] == expected_depth_hash
    assert mapping["version"] == "2026-08-25.0"
    assert 120 not in game_profile["input_contract"]["keyboard_vks"]
    assert 86 not in game_profile["input_contract"]["keyboard_vks"]
    assert recording["version"] == "5"
    assert (recording["width"], recording["height"]) == (1920, 1080)
    assert recording["window_mode"] == "borderless"
    assert recording["segment_seconds"] == 60
    assert recording["test_segments"] == 1
    assert device["version"] == "6"
    assert "native_1920x1080_borderless_fullscreen" in device["evidence_class"]
    assert "fallout4_window_class_verified" in device["evidence_class"]
    scans = {
        binding.get("scan_code"): binding["semantic"]
        for binding in mapping["bindings"]
        if binding["device"] == "keyboard"
    }
    assert scans[16] == "combat.vats"
    assert scans[15] == "ui.pipboy_or_flashlight"
    assert scans[37] == "ui.quick_stats"
    assert scans[23] == "ui.quick_inventory"
    assert scans[36] == "ui.quick_data"
    assert scans[50] == "ui.quick_map"
    assert scans[24] == "ui.quick_radio"
    assert scans[63] == "system.quicksave"
    assert 67 not in scans
    assert 47 not in scans


# ── 2026-08-20 批次：十四款本机取证 PC catalog 的共同参数与投影可达性 ──────────

LOCAL_PC_BATCH = [
    "deathloop-pc",
    "atomic-heart-pc",
    "doom-eternal-pc",
    "resident-evil-village-pc",
    "avatar-frontiers-of-pandora-pc",
    "dying-light-pc",
    "far-cry-5-pc",
    "forza-horizon-5-pc",
    "gta5-legacy-pc",
    "indiana-jones-great-circle-pc",
    "titanfall-2-pc",
    "prey-2017-pc",
    "red-dead-redemption-2-pc",
    "starfield-pc",
]

# catalog.rs 的 SET1_TO_VK / SET1_EXTENDED_TO_VK（scan -> VK）。
# 上线代码用它做加载期 fail-closed 守卫；这里复刻，好让 CI 在没有 Windows
# 编译环境时也能挡住同一类错误。
SET1_TO_VK = {
    1: 27, 2: 49, 3: 50, 4: 51, 5: 52, 6: 53, 7: 54, 8: 55, 9: 56, 10: 57,
    11: 48, 12: 189, 13: 187, 14: 8, 15: 9, 16: 81, 17: 87, 18: 69, 19: 82,
    20: 84, 21: 89, 22: 85, 23: 73, 24: 79, 25: 80, 26: 219, 27: 221, 28: 13,
    29: 17, 30: 65, 31: 83, 32: 68, 33: 70, 34: 71, 35: 72, 36: 74, 37: 75,
    38: 76, 39: 186, 40: 222, 41: 192, 42: 16, 43: 220, 44: 90, 45: 88, 46: 67,
    47: 86, 48: 66, 49: 78, 50: 77, 51: 188, 52: 190, 53: 191, 54: 16, 55: 106,
    56: 18, 57: 32, 58: 20, 59: 112, 60: 113, 61: 114, 62: 115, 63: 116,
    64: 117, 65: 118, 66: 119, 67: 120, 68: 121, 87: 122, 88: 123,
}
SET1_EXTENDED_TO_VK = {
    28: 13, 29: 17, 53: 111, 56: 18, 71: 36, 72: 38, 73: 33, 75: 37, 77: 39,
    79: 35, 80: 40, 81: 34, 82: 45, 83: 46,
}


@pytest.mark.parametrize("game", LOCAL_PC_BATCH)
def test_local_pc_batch_shares_one_recording_recipe(game):
    """一批取证入册的游戏必须共用同一套录制参数（owner 定的"老规矩"）。

    1920x1080 / 30fps / 带音频 / 不要求全屏或窗口 / 不带 depth。
    这几个数散在十四个 bundle 里，改错一个不会有任何别的东西报错——
    直到数据落到训练集里才看得出帧率不一致。
    """
    directory = CATALOGS / game
    recording = json.loads(
        (directory / "recording-profile.json").read_text(encoding="utf-8")
    )
    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))

    assert (recording["width"], recording["height"]) == (1920, 1080)
    assert (recording["fps_num"], recording["fps_den"]) == (30, 1)
    assert recording["gop_frames"] == 120  # round(fps * 4s)
    assert recording["window_mode"] == "any"
    assert recording["hdr"] is False
    assert recording["audio"] == {
        "mode": "process_loopback",
        "sample_rate_hz": 48000,
        "channels": 2,
        "bits_per_sample": 16,
        "sample_format": "pcm_s16le",
        "channel_mask": 3,
        "compression": "zstd",
    }
    assert "depth_profiles" not in catalog
    assert "camera_bindings" not in catalog


@pytest.mark.parametrize("game", LOCAL_PC_BATCH)
def test_local_pc_batch_bindings_reach_frames(game):
    """每条键盘绑定都必须真能走到帧上。

    键与标签之间隔着两道独立的闸：Raw Input 丢掉 `keyboard_vks` 之外的键，
    投影丢掉 `control_vks` 之内的键。任一边挡住的绑定永远不会产出标签，
    而现场表现和"玩家没按过"一模一样——COD 语料就这么丢了 move.prone
    整整一个会话系列。上线代码里也有这道守卫；这里保留跨平台检查并给出
    精确到游戏、按键和 semantic 的错误，避免等到 Windows 构建才发现。
    """
    directory = CATALOGS / game
    profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )
    mapping = json.loads((directory / "mapping.json").read_text(encoding="utf-8"))
    contract = profile["input_contract"]

    for binding in mapping["bindings"]:
        if binding.get("device") != "keyboard":
            continue
        scan, extended = binding["scan_code"], binding["extended"]
        table = SET1_EXTENDED_TO_VK if extended else SET1_TO_VK
        vk = table.get(scan)
        assert vk is not None, (
            f"{game}: scan {scan} extended={extended} ({binding['semantic']}) "
            "在上线反查表里查不到，守卫会静默跳过它"
        )
        assert vk in contract["keyboard_vks"], (
            f"{game}: {binding['semantic']} 绑在 VK {vk}，但合同没声明，Raw Input 会丢掉"
        )
        assert vk not in contract["control_vks"], (
            f"{game}: {binding['semantic']} 绑在 VK {vk}，它在 control_vks 里，投影会跳过"
        )

    buttons = {b["button"] for b in mapping["bindings"] if "button" in b}
    assert buttons <= set(contract["mouse_buttons"])
    has_wheel = any(b.get("control") == "wheel" for b in mapping["bindings"])
    assert contract["capture_wheel"] == has_wheel, (
        f"{game}: capture_wheel 与 mapping 是否有滚轮绑定不一致"
    )


def test_gta5_legacy_exact_executable_pin_keeps_other_gates_closed():
    directory = CATALOGS / "gta5-legacy-pc"
    catalog = json.loads((directory / "catalog.json").read_text(encoding="utf-8"))
    profile = json.loads(
        (directory / "game-profile.json").read_text(encoding="utf-8")
    )

    assert catalog["catalog_version"] == "2026-09-02.0"
    assert profile["version"] == "3"
    assert profile["executable_names"] == ["GTA5.exe"]
    assert profile["executable_sha256"] == (
        "sha256:677e4e355cfbdb13273b1d992407e3c261b3a108dc4dd5c8a0c4c1da651802e5"
    )
    assert profile["window_class"] is None
    assert "depth_profiles" not in catalog
    assert "camera_bindings" not in catalog


# 七款游戏的首版只收录默认键位下、无需切换载具/菜单/多人模式即可稳定解释的
# 高频玩法输入。精确数量防止后续改动悄悄扩张范围；下面的前缀、按键和复合
# semantic 则把最容易混入的上下文边界钉死。范围依据见 docs/143。
SEVEN_GAME_V1_BINDING_COUNTS = {
    "avatar-frontiers-of-pandora-pc": 31,
    "dying-light-pc": 26,
    "far-cry-5-pc": 37,
    "forza-horizon-5-pc": 15,
    "gta5-legacy-pc": 27,
    "indiana-jones-great-circle-pc": 23,
    "titanfall-2-pc": 21,
}

SEVEN_GAME_V1_FORBIDDEN_PREFIXES = {
    "avatar-frontiers-of-pandora-pc": ("ui.", "vehicle.", "communication."),
    "dying-light-pc": ("vehicle.", "communication."),
    "far-cry-5-pc": ("ui.", "vehicle.", "communication."),
    "forza-horizon-5-pc": ("ui.", "vehicle.", "multiplayer."),
    "gta5-legacy-pc": ("ui.", "vehicle.", "communication."),
    "indiana-jones-great-circle-pc": ("ui.", "vehicle.", "communication."),
    "titanfall-2-pc": (
        "ui.",
        "vehicle.",
        "communication.",
        "ability.titan_",
    ),
}

SEVEN_GAME_V1_FORBIDDEN_SEMANTICS = {
    "avatar-frontiers-of-pandora-pc": {"move.auto_move"},
    "dying-light-pc": {
        "ui.decline",
        "ui.inventory",
        "ui.journal",
        "ui.map",
        "ui.skills",
    },
    "far-cry-5-pc": set(),
    "forza-horizon-5-pc": {
        "communication.quick_chat",
        "communication.text_chat",
    },
    "gta5-legacy-pc": set(),
    "indiana-jones-great-circle-pc": set(),
    "titanfall-2-pc": {"ability.eject"},
}

SEVEN_GAME_V1_FORBIDDEN_SCANS = {
    # F 同时是步战上下文的“上车”；不收录比误标成通用 interact 更安全。
    "gta5-legacy-pc": {33},
    # G/X/V/F1 来自泰坦或多人上下文，不属于首版单人 Pilot 范围。
    "titanfall-2-pc": {34, 45, 47, 59},
}

SEVEN_GAME_V1_CONTEXTUAL_SEMANTICS = {
    "avatar-frontiers-of-pandora-pc": {
        ("keyboard", 19, False): "combat.reload_or_cancel_shot",
        ("keyboard", 48, False): "target.mark_or_social_wheel",
    },
    "dying-light-pc": {
        ("keyboard", 19, False): "combat.reload_or_repair",
        ("keyboard", 47, False): "interact.secondary_use",
    },
    "far-cry-5-pc": {
        ("keyboard", 19, False): "combat.reload_or_cancel_or_block",
        ("keyboard", 33, False): "combat.melee_or_takedown_or_move_body",
        ("keyboard", 42, False): "move.sprint_or_hold_breath",
    },
    "gta5-legacy-pc": {
        ("keyboard", 16, False): "combat.take_cover_or_heavy_melee",
        ("keyboard", 19, False): "combat.reload_or_light_melee",
        ("keyboard", 57, False): "move.jump_or_combat_dodge",
    },
    "indiana-jones-great-circle-pc": {
        ("keyboard", 18, False): "interact.use_or_pickup_or_exit",
        ("mouse_button", 1, False): "combat.primary_attack_or_throw_or_reload",
        ("mouse_button", 2, False): "combat.aim_or_prepare_throw",
    },
}


def _binding_physical_key(binding):
    if binding["device"] == "keyboard":
        return ("keyboard", binding["scan_code"], binding["extended"])
    if "button" in binding:
        return ("mouse_button", binding["button"], False)
    return ("mouse_control", binding["control"], False)


@pytest.mark.parametrize("game", SEVEN_GAME_V1_BINDING_COUNTS)
def test_seven_game_v1_mapping_scope_is_narrow_and_explicit(game):
    mapping = json.loads(
        (CATALOGS / game / "mapping.json").read_text(encoding="utf-8")
    )
    bindings = mapping["bindings"]
    semantics = {binding["semantic"] for binding in bindings}

    assert len(bindings) == SEVEN_GAME_V1_BINDING_COUNTS[game]
    assert not (semantics & SEVEN_GAME_V1_FORBIDDEN_SEMANTICS[game])
    for semantic in semantics:
        assert not semantic.startswith(SEVEN_GAME_V1_FORBIDDEN_PREFIXES[game]), (
            f"{game}: {semantic} 超出七款游戏首版 mapping 范围"
        )

    keyboard_scans = {
        binding["scan_code"]
        for binding in bindings
        if binding["device"] == "keyboard"
    }
    assert not (
        keyboard_scans & SEVEN_GAME_V1_FORBIDDEN_SCANS.get(game, set())
    ), f"{game}: 混入了首版明确排除的上下文按键"

    by_physical_key = {_binding_physical_key(binding): binding for binding in bindings}
    for physical_key, expected_semantic in SEVEN_GAME_V1_CONTEXTUAL_SEMANTICS.get(
        game, {}
    ).items():
        assert by_physical_key[physical_key]["semantic"] == expected_semantic

    if game == "forza-horizon-5-pc":
        profile = json.loads(
            (CATALOGS / game / "game-profile.json").read_text(encoding="utf-8")
        )
        assert profile["input_contract"]["capture_mouse_move"] is False
        assert profile["input_contract"]["capture_wheel"] is False
