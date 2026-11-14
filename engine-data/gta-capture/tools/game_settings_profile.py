"""Switches the game's graphics settings between the recording profile and the export profile.

    python tools/game_settings_profile.py record            # before recording: the picture is not recorded, keep it light
    python tools/game_settings_profile.py export            # before exporting: everything on, the picture is rendered now
    python tools/game_settings_profile.py export-8gb        # a candidate export profile for 8 GB cards (see below)
    python tools/game_settings_profile.py show              # what the file says, against the profiles

Why two profiles: a replay stores where everybody is and what they do, not pixels -- the picture is rendered
at export, under whatever settings the game has then. So recording needs no picture quality at all (lighter
settings are headroom; the frames take AB of beach-demo-003 dropped were the program's key events, not the
GPU, which idles while recording), and export wants anti-aliasing, supersampling and full draw distance
(the customer, 2026-09-30: jagged edges, shimmering shadows, detail popping). Neither profile touches resolution (1920x1080, what systeminfo says), Post FX
(High: higher adds depth of field), motion blur (0) or population density (what spawns is part of the take).

The game reads settings.xml at start-up: close it, switch, start it again. The file is backed up first.
"""
import argparse
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

# Recording: no anti-aliasing or supersampling (the replay stores positions and animations, not pixels), the
# rest a notch down. Measured 2026-09-30: the GPU idles while recording (55 C, a third of its clock), so the
# picture settings were never what dropped frames; the settings below are headroom, not a fix.
RECORD = {
    "MSAA": "0", "TXAA_Enabled": "false", "SamplingMode": "0", "MaxLodScale": "0.000000",
    "TextureQuality": "1", "ShadowQuality": "2", "UltraShadows_Enabled": "false", "Shadow_SoftShadows": "1",
    "ReflectionQuality": "1", "ReflectionMSAA": "0", "SSAO": "0", "ParticleQuality": "1", "WaterQuality": "1",
}
EXPORT = {
    "MSAA": "4", "TXAA_Enabled": "true", "SamplingMode": "8", "MaxLodScale": "1.000000",
    "TextureQuality": "2", "ShadowQuality": "3", "UltraShadows_Enabled": "true", "Shadow_SoftShadows": "3",
    "ReflectionQuality": "2", "ReflectionMSAA": "4", "SSAO": "2", "ParticleQuality": "2", "WaterQuality": "2",
}
# A CANDIDATE, not yet compared on the delivery's frames (2026-10-13). On the RTX 4060 8 GB the export profile
# stuttered and its video was turned down; 3840x2160 with 4x MSAA (and 4x on reflections) is where an 8 GB card
# runs out of memory, and short of memory the game drops textures and switches LODs early -- what the per-frame
# fence cannot see. This keeps the supersampling (x2: what calms the shadow shimmer and the edges the customer
# saw), the shadows, the extended distance and the textures, and drops MSAA -- and TXAA with it, which needs MSAA
# -- for FXAA. Before a group is exported with it: export lane 1 (the fixed camera) of one take with both
# profiles and compare the same frames (frame-to-frame flicker of still areas, the edge-stair energy PR #962
# measured, a look at the far railings and the lifeguard tower), and run tools/export_timing.py with
# scripts/export-gpu-log.ps1 on both. A group's 12 lanes are exported with one profile (game_settings.picture).
EXPORT_8GB = dict(EXPORT, MSAA="0", TXAA_Enabled="false", ReflectionMSAA="0", FXAA_Enabled="true")
EXPORT = dict(EXPORT, FXAA_Enabled="false")
RECORD = dict(RECORD, FXAA_Enabled="false")
# never changed by either profile, and read back so a wrong file is noticed
KEEP = {"ScreenWidth": "1920", "ScreenHeight": "1080", "PostFX": "1", "MotionBlurStrength": "0.000000", "LodScale": "1.000000"}
PROFILES = {"record": RECORD, "export": EXPORT, "export-8gb": EXPORT_8GB}


def default_path() -> Path:
    return Path.home() / "Documents" / "Rockstar Games" / "GTA V" / "settings.xml"


def value_of(text: str, tag: str):
    found = re.findall(rf'<{tag} value="([^"]*)"', text)
    return found[0] if len(found) == 1 else None


def game_running() -> bool:
    if sys.platform != "win32":
        return False
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq GTA5.exe"], capture_output=True, text=True).stdout
    return "GTA5.exe" in out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("profile", choices=["record", "export", "export-8gb", "show"])
    ap.add_argument("--settings", type=Path, default=None, help="the game's settings.xml (default: this user's)")
    args = ap.parse_args(argv)
    path = args.settings or default_path()
    if not path.is_file():
        print(f"settings.xml not found: {path}", file=sys.stderr)
        return 2
    text = path.read_text(encoding="utf-8")
    if args.profile == "show":
        for tag in list(EXPORT_8GB) + list(KEEP):
            v = value_of(text, tag)
            which = " ".join(name for name, prof in PROFILES.items() if prof.get(tag) == v) or ("kept" if KEEP.get(tag) == v else "neither")
            print(f"  {tag:22s} = {v!s:10s} {which}")
        return 0
    if game_running():
        print("GTA5.exe is running: close the game first (it reads settings.xml only at start-up)", file=sys.stderr)
        return 3
    wrong = {tag: value_of(text, tag) for tag in KEEP if value_of(text, tag) != KEEP[tag]}
    if wrong:
        print(f"settings.xml differs from what every profile keeps: {wrong}; not touching it", file=sys.stderr)
        return 4
    backup = path.with_name(path.name + ".before-" + args.profile + "-" + time.strftime("%Y%m%d-%H%M%S"))
    shutil.copyfile(path, backup)
    for tag, value in PROFILES[args.profile].items():
        pat = re.compile(rf'(<{tag} value=")([^"]*)(" ?/>)')
        found = pat.findall(text)
        if len(found) != 1:
            print(f"{tag}: found {len(found)} times in settings.xml; the file is left as it was (backup {backup})", file=sys.stderr)
            return 5
        if found[0][1] != value:
            print(f"  {tag}: {found[0][1]} -> {value}")
        text = pat.sub(lambda m: m.group(1) + value + m.group(3), text)
    path.write_text(text, encoding="utf-8", newline="")
    print(f"{args.profile} profile written; backup {backup}; start the game now")
    return 0


if __name__ == "__main__":
    sys.exit(main())
