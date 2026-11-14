import json
import sys
from pathlib import Path


MODULE_ROOT = Path(__file__).resolve().parents[1]
if str(MODULE_ROOT) not in sys.path:
    sys.path.insert(0, str(MODULE_ROOT))


def manifest_dict(*, calibrated=True):
    return {
        "schema": "dm-gta-dependency-manifest/1",
        "game": {"path": "missing/GTA5.exe", "sha256": "",
                 "expected_build": "1.0.3889.0", "process_name": "GTA5.exe"},
        "gpu": {"vendor": "NVIDIA", "adapter_name": "NVIDIA Test Adapter",
                "driver_version": "32.0.15.9000"},
        "components": {
            name: {"path": f"missing/{name}", "sha256": ""}
            for name in ("scripthook_dll", "asi_loader", "replus_asi",
                         "replus_capture_addon", "reshade_dxgi", "replus_render_ini",
                         "replus_main_ini", "replus_lights_ini", "replus_settings_exe",
                         "igcs_dof_shader", "reshade_install_receipt",
                         "capture_build_receipt", "diogenes_controller_asi",
                         "gta_settings_xml", "ffmpeg", "ffprobe")
        },
        "source_pins": {
            "scripthook_release": "v3889.0 / 1158.13",
            "scripthook_runtime_zip_sha256":
                "b64c97c3353906f14621e7e9511e4aec2a7d436ecc21ed124d3816585e2e6188",
            "scripthook_sdk_zip_sha256":
                "56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22",
            "replus_commit": "c6eba57160ca444817b149b3304ed1454e0ba2d9",
            "replus_patch_sha256":
                "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa",
            "igcs_fork_commit": "13c92ef2565403ffcc2501f779ca7cd935e5d598",
            "igcs_patch_sha256":
                "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5",
            "reshade_release": "6.8.0 full add-on support",
            "reshade_setup_sha256":
                "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445",
            "reshade_signer_thumbprint": "589690208A5E52FB96980C4A6698F50ACD47C49F",
            "reshade_dll_sha256": "",
            "controller_git_commit": "1" * 40,
        },
        "render": {"fps": 30, "graphics_api": "DX11",
                   "frame_step_mode": "fixed_replay_timeline",
                   "render_samples": 1, "motion_blur": False,
                   "width": 1920, "height": 1080},
        "calibration": {"calibration_id": "test-calibration-v1",
                        "filmback_id": "test-36mm", "metric_scale": 1.0,
                        "metric_scale_approved": calibrated,
                        "acceptance": {"max_reprojection_error_px": 1.0,
                                       "min_sample_count": 100},
                        "projection_evidence": {"path": "missing/projection.json",
                                                "sha256": ""}},
        "policy": {
            "purpose": "internal_nontrainable_poc",
            "story_mode_only": True,
            "online_forbidden": True,
            "non_trainable": True,
            "upload_enabled": False,
            "customer_replay_sync_accepted": False,
            "customer_metadata_semantics_accepted": False,
            "commercial_rights_approved": False,
        },
    }


def scenario_dict(duration=1.0):
    return {
        "schema": "dm-gta-scenario/1",
        "scenario_id": "test-poc",
        "mode": "story_mode",
        "seed": 1,
        "location": [0.0, 0.0, 0.0],
        "radio": "off",
        "weather": "CLEAR",
        "clock": "14:30",
        "source_epoch": "2026-08-25 00:00:00.000",
        "player": {"model": "current_story_player",
                   "control": "native_task_go_straight",
                   "route_kind": "forward_from_anchor", "distance_m": 5.0,
                   "speed_mps": 1.0, "heading_deg": 0.0},
        "dynamics": [{"id": "player", "type": "humanoid",
                      "control": "native_task_go_straight", "route": "player"}],
        "replay": {"duration_s": duration, "warmup_s": 0.0, "fps": 30},
        "effect_exclusions": ["radio", "online", "loading_transition",
                              "random_peds", "random_vehicles", "police_dispatch"],
    }


def write_json(path: Path, value) -> Path:
    path.write_text(json.dumps(value), encoding="utf-8")
    return path
