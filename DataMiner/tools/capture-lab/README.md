# P0 Native Capture Lab

This is a Windows-only, non-trainable validation harness. It creates a synthetic
Win32 client area, binds WGC to that HWND, performs GPU-only copy/crop/NV12
conversion, and writes hardware-only NVIDIA H.264 segments through Media
Foundation. It is not a production certification result.

From `DataMiner`:

```powershell
# Fast local check (one 2-second segment)
cargo run -p capture-lab -- --smoke

# CP2077/#735 D3D12 feasibility: headless D32 clear/barrier/copy/fence smoke.
# This does not launch or instrument a game process.
cargo run -p capture-lab --bin d3d12-depth-smoke -- --iterations 3

# Optional WARP + Windows Graphics Tools debug layer/DRED run.
cargo run -p capture-lab --bin d3d12-depth-smoke -- --warp --debug

# Stage-1 process-loopback probe (audible tone; still non-trainable)
cargo run -p capture-lab -- --smoke --audio-loopback

# Five independent 60-second calibration runs + one cohort-level verdict
.\tools\run-audio-cross-run.ps1 -RequiredBuild 26200

# Audio-only target-process health probe. This is the 22H2/COD Alt+Tab tool;
# perform the documented fullscreen actions manually while it runs.
cargo run --release -p capture-lab --bin process-audio-probe -- `
  --pid <cod-pid> --seconds 600 --out tools/capture-lab/out/cod-22h2-probe.json

# Required P0 endurance run (profile defaults: 30 x 10 seconds)
cargo run --release -p capture-lab

# H-1 1080p60 RAW experiment (60 x 10 seconds, always non-trainable)
cargo run --release -p capture-lab -- --profile config/capture-profiles-60fps-h1.json

# Fast H-1 segment-boundary check (2 x 2 seconds)
cargo run --release -p capture-lab -- --profile config/capture-profiles-60fps-h1.json --segments 2 --seconds 2

# Verify prohibited release entry points/dependencies are absent
.\tools\verify-release-surface.ps1 -RequireReleaseBinary
```

Reports and immutable per-segment artifacts are written under
`tools/capture-lab/out/` and intentionally ignored by Git. A run contains
`report.json`, `report.md`, plus `video.mp4`, `events.jsonl.zst`,
`frames.jsonl.zst`, `meta.json`, and `sha256.json` for every segment.

`--audio-loopback` is an explicit, short (2–60 seconds) laboratory path. It
captures only the `capture-lab` process tree as PCM s16le/48 kHz/stereo, plays a
known 1 kHz tone-burst sequence, flashes the synthetic scene from the same QPC
schedule, and adds an `audio_calibration` block to `report.json`. Raw probe bytes
are written to `audio-calibration.pcm.zst` at the run root; they are not segment
artifacts and are never consumed by the release client, uploader, converter, or
dataset tooling.

A single run can report constant offset and residual p50/p95/max, but it cannot
pass the docs/56 hard gate by itself. `run-audio-cross-run.ps1` requires at
least five reports from one OS/GPU/driver tuple and applies one cohort-median
offset to every run. Its conservative p95/max bounds must remain within one/two
video frames. This verdict covers timing stability only; the separate
`process-audio-probe` evidence is still required for 22H2 fullscreen Alt+Tab,
device changes, and the original COD machine.

For an explicit audio probe, the top-level `passed` value and process exit code
also require the synthetic game-performance limits to pass. A timing-clean run
that exceeds the average-FPS or 1%-low limit therefore fails instead of being
reported as an audio success.

The H-1 profile is an evidence-gathering profile, not a release profile. Its
requested bitrate and GOP intentionally have no accepted driver-relaxation
envelope: the report must preserve the effective NVIDIA Media Foundation
settings so a later per-game profile can freeze only values observed and
accepted on the target machine. A successful synthetic run does not authorize a
real game or make its artifacts trainable.
