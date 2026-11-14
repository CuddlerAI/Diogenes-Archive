# Bannerlord campaign-map depth certification

This directory pins and certifies the offline single-player campaign-map capture path for
Mount & Blade II: Bannerlord `v1.4.7.117484` (Steam build `24127665`). It is research-only:
do not use it in multiplayer or while an anti-cheat service, driver, or module is active.

## Pinned build

- executable: `Bannerlord.exe`
- executable SHA-256: `5fc59a9da65474a707fd3e868a24c6f0a782e9364edd4d17a8f8813171237de0`
- graphics API: D3D11
- render size: 1920×1200; delivered depth: 1280×720
- main scene DSV formats: `R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`
- player source: the included single-player campaign module, exposed through
  `Local\DataMinerBannerlordTelemetryV1`

The profile uses an exact DSV identity and format pin. The observed candidates have a low
heuristic score margin, so production is deliverable only when that operator pin is present;
any MSAA observation still fails closed.

## One complete local run

1. Build the probe from `DataMiner`:

   ```powershell
   cargo build -p dm-depth-probe --release
   ```

2. Build and install the telemetry module:

   ```powershell
   tools\pr609-bannerlord\Build-And-Install-Telemetry.ps1
   ```

3. Launch Bannerlord through Steam, enable only the single-player telemetry module, load a
   campaign save, and click normal speed so campaign ticks are live. Re-run the runtime gate;
   continue only if module/service/driver enumeration is complete and no anti-cheat component
   is running or loaded.

4. Inject the release probe into the running single-player process:

   ```powershell
   $process = Get-Process TaleWorlds.MountAndBlade.Launcher
   $dll = (Resolve-Path target\release\dm_depth_probe.dll).Path
   target\release\dm-depth-inject.exe --pid $process.Id --dll $dll
   ```

5. Start and seal a bounded run. `Generation` must be greater than every generation already
   consumed by the injected process:

   ```powershell
   tools\pr609-bannerlord\Invoke-BannerlordCertification.ps1 `
     -ProcessId $process.Id `
     -Generation 1 `
     -OutputDir C:\dm\bannerlord-certification
   ```

The runner waits for the `Ready` acknowledgement, 60 indexed depth frames, and the final
`Stopped`/`Flushed` barrier. It rejects a non-empty writer queue, count mismatch, writer error,
camera diagnostic, or a depth frame without same-`present_seq` camera and player rows.

## Certified result

The committed evidence summarizes the sealed run
`bannerlord-production-camera-fix-20260811T030000Z`:

- 60 depth rows and 60 EXR files; all 60 raw and stored frames are non-uniform;
- raw sampled uniqueness 8058–8062; stored sampled uniqueness 1825–1830;
- projection A/B observed from the bound constant buffer on all 60 depth frames;
- 28,827 camera rows, 28,827 player rows, and zero camera diagnostics;
- all 60 depth `present_seq` values are present in both pose streams;
- flushed queue depth 0, final present equals cutoff present, and no writer errors.

The first corrected camera sample is `(236.1057, 445.3984, 42.20235)`. This is also a
regression fixture for the row-vector `Row3` view-matrix convention.

See `evidence/bannerlord-depth-certification-20260811.json` and
`evidence/bannerlord-runtime-gate-20260810.json` for the machine-readable record. Raw EXRs and
full pose streams remain local and are intentionally not committed.
