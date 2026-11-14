# BioShock Infinite depth adaptation audit (issue #609)

This directory records the exact-build discovery result for Steam AppID 8870. The audited build
can capture the main-scene D24S8 depth target reliably, but it is **not adapted for certified
delivery**: no production camera binding or cross-checked metric scale was established. The
fail-closed result is `blocked`, not a guessed profile.

## Exact build and runtime gate

- executable: `Binaries/Win32/BioShockInfinite.exe`
- SHA-256: `c20a42529c4181b1d10eed416d9847a823f9453dd267ca478efb3a10014a0e77`
- target architecture: i386
- graphics API observed before injection: D3D11 + DXGI
- anti-cheat gate: no loaded anti-cheat module and no running anti-cheat service or driver
- capture resolution: native 2560x1440, one sample, R24G8/D24S8; stored diagnostic depth was
  downsampled to 1280x720

The point-in-time runtime gate and every large local report are identified by SHA-256 in
`evidence/bioshock-blocker-20260810.json`. No game images or proprietary assets are committed.

## What passed

The 900-frame diagnostic run selected `main-scene-v1` on every row. The configured candidate was
also the ranked top DSV on all 900 rows, every ranking was confident, no MSAA target was observed,
and no raw frame was uniform. The raw sampled depth retained 6,738–7,772 distinct values per frame.

The values occupy the high end of the normalized D24 range (observed approximately 0.951–0.999),
which is the expected traditional-Z shape for this outdoor scene. The installed
`DefaultEngine.ini` independently describes 10 game units as the near plane. These facts are
useful discovery evidence, but they are insufficient to publish metric depth without the second
scale/convention cross-check required by the profile contract.

## Why production is blocked

1. **The apparent projection is not the main camera.** Discovery repeatedly found VS slot 0,
   1,280-byte buffer, offset 1,136 at draws 2–9. It stayed byte-for-byte constant across active
   phases and has `m00 == m11 == 1.1904762`. At 2560x1440 that implies non-square pixels
   (`fx/fy = 16/9`), while its `B = 37522.766` contradicts the installed 10-unit near-plane fact.
   It is rejected rather than promoted.
2. **No reproducible view binding passed action causality.** Reliable phase reports contained
   1,435–2,555 view-shaped candidates, mostly animated/skeletal transforms. The only candidate
   isolated by an earlier unfocused-input comparison kept a fixed rotation while only translation
   changed, so it is an object/player transform, not the camera view. The unfocused phases are
   explicitly excluded from certification.
3. **`metric_scale` is unresolved.** Repository research already flags the UE3 2 cm/unit claim as
   needing re-verification against the roughly 1.905 cm/unit UDK convention. The installed
   near-plane comment is one chain, not the two independent chains required by `DepthProfile`.
4. **Raw-buffer EXR is diagnostic only.** All 900 half-float stored frames reported
   `stored_quantization_collapse`; linearization must occur before half storage. With no certified
   `(proj_a, proj_b, metric_scale)`, materializing those files would manufacture a scale.

## Reproduction constraints

- Build `dm-depth-probe` and `dm-depth-inject` for `i686-pc-windows-msvc` from current `main`.
- Launch the game offline, load a main-scene save, and verify the exact executable hash and runtime
  anti-cheat gate before injection.
- Use discovery render identity 2560x1440, sample count 1, formats `r24_g8_typeless` and
  `d24_unorm_s8_uint`, `max_main_passes_per_present=1`, and draw window 0–15.
- Treat every discovery session as non-delivery. A future attempt must obtain a controllable save,
  add raw/combined-matrix correlation if necessary, demonstrate mouse/turn causality, and provide a
  second metric-scale anchor before adding a production binding or depth profile.

The machine-local game configuration was restored after the experiment, and the probe runtime
configuration was left disabled.
