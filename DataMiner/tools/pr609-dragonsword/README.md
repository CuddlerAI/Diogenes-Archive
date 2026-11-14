# DragonSword: Awakening depth and camera certification

This directory pins the offline single-player D3D11 capture path for the Steam build of
`DragonSword: Awakening` (`1.0.8`, AppID `4570720`, build `24597321`). It is a
research-only, non-trainable integration. Do not use it with online services, multiplayer,
an active anti-cheat component, or a different executable hash.

## Pinned build and scene

- executable: `DSClient-Win64-Shipping.exe`
- executable SHA-256: `0c9d54a35d7160e671a3db10c5e645140fdad15a7293314681e9eda7e360c1fe`
- launch mode: Steam with explicit `-dx11`
- audited scene: opening lakeside gameplay with the movement tutorial HUD visible
- source depth: 1664x936, `R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`
- delivered depth: 1280x720 EXR, reverse-Z

The Unreal view uniform is bound at vertex slot 0 on pass 1, draw 0. The binding reads
the row-major view rotation at byte 192, world position at byte 896, and `ViewToClip` at
byte 448. The observed reverse-infinite projection is `A=0`, `B=10`; combined with the
engine's centimetre convention (`metric_scale=0.01`), this is a 0.1 metre near plane.

## Reproduce a bounded local run

1. Build `dm-depth-probe` in release mode.
2. Launch AppID 4570720 offline with `-dx11`, choose **Continue**, and wait until the
   lakeside gameplay scene and movement HUD are visible.
3. Enumerate modules, services, and drivers. Continue only when no anti-cheat module,
   service, or driver is loaded or running.
4. Inject the release probe into `DSClient-Win64-Shipping.exe`.
5. Run:

   ```powershell
   tools\pr609-dragonsword\Invoke-DragonSwordDepthCameraCertification.ps1 `
     -ProcessId $process.Id `
     -Generation 1 `
     -OutputDir C:\dm\dragonsword-certification
   ```

The runner verifies the pinned executable and `-dx11` command line, re-runs the complete
anti-cheat runtime gate, waits for `Ready`, captures the requested depth frame count, and
seals the run at `Flushed`. It rejects uniform depth, projection drift, stale projection
evidence, a missing same-present camera row, a missing operator exact pin, camera
diagnostics, count mismatches, or writer errors. It also requires at least 1,000 raw and
500 stored sampled depth values in every frame. Those exact-build thresholds sit well
below the certified gameplay minima (2,499/1,052) but reject the static startup sphere
(164/103), which is non-uniform yet is not a main gameplay scene. Probe faults are
surfaced immediately and a bounded capture timeout requests and waits for a clean
stop/flush.

## Certified result and remaining boundary

The sealed run `dragonsword-depth-camera-cert-20260810T221046Z` produced:

- 60 depth rows and 60 EXR files; all raw and stored frames are finite and non-uniform;
- 96 camera rows, zero camera diagnostics, and same-present camera coverage for all 60
  depth rows;
- exact-draw projection evidence (`A=0`, `B=10`) on every depth row;
- queue depth 0, final present equal to cutoff present, and no writer errors.

Depth and camera are ready for the research-only lane. Full pose certification remains
blocked because this build has no audited player-root provider; the sealed run correctly
emitted zero player rows. No screen-space or camera-relative approximation is substituted.

See `evidence/dragonsword-depth-camera-certification-20260811.json` and
`evidence/dragonsword-runtime-gate-20260811.json`. Raw EXRs, discovery reports, and full
pose streams remain local and are intentionally not committed.

The rejected 60-second startup-screen run is summarized in
`evidence/dragonsword-static-screen-rejection-20260811.json`. It is retained specifically
to prevent a long, sealed, non-uniform run from being mistaken for gameplay certification.
