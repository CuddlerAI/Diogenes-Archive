# Halo: Reach MCC depth and camera certification

This directory pins the offline single-player capture path for Halo: Reach in Halo: The Master
Chief Collection `1.3528.0.0` (Steam AppID `976730`, build `19905945`). It is research-only and
non-trainable. Never use this path in matchmaking, multiplayer, or with an anti-cheat component
loaded or running.

## Pinned build and safe launch

- executable: `MCC-Win64-Shipping.exe`
- executable SHA-256: `be70d6dcd1a884f10ceb342a7a2dcb35ee0fa43181b66a1d19c3d830e9834691`
- title module: `haloreach.dll`
- title-module SHA-256: `738dd2d24ea3aea12e1ee9aa4a61094bf116027d42004c35a19e5048608b0894`
- graphics API: D3D11
- source DSV: 2560x1440, `R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`
- delivered depth: 1280x720 EXR, reverse-Z

Launch only the Steam item **Play Halo: MCC Anti-Cheat Disabled (Mods and Limited Services)**.
Halo Support documents that item at
<https://support.halowaypoint.com/hc/en-us/articles/360037475251-How-to-Launch-Halo-The-Master-Chief-Collection-with-Easy-Anti-Cheat-EAC-Disabled>.
The equivalent URI used for this run was `steam://launch/976730/option2`.

The certification runner re-enumerates modules, services, and drivers before writing a control
command. It stops unless D3D11 and the exact Reach module are present and no EAC/ACE module,
running service, or running driver is found. During setup, `steam://run/976730` once reused Steam's
cached normal launch choice; that process was exited immediately, before probe injection or any
capture. No artifact from that launch is part of this evidence.

## Camera binding

Reach stores a combined transform at offset 0 and a packed affine view at offset 64 in the same
144-byte VS slot-0 buffer at pass 0 / draw 2. The packed view omits its homogeneous corner. The
binding therefore reconstructs `V[3][3] = 1` and resolves the canonical projection as
`transpose(C * transpose(V))`. The exact-draw resource may rotate each frame; production registers
and snapshots only the authored source at the authored draw, never an arbitrary buffer.

## Reproduce one bounded run

1. Build `dm-depth-probe` in release mode.
2. Launch MCC using the official anti-cheat-disabled item and load the pinned Reach campaign save.
3. Inject the release probe into `MCC-Win64-Shipping.exe`.
4. From `DataMiner`, run:

   ```powershell
   tools\pr609-halo-reach\Invoke-HaloReachDepthCameraCertification.ps1 `
     -ProcessId $process.Id `
     -Generation 1 `
     -OutputDir C:\dm\halo-reach-certification
   ```

The generation must be newer than every generation consumed by that injected process. The runner
waits for `Ready`, exactly 60 depth frames, and the final `Flushed` barrier. It rejects a dirty
anti-cheat gate, uniform depth, stale projection evidence, missing same-present camera coverage,
camera diagnostics, count mismatches, or writer errors.

## Certified result and remaining boundary

The sealed generation `6090809` produced 60 depth rows and EXRs, 76 camera rows, same-present camera
coverage for all 60 depth rows, zero camera diagnostics, queue depth 0, and no writer errors. Every
raw and stored frame was finite and non-uniform. Projection A/B came from the exact bound buffer on
each depth frame.

Depth and camera are ready for the research-only lane. Player pose remains blocked because this
exact build has no audited player-root provider; zero player rows were emitted, and no guessed
camera-relative pose is substituted. The `3.048` metric scale is supported by the public Blam/JMS
unit convention surveyed in `docs/82-depth-map-bounds-public-data-survey.md`; it has no independent
live-scene anchor, so this package remains non-trainable.

See `evidence/halo-reach-depth-camera-certification-20260811.json` and
`evidence/halo-reach-runtime-gate-20260811.json`. Raw EXRs and full pose streams remain local and
are intentionally not committed.
