# Fallout 4 PR 405 capture tools

This directory contains the reusable Windows operator tools and exact-build evidence used to
prepare the Fallout 4 capture in issue #544. The scripts are intentionally fail-closed: a preview
that does not satisfy the current `depth-index/v6` projection, cadence, health, camera and player
pose checks exits with code 2.

The checked-in binding and depth profile are valid only for Fallout 4 runtime `1.11.221.0` with
executable SHA-256
`428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61`. Do not reuse them for a
different executable. Re-run the discovery procedure in `docs/95-camera-projection-binding-fieldguide.md`
and commit new exact-build evidence instead.

The supported capture scope is first person only. The singular `dm-depth-profile-1` pins the
observed traditional-Z `near=1` projection family and the probe still linearizes every frame from
the same-Present matrix. Third person uses a different `near=15` family and is intentionally not
authorized; switching to it during a run must fail closed with `DEPTH_PROJECTION_MISMATCH`.

Performance certification must be produced from A1/A2 runs that stay in first person and use the
exact final profile, CameraBinding and backend generation. The DLL SHA is still checked and audited,
but another reviewed SHA may reuse the result when its D3D11 generation is unchanged. See
[docs/151](../../docs/151-depth-probe-artifact-and-generation-certification.md).

## Which inputs travel with the game, and which one does not

The game profile and CameraBinding remain exact-build inputs. Performance certification is now a
signed central, generation-scoped input rather than a writable local certificate.

| Input | Scope | On a new machine |
|---|---|---|
| `config/catalogs/dev/fallout4-steam-release-test/depth-profile.json` | **Per game build** — pinned by `executable_sha256` | **Copy as is.** Verify the game exe SHA-256 first |
| `config/catalogs/dev/fallout4-steam-release-test/camera-binding.json` | **Per game build** — same pin | **Copy as is.** Same check |
| signed performance certification manifest | **Per backend generation + explicit GPU model** | Reuse only when the exact DXGI model appears in the reviewed coverage list |

Both game-scoped files describe facts about the *rendered frame*, not about the host: DSV
selection and projection parameters in the profile; constant-buffer slot/offset, main-scene
pass/draw window and camera singleton offsets in the binding. Those are identical wherever the
same executable runs, so **copying them across machines is the correct action** — re-running
discovery only risks introducing an inconsistency.

Four-stage validation writes a candidate containing driver and adapter LUID for audit. That
candidate never opens the gate by itself. A human reviews it, chooses an explicit hardware coverage
list, and publishes the central manifest. Unknown models fail closed; there is no GPU ranking.
Profile or CameraBinding byte changes still invalidate the matching key. Probe SHA changes alone do
not: only the declared D3D11 generation decides whether this Fallout 4 evidence remains reusable.

> Making all three arrive automatically instead of by hand is designed in
> `docs/130-depth-capture-inputs-auto-provisioning.md`.

## What is included

- `Test-Pr405Readiness.ps1`: checks release binaries, 1920x1080 borderless-fullscreen game settings,
  exact executable,
  depth profile, camera binding, matching performance certification, player telemetry and the
  final action-camera assembler.
- `Start-Pr405Preview.ps1`: drives the versioned probe Start/Stop protocol, waits for
  `Started -> Ready -> Stopped -> Flushed`, then validates the bounded preview.
- `Run-Pr405Trajectory.ps1`: optional, explicit operator-side movement trajectory with a JSONL
  audit log. It targets one supplied HWND and is not part of the collector process.
- `Run-Pr405ViewCalibrationSegment.ps1`: explicit mouse-turn segment for view-scale calibration.
- `Invoke-Fallout4CameraEvidence.ps1`: #636 non-deliverable three-phase render-camera evidence
  workflow. It captures same-Present GPU view tracks and the legacy singleton reference, seals
  `POSE_STATIC -> POSE_WALK -> POSE_TURN`, and runs the offline candidate analyzer. It never sends
  game input or authorizes a production CameraBinding.
- `Invoke-Fallout4CameraEvidenceSweep.ps1`: the preferred #636 automatic multi-window workflow.
  The operator performs STATIC, WALK and TURN once each; within each action it automatically seals
  16 bounded 8-draw windows (draw 0..127 by default), uses the small progress sidecar for control,
  and produces one fail-closed offline sweep report. Window budgets are configurable acceptance
  parameters, not hardware truths. `CaptureOne` repairs one explicitly named phase/window without
  forcing a repeat of every already-reliable window. Each independent `CaptureOne` requires a
  freshly restarted Fallout 4 process; the script rejects an already-resident probe before capture
  because the legacy configuration watcher owns one session per process.
- `Invoke-Pr405Focus.ps1`, `Invoke-Pr405Click.ps1`, `Request-Pr405ElevatedClick.ps1` and
  `Invoke-Pr405Hotkey.ps1`: small operator helpers for an elevated game window.
- `evidence/`: the measurements those inputs were derived from. The depth profile and camera
  binding themselves live **only** in the catalog bundle
  (`config/catalogs/dev/fallout4-steam-release-test/{depth-profile,camera-binding}.json`) — the
  scripts here, the client and `depth-lab` all read that one copy, per [130 号 §2.4](../../docs/130-depth-capture-inputs-auto-provisioning.md).
  They used to be duplicated into this directory; that second copy drifted once (PR #754 shipped a
  pre-#739 projection) and has been removed. The camera binding is the E-geo-authorized
  render-hybrid v3: exact-build GameCamera position plus same-draw VS slot 12 render basis and
  projection. Its promotion evidence is
  `evidence/fallout4-render-camera-egeo-20260811.json`.

PresentMon and game files are deliberately not vendored. Supply the authored central manifest,
packaged runtime manifest, generation key and hardware audit JSON; the readiness scripts delegate
the semantic match to the same strict Rust gate as the client. This offline check does **not** prove
R2 delivery or the envelope signature; the installed client must separately fetch and verify the
published signed `current.json` during the true smoke. Supply the already captured player index
through `-PlayerTelemetryPath`.

## Typical flow

### 1.11.240.0 adaptation — authoring only, not production-approved

The `authoring/1.11.240.0/camera-reference.json` file pins the new executable and the
PlayerCamera relocation from F4SE 0.7.9 commit
`6f6a7caa9aeaebc957a70d934b6ab6b80eb768b2`. It deliberately is **not** a CameraBinding:
it contains no render matrix sources, projection, geometry authorization, or depth profile.
The shipped catalog remains pinned to 1.11.221.0. No old evidence or performance measurement
has been relabeled for the new build.

The probe selects camera/player layouts by exact hash. 1.11.240 is admitted only for discovery
or an explicit managed `performance_validation_only` run; the latter writes a permanent
`provenance/performance-validation-only-fallout4-authoring.json` marker before capture so client
recovery cannot finalize it as deliverable. A normal StartRun and the legacy production watcher
still reject this candidate, even if a caller supplies a new-hash binding. Unknown hashes are
rejected in every mode. Existing 1.11.221 offsets and per-frame capture work are unchanged.

Before discovery, independently confirm the exact executable/runtime/anti-cheat gate, actual
main-scene DSV size and format, and an offline single-player first-person scene. Restart the game
to remove any resident probe. Use a separately built local validation probe/injector/analyzer via
`-ArtifactRoot`; do not overwrite the installed package or publish these local binaries.
For example, **only after verifying a 1920×1080 DSV**, start the first bounded static window:

```powershell
tools\pr405-fallout4\Invoke-Fallout4CameraEvidenceSweep.ps1 `
  -Action StartStatic -OutputDir <new-local-evidence-directory> `
  -ArtifactRoot <local-validation-binaries> `
  -CameraBindingPath tools\pr405-fallout4\authoring\1.11.240.0\camera-reference.json `
  -ExpectedFallout4Hash fdcef37ac1230af6d0b0050eb2142b139ef3a867b37b9211fb6edfcc646072f8 `
  -RenderWidth 1920 -RenderHeight 1080 -WindowCount 1 -DrawWindowCount 8
```

The operator remains motionless for STATIC, translates without rotating for WALK, then rotates
in place for TURN. Follow the returned next phase and keep the same window/sample budgets.
This small initial window is a smoke, not sufficient evidence by itself; expand the search only
when results require it. `Abort` stops the owned sweep and restores the prior disabled config.
The waiters require report **v4** (camera-to-world quaternion plus exact-draw viewport evidence).
Hybrid v2 analysis rejects pre-v4 reports and compressed weapon-layer depth ranges even when their
orientation/FOV looks correct. It conservatively requires one full-render viewport with a valid
depth span of at least 0.5; this is candidate eligibility, not world-camera or E-geo approval.
No observed Min/MaxDepth, raw index or draw ordinal is rewritten. See docs/123 §2.
For 1.11.240 discovery, explicitly sample additional passes/windows; first-pass draws 0..7 selected
the wrong layer. New main-scene candidates still require controlled capture and strict E-geo.
Start pins executable path, SHA-256, process creation time, DLL hash, and refuses a resident DLL.

The offline analyzer now has an explicit `--analysis-mode fallout-render-hybrid` mode. It uses
same-Present reference position plus the validated Fallout render-basis decoder, never a zero-
translation reflected matrix as a complete rigid view. Both Fallout capture scripts select it
explicitly; generic analysis and historical reports retain their previous semantics. New reports
use separate hybrid schemas, preserve rejected candidates and position provenance, refuse output
overwrite, and still require a unique source plus independent E-geo. See docs/122 and docs/123.

2026-09-07 local 1.11.240.0 STATIC/WALK/TURN discovery sealed reliably and replayed through the new
analyzer, selecting one VS12 / 752-byte / offset-0 source with equivalent draws 0..7. This is
authoring evidence, not production compatibility or performance certification. Raw reports stay
under ignored `target/`; no device information or gameplay artifacts belong in Git.

The separate 75 -> 90 -> 75 projection capture also sealed reliably (114 / 91 / 46 reference
samples). The exact VS12 / draw6 / offset64 projection yielded horizontal FOV 74.999999 / 90 /
74.999999 degrees and restored identical focal scales. This audits the stored final matrix per
phase, not a continuous per-Present projection track or RGB/depth alignment. Its reports and the
new strictly recomputed authoring candidate remain under ignored `target/`.

Two completed short captures now reject that candidate: 917 drawn-weapon frames and 916
holstered frames both pass exact watermark/depth pairing, but fail the strict E-geo gate.
The selected pass0/draw6 still supplies viewport `0..0.01`; in the holstered capture all 729
raw-depth frames have their minimum value above that interval (overall minimum 0.824079).
The current coefficients therefore cannot linearize the scene. Holstering is a diagnostic
comparison, not a supported workaround. Broader pass/projection evidence is required before
authoring a replacement; do not rewrite captured indices or loosen geometry thresholds.

The current scope is 1.11.240.0 only; no new old-version compatibility or fallback is planned.
The subsequent report/v4 scene sweep has 25 reliable samples per phase. Multiple passes retain
matching orientation, so its original result remains Ambiguous. Pass 3 / draw 0 has complete
75-sample coverage, stable viewport 0.01..1 and near approximately 15. It is selected only for a
local trial using `camera-binding-author --trial-source <source.json> --trial-reason <reason>`.
The trial manifest and E-geo result use separate schemas and never authorize production, even
if local geometry passes. The earlier pass0 FOV proof is not evidence for this scene projection.

2026-09-08 trial: 920 decoded video watermarks, all 453 eligible even frames paired, no camera
diagnostics. Unchanged E-geo still FAILS only improved-share (0.5784 < 0.60); valid-frac p10 is
0.8136 and improvement p50 is 2.4188. Exact explanatory replay found all 36 translation and 22
rotation samples improved; 44 other usable samples were below the existing pose-motion thresholds.
The original failure is retained; no samples or thresholds were changed. See docs/123 for details.

The subsequent independent motion-rich capture, with identical binaries/binding/profile and
unchanged thresholds, PASSES trial E-geo: 918 watermarks decoded, exact eligible-frame coverage
1.0, improvement p50 3.7950, improved share 0.9583, valid-frac p10 0.7564, inverse p50 0.7881.
The report remains `production_binding_authorized=false`. This is one short scene validation,
not broad scene/weapon-state coverage. It also records 238 repeated video output frames, which
must be examined in performance acceptance; zero queue drops does not mean zero duplicates.

Subsequent unchanged 30-second trials also passed for drawn-weapon outdoor and interior scenes:
improved share 89.62% / 81.03%, improvement p50 2.5968 / 2.0300. They still report
`production_binding_authorized=false`. The first interior attempt failed at 57.80% improved
share and remains immutable; the independent continuous-motion retry did not change any
threshold. Outdoor repeats (149/920 in the drawn-weapon run) remain a performance concern;
low interior repeats (10/918) do not erase them.

The [versioned authoring directory](authoring/1.11.240.0/README.md) now contains the reviewed
candidate source files and a sanitized evidence summary. These are not a shipped catalog.
Binding JSON semantics match the measured input; the profile changes only its DSV evidence
locator and formatting. Both reviewed and original measured hashes are recorded separately.
Historical metric-scale evidence is not relabeled as a new measurement.

The first hardware short run with the queued-source CFR fix retained the exact probe/binding/profile:
1/915 duplicates versus the earlier helper's 149/920, exact even-frame pairing 458/458,
and trial E-geo PASS at 88.89% improved share. It used a restarted game and independent motion,
so this is before/after evidence, not strict A/B/A causal proof or long-run certification.
Some median latency metrics did not improve; see the authoring summary's separate recorder regression.

Pending: formal binding review, Player semantics, a production versioned binding/profile,
recording-cadence long-run/latency acceptance and final performance certification. The three bounded
scene/state checks are not exhaustive scene coverage or release approval.

Pure script regression (no injection/game/ProgramData access):
`powershell -NoProfile -ExecutionPolicy Bypass -File tools/pr405-fallout4/Test-Fallout4CameraEvidence.ps1`
(execution policy is scoped to that test process, not changed on the machine).

### Verified 1.11.221.0 flow

Build the release artifacts first, then run the readiness gate from an ordinary PowerShell prompt:

```powershell
tools\pr405-fallout4\Test-Pr405Readiness.ps1 `
  -PerfCertificationManifestPath C:\path\to\performance-certifications.json `
  -ProbeRuntimeManifestPath C:\path\to\dm-depth-probe-runtime.json `
  -PerfCertificationKeyPath C:\path\to\generation-key.json `
  -PerfHardwarePath C:\path\to\hardware-audit.json `
  -PlayerTelemetryPath C:\path\to\player-pose-index.jsonl
```

Only exit code 0 permits a bounded preview:

```powershell
tools\pr405-fallout4\Start-Pr405Preview.ps1 `
  -TargetProcessId 1234 `
  -PerfCertificationManifestPath C:\path\to\performance-certifications.json `
  -ProbeRuntimeManifestPath C:\path\to\dm-depth-probe-runtime.json `
  -PerfCertificationKeyPath C:\path\to\generation-key.json `
  -PerfHardwarePath C:\path\to\hardware-audit.json `
  -MaxFrames 120
```

The preview report is `preview-validation.json` under the generated output directory. A preview is
evidence for readiness only; it is not a certified PR 405 delivery. Formal materialization still
requires strict RGB/depth frame pairs and a fresh full delivery validation.

`Start-Pr405Preview.ps1` injects (or hash-verifies and safely reuses) the exact
`dm_depth_probe.dll` from `-ArtifactRoot`, sends the profile/binding SHA-256 values in `StartRun`,
and rejects a `Ready` that does not echo them. This prevents a current script from silently testing
an older resident DLL.

## Issue 609 local research certification (2026-08-10)

The exact build above has now passed the depth-specific production checks in an offline
single-player save:

- an 8-frame bounded preview;
- a 60-second run with 1,800/1,800 depth rows and files;
- a five-minute run with 9,000/9,000 depth rows and files; and
- a local RGB/depth same-shape delivery smoke with 238/238 watermarks decoded, 100% eligible-frame
  pairing coverage, a zero-drop clip plan, and a complete local delivery bundle.

All production runs used `pass_identity_trace=false`. Across the five-minute run, all 9,000 depth
sequences were contiguous, finite and non-uniform. Every frame used the exact configured projection
draw, selected `main-scene-v1`, matched the ranked DSV winner, and retained an
`observed_this_frame` projection. The checked-in summary is
`evidence/fallout4-production-certification-20260810.json`.

The RGB smoke initially exposed a real integration blocker: the Razer Cortex FPS overlay covered
the 192x8 watermark rectangle at the top left. Toggling `In Game FPS` off with the configured Cortex
shortcut restored lossless watermark decoding. Recorders must keep third-party overlays away from
the configured watermark rectangle.

This certification is local, private, non-trainable research evidence. No artifact was uploaded or
externally delivered. The same-shape delivery smoke uses a generated 30 Hz frame clock and neutral
input fields because this task certifies the depth adapter, not the production input recorder; it
must not be represented as a formal dataset delivery or input-calibration certification.

For the diagnostic pass/draw identity experiment only, add `-PassIdentityTrace`. This writes one
`depth-pass-identity-trace.jsonl` row per captured depth frame and joins the exact production
projection draw to every outgoing selected-DSV pass. The switch is off by default and deliberately
retains the full candidate snapshot ring for audit.

Normal exact-build Camera production does not use `unbind_from_end` as a prediction. It counts all
selected-DSV unbinds for durable pass evidence, but issues `CopyResource` only for the pass that
contains the configured exact draw and an `observed_this_frame` projection. Missing, duplicate or
mismatched identity faults the production stream; it never falls back to a nearby ordinal.

## #636 render-camera evidence

The render-camera re-evidence workflow is documented in
`docs/122-fallout4-render-camera-evidence-contract.md`. Discovery itself is intentionally separate
from the DataMiner recording UI and performance calibration. The selected source subsequently
passed the strict RGB/depth E-geo v3 gate on 2026-08-11 and was promoted to the bundled render-hybrid
v3 binding. The gate binds the recorder run projection, Ready/Flushed identity, validation-only
provenance marker, and the run-owned probe/binding snapshots; cross-run index assembly is rejected.
That geometry authorization does not replace the client's exact final-DLL/binding
performance-certification gate.
