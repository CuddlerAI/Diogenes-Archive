# A Plague Tale: Innocence depth and camera certification

This directory pins the offline single-player capture path for the Steam build of
`A Plague Tale: Innocence` (`1.0.7`, AppID `752590`). It is research-only and
non-trainable. Do not use it with multiplayer, an active anti-cheat service/driver/module,
or a different executable hash.

## Pinned build and scene

- executable: `APlagueTaleInnocence_x64.exe`
- executable SHA-256: `fb5f31c3bd07f89142281fa8469fc4db7811e94992c7a46fdc14e0f5af07b329`
- graphics API: D3D11
- audited scene: chapter XVII, `为了彼此`
- render depth identity: 2560x1440, `R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`
- delivered depth: 1280x720 EXR, reverse-Z

The engine stores the main camera as a left-handed world-to-camera rotation plus a
separate world position in one constant buffer. `camera-binding/v1` records that layout
explicitly and resolves it to the canonical right-handed view matrix before emitting a
quaternion. Projection and view data are read at the same pass/draw window.

## Reproduce a bounded local run

1. Build `dm-depth-probe` in release mode and launch the pinned build offline.
2. Select the pinned save, load chapter XVII, and wait for live gameplay.
3. Re-run the runtime gate. Continue only when module/service/driver enumeration is
   complete and no anti-cheat component is loaded or running.
4. Inject the release probe into the live process.
5. Run:

   ```powershell
   tools\pr609-a-plague-tale\Invoke-APlagueTaleDepthCameraCertification.ps1 `
     -ProcessId $process.Id `
     -Generation 1 `
     -OutputDir C:\dm\a-plague-tale-certification
   ```

The runner waits for `Ready`, exactly 60 depth frames, and the final `Flushed` barrier.
It rejects uniform depth, stale projection evidence, a missing same-present camera row,
camera diagnostics, count mismatches, or writer errors.

## Certified result and remaining boundary

The sealed run `a-plague-tale-production-lh-packed-camera-20260811T065500Z` produced:

- 60 depth rows and 60 EXR files; every raw and stored frame is finite and non-uniform;
- 1,719 camera rows, zero camera diagnostics, and same-present camera coverage for all
  60 depth rows;
- projection A/B observed from the pinned constant buffer on every depth frame;
- queue depth 0, final present equal to cutoff present, and no writer errors.

Depth and camera are therefore ready for the research-only lane. Full pose certification
remains blocked because this build has no audited player-root provider: the sealed run
correctly emitted zero player rows. No guessed screen-space or camera-relative player pose
is substituted.

See `evidence/a-plague-tale-depth-camera-certification-20260811.json` and
`evidence/a-plague-tale-runtime-gate-20260810.json`. Raw EXRs and full pose streams remain
local and are intentionally not committed.
