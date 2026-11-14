# CP2077 issue #735 preflight

## Current recording constraint: holstered (2026-09-14)

The normal client catalog/StartRun path now understands the explicitly versioned
`camera-binding/cp2077-holstered-v3`: paired authored aliases, fresh physical
warmup on every launch, and independent world/camera basis declarations. This is
runtime support, not a published CP2077 profile/binding. Recording capabilities
and runtime admission are unchanged. Offline authoring now accepts a versioned
evidence set that reuses motion, FOV and geometry from their actual launches;
each launch need not repeat the entire experiment. See the
[integration status and outstanding hardware evidence](ORDER-WATERMARK-DIAGNOSTICS.md#常规客户端绑定接线2026-09-15).

All subsequent CP2077 depth diagnostics require the operator to keep weapons
holstered, with no visible hands/weapon model, no Shift, and an unobstructed
outbound/return route. This is an operator constraint, not automatic visual
weapon detection. Existing sessions remain immutable and non-trainable.

The explicit `--selected-present-holstered` diagnostic now reuses
the shared GPU candidate ring and selected-Present transport after same-launch
camera admission. It requires a newly built diagnostic DLL and new discovery;
old sealed DLLs and profiles do not acquire this capability. See
[bootstrap, stop ordering and limits](ORDER-WATERMARK-DIAGNOSTICS.md#空手相机准入与-selected-present-短测入口).
The v2 raw bootstrap/Ready negotiates feedback v4 and `even_cfr_30fps/v1`.
RGB stays native 30 FPS. Every CFR slot still sends its final identity; even
`session_frame` values (0, 2, ...) require depth, odd slots explicitly do not.
An odd slot's unselected canonical Present remains in the GPU ring until a
later required slot uses it or the RGB source advances. A duplicate RGB slot
can therefore require its **first** depth readback. No original pixels or QPCs
are synthesized. Ordinary feedback stays byte-compatible v3/every-RGB-frame.

Omitting `--manual-stop` enables the existing 64-second RGB timer; the raw config
must also have `manual_stop=false`. Stop closes RGB, seals feedback, then drains
the probe readbacks/writer. Probe `elapsed_ms` still includes warmup and drain;
it is not relabelled as the video duration. Timed RGB remains 63–65 seconds,
1890–1950 frames. Even slots yield 945–975 required references (nominally 960),
while the separate 900–1024 **distinct original-depth** gate remains unchanged.
Short operator diagnostics may still use `--manual-stop`; insufficient duration
or original count remains an expected failure, never an acceptance result.

Pairing report v3 adds the complete even-slot/full-Present map, original QPCs,
depth ordinals, run identity and input hashes. Missing one required depth or
watermark, extra unrequested depth, or unresolved feedback fails the attempt;
checking only the old depth-to-RGB coverage is insufficient. Existing source,
pixel, camera, geometry, input and non-trainable gates remain in force. The
local selected-geometry reader accepts this explicit v2 summary and emits
geometry v2 with required even-slot coverage counted separately from all 30 FPS
video watermarks. Legacy v1 inputs retain every-RGB-frame depth checks and their
v1 report; geometric thresholds and original-session SHA checks are unchanged. The
legacy formal five-piece packager has not been promoted to accept this raw-only
contract. Camera report deduplication is a separate pending change; its current
capacity limits still apply. Rebuild/seal, rediscover and short-test before a
64-second hardware attempt; these source changes are not hardware evidence.

For a completed timed even-CFR run, `--audit-session ... --output ...` verifies
the recorded v2 Arm before selecting its 15 Hz contract and emits offline replay
v2, including required-slot pairing. Legacy recordings keep 16 Hz/v1. The offline
camera reader uses the same bounded archive reader as recording finalization.
Geometry checks bind the actual 30 FPS metadata, frame count and exact run path;
pairing rejects duplicate watermark rows or missing completion state and binds
the original Arm/metadata hashes. These offline reports never upgrade a session.

This directory contains the reproducible, non-trainable preflight inputs for
issue #735. It is deliberately **not** a game-adaptation certification or an
authorization to inject the depth probe.

`cp2077-build-manifest.json` records only exact-build facts that are safe to
review in Git. It excludes install paths, account names, save data, adapter
identifiers and captured game content.

`evidence/` contains redacted runtime and synthetic summaries. Raw local
reports remain under the ignored `tools/capture-lab/out/` tree.

## Standalone fixed-view diagnostic protocol (2026-09-09)

### Independent direction/FOV authoring windows (2026-09-15)

The diagnostic raw config may explicitly set `camera_authoring_windows: true`.
This requires a `cp2077-diagnostics` build with debug assertions, manual-stop
`issue735_local`, and at most 1,024 original frames. It is incompatible with
selected-Present recording. Production binding, camera admission, FOV tolerances,
and continuous-recording coverage gates are unchanged.

After one ordinary raw arm, publish create-new, atomic
`camera-authoring-window-01.json` (then 02, etc.) in that same private session:
`schema=dm-cp2077-camera-authoring-window/v2`,
`source_policy=revalidate-at-drained-boundary`, exact `process_launch_id`, sequential
`ordinal`, an alphanumeric/hyphen `phase`, and `duration_ms` (1,000–40,000).
V1 requests are rejected; they did not authorize a phase source change.
At most eight non-overlapping windows and 64 seconds of requested observation are
allowed within the unchanged 240-second arm lifetime and original frame ceiling.

Before each window, the probe requires the prior render interval, authorized
Present depths, deferred/submitted camera draws and GPU slots to drain. It archives
the camera tail and passes a FIFO durable-writer barrier before proving exactly one
currently executed-clear semantic DSV. Preparing a source has a five-second bound.
Zero active sources keep the next window closed while waiting within that same
bound; the deadline is never restarted and historical targets are never substituted.
Multiple active sources, preparation timeout, identity/geometry drift or a failed
session still stop.
A healthy phase change reuses the same ring/device/footprint and preserves capture
IDs, fence/queue identities, counters, observations and copy receipts. It does not
clear/re-arm a failed session. In-flight work can never be discarded to enable a
new source. Within an active window the original immutable identity/cadence guards
remain; ordinary raw, selected-Present and production selection are unchanged.

Native receipt v2 is published only after the actual source is prepared. It records
the previous/new candidate, prepared Present/QPC, request hash, writer barrier and
window QPC bounds. Raw depth index v2 adds the authoring phase to every frame and
checks the GPU frame's actual DSV identity against its authorized Present. Old/late
frames cannot be labeled by a newer global binding. These v2 rows are diagnostic
and are not accepted as ordinary continuous-recording input.

The observation gate is latched per render interval: reaching the deadline stops
new intervals, while the already-authorized last interval completes its camera
observations. The watermark appears only in active observation intervals. Menus
request no new copies, matrices or depth-order diagnostic events; already recorded
work still executes/drains. Existing archive and depth-order capacity/loss counters
remain bounded and visible; this is not a claim that all diagnostic losses are zero.
Every window starts its own cadence clock without altering any original timestamp.

These deliberately discontinuous artifacts are **not** a short joint recording;
the native authoring windows alone contain no Raw Input evidence and must never
be assigned recording coverage or input-QC success. The optional action witness
below is separate evidence, not a continuous RGB recording. Operator phase labels are claims to check against physical
matrices and independent setting snapshots, not automatically verified actions.
Controller preparation must recheck the configured FOV, resolution and FPS cap
after foreground waits and immediately before publishing each window, then retain
setting changes during the window. A setting read before a 15-second preparation
can be stale. Settings remain supporting evidence, not matrix proof; the same
unlabelled white watermark is not an acknowledgement of a particular phase.
Collect forward/return, right/return and crouch/stand motion at FOV 75; retain
same-process settings/matrix evidence for 75 → 90 → 75, then independently repeat
the complete experiment in a fresh process. Never re-arm a failed session.
Missing directions, source receipts, return accuracy, A/B correspondence or
geometry remain blockers; no candidate becomes an approved catalog binding here.

The reusable controllers now live in `authoring/`. Copy those scripts into the
**next new sealed run bundle**, alongside that bundle's verified `common.ps1`,
launch/runtime records and `Stop-Authoring.ps1`; do not overwrite a prior bundle.
**Automatic authoring is suspended.** The new GPU WAIT / REC / FOV / STOP
renderer and its Present integration have been removed after a game exited
about 1.45 seconds after its first sampling request (0xc0000409, no receipt or
depth frames). No crash stack established its cause; withdrawal is risk reduction,
not a verified crash fix. `Invoke-AuthoringSequence.ps1` now rejects immediately,
before loading a bundle, launching anything or publishing an arm/window request.
Old sealed scripts and DLLs remain evidence and must not be used for another run.

`Capture-AuthoringWindow.ps1 -RunDirectory <new bundle> -Stage authoring-a
-Ordinal <1|2|3> -FovOnly` remains the single-phase controller (4 seconds per
phase). Each phase needs explicit operator readiness and return to the game;
there is no in-game prompt to wait for. Stop on game exit or missing native
receipts and retain the session. It checks the same process before publishing,
while waiting and throughout sampling, reporting game exit separately from a
receipt timeout. Full three-axis/A/B evidence remains incomplete. Do not resume
game tests until offline review and a new seal; first test sampling startup alone,
not a complete FOV sequence. Keep the existing 64-second evidence.

At stop, `camera-authoring-timeline.json` preserves at most 2,048 native source
samples (nominally 4 Hz plus phase boundaries). The v1 `guide_submissions` array
is always empty and `guide_evidence` explicitly states that GPU prompts are disabled.
Sampling uses the existing full candidate snapshot; the active count is complete,
with up to four IDs and an explicit truncation flag. Present only updates bounded
memory; it does no file I/O. Controllers emit create-new `*.transitions.jsonl`
files (up to 2,000 events each), using the same original QPC/frequency for FOV,
foreground check completion, requests, native receipts, completion and failures.
After stopping, `Merge-TransitionTimeline.py --native <native JSON> --controller
<sequence JSONL> <phase JSONLs...> --output <new JSON>` verifies launch/clock and
input SHA before/after the read-only merge. It does not infer menu state or alter
recorded frame timestamps. These small diagnostic traces are separate from depth
pixels and the camera archive and remain non-trainable.

Offline checks (no game launch): `Test-SequencePolicy.ps1`,
`Test-Controller.py --output <new directory>`,
`Test-TransitionEvidence.ps1 -OutputDirectory <new directory>`, and
`Test-TransitionTimeline.py`, all in `authoring/`. Synthetic tests do not establish
the root cause or resolution of the game exit.

### Bounded authoring action witness (2026-09-16)

The diagnostic standalone helper accepts this explicit, separate mode:

```powershell
dm-cp2077-raw-demo-record --observe-authoring-input --pid <PID> --launch-id <exact launch id> --duration-seconds 60 --output-dir <new absolute directory>
```

It requires `cp2077-diagnostics` **and** debug assertions. It reuses the existing
target-bound `TargetWindowMonitor` and `RawEventWriter`, including the neutral-input
barrier, bounded queue, foreground-loss stop and original QPC event timestamps.
It does not record RGB, arm depth, focus a window or inject input. The maximum is
10–60 seconds; a missing stop is a failed timeout, not completion.

The controller starts it after foreground preflight, waits for atomic `ready.json`,
then opens the first movement window. At the window end it atomically publishes
`stop-request.json` (`dm-cp2077-authoring-input-stop/v1`, exact `process_launch_id`),
waits for `result.json` and verifies that the sealed input interval covers the
native window. `authoring/Input-Witness.ps1` supplies these helper operations.
Stop it before opening FOV menus; phases two and three need no movement witness.
Only ordinal one arms depth; any failed session or witness is retained and never
reused. The continuous three-phase controller retains its 200-second total budget
and the probe's existing 240-second safety ceiling, without intermediate chat waits.

The result (`dm-cp2077-authoring-input-witness/v1`) hashes the original sealed
event stream and reports loss/fault state. Partial events are finalized on an
observation failure when the writer remains healthy. Planned cue times are not
event times, and a key press alone cannot prove movement, posture, collision-free
motion or the physical coordinate basis. Compare actual events with camera QPCs,
verified game key bindings and operator evidence; no automatic approval is emitted.
These witnesses remain non-trainable and do not authorize a production binding.

Offline helper supervision test (new output directory, no game/input capture):

```powershell
pwsh -File tools/pr735-cp2077/authoring/Test-InputWitness.ps1 -OutputDirectory <new local test directory>
```

Build the standalone local helper from `DataMiner/` with the existing raw-demo
manifest selection in `apps/client/src-tauri/build.rs`:

```powershell
$previousRawDemoBuild = $env:DATAMINER_RAW_DEMO_BUILD
try {
    $env:DATAMINER_RAW_DEMO_BUILD = '1'
    cargo build -p dataminer-client --release --features depth-lab,dev-catalog --bin dm-cp2077-raw-demo-record --locked
} finally {
    $env:DATAMINER_RAW_DEMO_BUILD = $previousRawDemoBuild
}
```

This selects the existing asInvoker acceptance-helper manifest for the local
tool. A normal release client build uses its separate UAC manifest.

When reusing a Cargo target directory across source exports, verify the actual
`build-script-executed` message for `dataminer-client`: its emitted
`DATAMINER_CONFIG_DIR` must resolve to the current source's `DataMiner/config`.
The build script reads Cargo's package directory at execution time and watches
the relative config directory; a path captured with `env!` in a cached build-script
executable can otherwise embed catalogs from an older export. A successful Cargo
exit or a matching DLL version alone does not establish correct client provenance.
Keep source hashes before/after, feature/profile records and all five tool hashes;
an incorrect config path invalidates that seal before any game launch.

The guided wrapper reads the Rust preview and live cues explicitly as UTF-8,
including when launched by a hidden PowerShell process. After the preparation
countdown, keep every keyboard and mouse button released until the actual W cue;
the countdown is not the first-frame or movement anchor. Non-neutral first-frame
failures retain the pre-anchor contract-key and mouse-button virtual-key codes in
recorder stderr. These diagnostics use the existing Raw Input history and do not
clear held state or relax the gate. Once the standalone coordinator arms depth,
normal completion and every subsequent error return request its same-launch stop,
including failures before the first RGB frame. A failed session must not be reused.
Audio preview and preparation speech contain only a request to stay still; they
do not quote any movement instruction or key name. Movement speech is reserved
for actual Rust action cues after the first video frame.

The timed `issue735_local` / 64-second standalone recorder emits action-plan v2
and result v3. Its explicit input policy is `fixed_view_ws/v3`: real W/S
events, continuous outbound/return holds, no sprint, mouse movement, conflicting
keys, or keys left held at the end. Key repeat is permitted; releasing and pressing
again cannot be counted as one continuous hold. For manually counted timing, both
key-down and key-up allow ±5 seconds around their actual action cues (v2 used
±3 seconds); each continuous hold must still last 12–24 seconds. The action
timeline, initial neutral-input gate, depth/camera/pairing and coverage requirements
are unchanged. Read-only replay may evaluate old v2 evidence with the explicit v3
policy in a separate report; it never rewrites the original session or its outcome.
This narrow diagnostic does not
prove mouse/three-axis calibration. PR405, manual-stop standalone capture and the
main collector retain their existing keyboard-and-mouse requirements and timing policy.

The local fixed-view camera check uses `fixed_view_centered_half_pixel/v1` only
for the observed normalized row-vector perspective family at native 1920×1080.
The 14 non-offset coefficients retain the existing maximum delta of 0.001;
`[2][0]` and `[2][1]` must each remain within ±0.5 native pixel of the centered
principal point (1e-6 pixel numerical tolerance). Unknown/oblique/transposed
geometry, non-finite values and larger offsets fail. Source matrices are preserved
per frame; this does not identify the game's jitter implementation or certify
metric geometry. Other camera contracts retain full-matrix stability validation.
The pose/camera QC schema is v4. Timed local fixed-view diagnostics use
`same_launch_selected_cbv_copy_series/v1`: every source/draw/copy observation is
retained against the original report hash. Repeated draws may write the same
complete payload into one unchanged, generation-bound destination range from one
generation-bound upload resource; upload offsets remain in each exact receipt and
may differ between draws;
they are distinct copies, not one physical alias. Root-parameter families need
at least eight common Presents with equal full payload hashes and matrices.
One draw or copy-submission identity cannot contradict itself. A range/generation
change, unequal payload/matrix, missing receipt, loss counter or insufficient
coverage still refuses admission. Every retained view draw requires its exact
projection receipt. The original exact-copy alias reducer, cross-launch authoring
and production binding gates remain unchanged. The policy is offline-only and
does not require replacing an unchanged probe DLL.

For a bounded raw short test, the same recorder can separately observe the pinned
game HWND and process CPU times without activating it, registering input, or arming
capture:

```powershell
& $recorder --observe-window --pid $gameProcessId --duration-seconds 90 --output $newAbsoluteReport
```

The observer sleeps 100 ms between checks for 10..120 seconds; actual sample
intervals also include validation cost. It retains process creation
identity and QPC, and records early window failures. It does not measure FPS or
GPU cost. Correlate its sampled foreground/CPU observations with the raw/pose
Present timestamps and arm/stop QPC; do not claim continuous foreground coverage
between samples. Output is create-new. This command neither starts nor stops
the independent raw capture.

Before asking an operator to move, show the complete timetable and run the Chinese
speech preview. `Invoke-Cp2077GuidedCapture.ps1` defaults to preview only; it neither
launches nor records the game. Its timetable comes from the exact recorder binary,
so display text and Rust action steps cannot silently diverge:

```powershell
$recorder = (Resolve-Path '.\target\release\dm-cp2077-raw-demo-record.exe').Path
& $recorder --print-action-plan
.\tools\pr735-cp2077\Invoke-Cp2077GuidedCapture.ps1 -RecorderPath $recorder
```

After a fresh validated independent game launch, add `-StartCapture`, `-SessionDir`,
`-GameProcessId` and `-LaunchId`. The wrapper refuses completed/stale sessions,
provides a 10-second preparation interval (configurable up to 60), then follows
Rust's actual QPC cues. It never supplies game input. A missing, duplicate or
out-of-order cue fails the wrapper; an exception after arm requests the existing
same-launch probe stop atomically. Preview v2 carries detailed `instruction_zh`
and separate `speech_zh` short cues. Spoken “前进” means hold W, “后退” means hold S,
and “松手” means release. Actual Rust markers still determine all action times.
`spoken-cues.jsonl` v2 records request/return QPC and bounded SAPI completion;
a cue exceeding 1500 ms fails presentation and requests stop. This software
completion does not measure Bluetooth, device or acoustic onset latency.
Live chat text is auxiliary and can be late;
read the full text before capture and keep the game focused while following speech.

| Seconds | Operator action |
| --- | --- |
| 0–5 | Stand still; keep the mouse still. |
| 5–23 | Hold W continuously, without Shift. |
| 23–29 | Release W; stand still. |
| 29–47 | Hold S continuously; back up without turning. |
| 47–49 | Release S; stand still. |
| 49–57 | If necessary, use short W/S presses to correct position. |
| 57–64 | Release all keys; remain still until capture ends. |

An already completed coordinated run can be diagnosed with the current validators
without re-arming or replacing its original result:

```powershell
& $recorder --audit-session $completedSession --output $newAbsoluteReportOutsideSession
```

The create-new `dm-cp2077-offline-validation-replay/v1` report separately records
camera, player, input and continuous-hold results plus hashes of the source evidence.
It accepts the prior v1 timed hardware-input plan for comparison and labels the new
policy explicitly. It does not repeat pixel QC, upgrade the old attempt or authorize
delivery. All results remain local and non-trainable.

The D3D12 probe now reports executed-copy receipts and lifetime CPU timings for
selected-draw resolution, pre-Execute snapshots and post-Execute bookkeeping.
Pre-Execute mapping lookup visits only exact requested sources, rather than all
tracked scene buffers. Duplicate-frame counters and target CFR remain unchanged;
this optimization still requires a game performance check. The new DLL must have
fresh discovery/profile evidence before a new independent recording.

For a GPU/encoder-only check with no game, window capture or input, run
`capture-lab.exe --encoder-preflight --profile <CaptureProfile.json> --out <new-dir>`.
It uses the existing hardware preflight and saves create-new `encoder-preflight.json`.
MF now receives rate/GOP preferences during input-media-type negotiation and still
verifies them through ICodecAPI. Unsupported controls stay explicitly recorded;
hardware-only, H.264 output, timestamps and IDR checks remain required.

Before any game-process instrumentation:

1. run the repository rights gate for AppID `1091500` in its default mode, which requires a
   publisher or counsel approval with all four scopes explicitly bounded;
2. verify the Steam install and reproduce the executable hash;
3. perform the documented runtime anti-cheat/protection check;
4. stop if the build identity, launch mode or protection state differs.

From `DataMiner`, reproduce the static preflight without launching the game:

```powershell
.\tools\pr735-cp2077\Test-Cp2077Preflight.ps1 `
  -OutputPath tools\capture-lab\out\cp2077-static-preflight.json
```

The expected current result is `preflight_pass_runtime_blocked`: it proves the
installed exact build and the live unified rights approval, while keeping the
renderer/protection gate false until a separate runtime check.

If the game lives outside the default Steam library, pass `-SteamLibraryPath`;
the default-path failure is `Steam app manifest not found`, not a build mismatch.

`steam_size_on_disk` is the one check that tracks **installed content** rather
than build identity, so installing or removing a DLC, language pack or preload
fails it while the build is unchanged. Its failure prints two totals and does not
say what the difference is. Diagnose it against `size_on_disk_breakdown.depots` in
`cp2077-build-manifest.json`: subtract the DLC depots and confirm
`executable.sha256` plus `source.build_id` are unchanged — those two are the real
identity gates. Only then revise the total, recording the new depot arithmetic in
the same field. Never bump the number to turn the gate green before the difference
is accounted for, and do not silence it by pointing `-ExpectedManifestPath` at an
untracked private manifest: that is how this repository's expected value sat one
DLC behind reality from 2026-08-12 until 2026-09-10 while every local round
reported a pass.

Before every recording attempt, validate the exact CP2077 capture recipe:

```powershell
.\tools\pr735-cp2077\Test-Cp2077CaptureSettings.ps1 `
  -OutputPath tools\capture-lab\out\cp2077-capture-settings.json
```

After exiting the game normally, apply the audited recipe with a timestamped,
atomic backup:

```powershell
.\tools\pr735-cp2077\Set-Cp2077CaptureSettings.ps1
```

The dedicated metric-calibration run is the only mode that deliberately keeps
the objective HUD marker visible. Apply and validate it with the explicit
purpose (the client does this automatically from the “准备米制标定” button):

```powershell
.\tools\pr735-cp2077\Set-Cp2077CaptureSettings.ps1 -Purpose metric_calibration
.\tools\pr735-cp2077\Test-Cp2077CaptureSettings.ps1 -Purpose metric_calibration
```

Its `UserSettings.metric-calibration-*` snapshot/attestation is distinct from
the formal `issue735` evidence and can never pass the hidden-HUD gate. Reapply
the default formal recipe after calibration before any formal clip.

The apply script refuses to replace the live file while `Cyberpunk2077.exe` is
alive, checking both before authoring and immediately before the atomic swap.
It changes only the audited capture graphics, HUD, subtitle, and call-display
fields; it does not touch Steam, network, cloud-save, telemetry, or save-game
settings. Use a copied settings fixture for script tests; never run the setter
against the live file while the game is open.

The setter preserves CP2077's native JSON bytes and edits only the unique
audited option `value`/`index` tokens. It deliberately does not round-trip the
whole document through PowerShell `ConvertTo-Json`: that serializer expands the
file and changes typed float tokens such as `75.0`/`0.0` into integer-looking
tokens, which CP2077 can report as a corrupted UserSettings file. A fixture
apply must remain close to the original byte size and retain those float forms.

The setter also creates a timestamped, create-new snapshot and hash attestation
for the next launch. The final coordinator requires that exact snapshot to pass
every PR #727 check and to have been completed no more than five minutes before
the bound process started. CP2077 may rewrite the live file after startup; that
drift is accepted only when its file timestamp is strictly after the same
process creation time. A subsequent launch therefore fails closed until the
operator exits normally and reapplies the recipe. Do not delete or edit the
snapshot/attestation between apply and capture.

This gate requires native 1920x1080 rendering (resolution scaling, dynamic
resolution, frame generation, and ray tracing off), FOV 75, the PR #727 hidden
HUD recipe, cinematic/radio/overhead (NPC background-chatter) subtitle surfaces off,
dispatcher/fixer holocalls replaced by text notifications, and image-space post
effects off. `settings_ready=true` is necessary but does not upgrade
`recording_ready`, which remains controlled by the independent Depth/runtime
gates. The Depth panel must independently report a ready exact-build binding,
paired frame identity, projection evidence, camera/player pose, and a valid
performance certificate.

The report deliberately returns `recording_ready=false` even when
`settings_ready=true`. Do not treat a settings-only pass or a selectable catalog
entry as permission to start a Depth recording.

The coordinator writes `pr727-settings-runtime-evidence.json` into the private
session. It records hashes and process/file time relationships without copying
the local settings path. FOV is additionally checked from the physical render
camera report; the bound-HWND `video.mp4` remains the auditable HUD/subtitle
picture evidence and is not falsely described as an automatic visual
classifier. These facts do not change `recording_ready=false` or the
non-trainable ceiling.

After a completed Depth run has produced full-rate pose JSON, verify the target
trajectory before keeping or delivering it:

```powershell
cargo run -p depth-lab -- loop-closure-check `
  --pose action_camera_30fps.json `
  --report loop-closure-report.json
```

The defaults enforce the customer 0.60 m / 8 degree closure tolerances plus a
local 2 m displacement guard and exactly one terminal return to the start pose.

The current rights record is `counsel_approved` under the unified decision in
[issue #855 §E](https://github.com/DiogenesModel/Diogenes/issues/855), which
retired the old project research-only lane. Rights are only one gate: the
runtime renderer/protection check still has to pass before instrumentation, and
the approval does not make any captured data trainable — `is_trainable` is still
derived solely from `clip_trainability`.

## Runtime protection gate

The exact pinned build was re-observed on 2026-08-18 with D3D12 loaded and no
known anti-cheat module, running service, or running driver hit. CP2077 and the
launcher may keep their ordinary background network connections; those
connections are recorded for audit but do not block local capture. DataMiner
remains local-only and its upload, publication, training, and delivery paths
remain closed. A build mismatch, missing D3D12 runtime, or anti-cheat signal
still blocks instrumentation.

The implementation and Go/No-Go gates are defined in
[`docs/142-cp2077-issue735-implementation-plan.md`](../../docs/142-cp2077-issue735-implementation-plan.md).

The final client's exact 64-second out-and-back action cues, actual-QPC marker
contract, and fail-closed pose/camera/Raw Input/Loop checks are documented in
[`docs/evidence/issue-735-cp2077-final-coordinator-gates-20260812.md`](../../docs/evidence/issue-735-cp2077-final-coordinator-gates-20260812.md).

The historical r26 D32S8 authoring and cross-launch semantic rebind investigation is documented in
[`docs/evidence/issue-735-cp2077-r26-d32s8-selector-20260812.md`](../../docs/evidence/issue-735-cp2077-r26-d32s8-selector-20260812.md).
It has been superseded for PR405 packaging by the exact native 1920x1080 current-r19 selector;
legacy 1280x720 r26 raw remains diagnostic-only and is rejected by the package/E-geo gates.

## Local PR #405 conversion

For the explicitly authorized unattended 30-second run, add the valueless CLI flag
`--autonomous-input` to the `pr405_local` coordinator command. This mode is unavailable to the
64-second contract. It checks the bound HWND, root-owner foreground window, and exact PID before
and immediately after every `SendInput` transition, and continuously while a movement key is
held. It emits only W-down/W-up/S-down/S-up on the action plan's actual QPC cues. Any focus/PID
drift, contradictory held-key state, or failed injection immediately releases every held W/S key
and fails the run.

The resulting `autonomous-input-ledger.jsonl` declares
`input_provenance=authorized_autonomous_sendinput` with scheduled/attempt/actual QPC, PID/focus
checks, transition, and injection count. Windows does not expose `SendInput` as hardware
`WM_INPUT`, so this mode requires the recorder's Raw Input stream to remain empty and never labels
the ledger as hardware input. Manual mode remains `input_provenance=hardware_raw_input` and keeps
its existing Raw Input hard gate. Pose, camera, geometry, movement, loop closure, RGB/depth, HUD,
FOV, and local-only gates are identical in both modes.

```powershell
dm-cp2077-raw-demo-record.exe `
  --pid <exact-pid> --launch-id <32-lowercase-hex> `
  --session-dir <absolute-private-session> `
  --contract pr405_local --duration-seconds 30 --autonomous-input
```

Before starting the `pr405_local` coordinator, copy the package-input template to the new
private session as `cp2077-pr405-package-input.json` and replace the metric-calibration sample
with independently measured evidence. The coordinator validates that calibration before RGB
starts. After capture it authors `cp2077-pr405-frame-evidence.jsonl` and
`cp2077-final-pose.jsonl` only when all 900 RGB frames can be joined to one exact Present each,
all 450 raw rows bind the even frames, and every RGB Present has one unique player observation
plus one unique view/projection pair carrying the audited CopyBufferRegion physical source.
Camera position, rotation and intrinsics are derived from those render matrices; player
telemetry is never substituted for a missing camera. Missing/ambiguous observations leave
neither authored file.

After a 30-second session has independently sealed video, raw D32, exact frame evidence,
camera/player pose, physical projection, dual-source metric calibration, E-geo v3, and
systeminfo, run the local-only converter described in
[`docs/evidence/issue-735-cp2077-pr405-local-packager-20260813.md`](../../docs/evidence/issue-735-cp2077-pr405-local-packager-20260813.md).
It validates all 900 RGB / 450 depth identities before creating an output, materializes the
docs/68 EXR and action-camera shapes in staging, hashes every artifact, and publishes with one
directory rename. Missing evidence is a refusal, never a partial delivery. The converter cannot
upload and cannot change the session's non-trainable status.

The metric file is not a hand-filled `1.0`. Use `cp2077-metric-evidence` in a separate exact-build
calibration session with at least three
full-frame HUD PNG/OCR samples from one straight radial movement toward or away from one unchanged
objective. Every sample states its exact `session_frame` plus Present/QPC and is rejoined to the
probe player row. The client’s metric mode uses `Ctrl+Alt+F9` to export those exact decoded RGB
frames and pre-scale player rows, then emits a deliberately invalid review template. The main
collector currently requires exactly five `Ctrl+Alt+F9` samples, although the offline authoring
validator accepts 3–32 samples. A different
reviewer must copy it, transcribe every whole-metre HUD reading, confirm the same objective, and
provide a distinct reviewer identity before the v2 observations schema can validate. The authoring
CLI hashes all screenshots and inputs, pins the exact-build
RED4ext `WorldPosition` conversion plus the published CET distance-meter implementation, and
recomputes the HUD/probe ratio. The package converter performs the same validation again.
That short calibration explicitly permits a visible objective HUD; it is never the final 900-frame
clip. Reapply and verify PR #727 `WholeHUDHidden` before the formal local recording.

Run `tools/pr735-cp2077/Validate-Cp2077EGeo.py` only after the exact 900/450 inputs exist. The CP
gate shares FO4's geometry kernel and formal-vs-inverse thresholds but requires independent
static/translation/rotation populations and CP's exact Present/QPC rows. Its v3 report binds the
metric evidence, all indexes and poses, video, sampled D32 bytes, and both validation source files;
the package converter rechecks those hashes and metrics before creating staging.

The All Games DEV client now exposes the local operator path directly when CP2077 is selected:
prepare/early-launch, arm on the first WGC frame with an audible cue, user-controlled stop after at
least 64 seconds, drain/QC, and local five-piece packaging. See
[`docs/evidence/issue-735-cp2077-client-fivepiece-landing-20260813.md`](../../docs/evidence/issue-735-cp2077-client-fivepiece-landing-20260813.md).

The five named delivery artifacts stay stable. A sibling `raw-input/` audit directory preserves the
formal main collector's compressed hardware `WM_INPUT` stream, its segment metadata and source hash
manifest. Packaging fails closed unless keyboard down/up and mouse movement are both present, every
input count/hash agrees, and all input overflow/drop counters are zero.

## Clear/copy order and replaced RGB watermark diagnostics

The opt-in, debug-assertions build and exact readout semantics are documented in
[ORDER-WATERMARK-DIAGNOSTICS.md](ORDER-WATERMARK-DIAGNOSTICS.md). Ordinary release
binaries do not enable these diagnostics. Fresh sealing, discovery and an independent
short joint run are still required; all results remain non-trainable.

The 2026-09-15 holstered translation and in-place rotation diagnostics completed selected-Present
feedback and exact offline RGB/depth identity checks. Original camera observation tokens remained
byte-identical through final report serialization. Independent historical HUD/player metric evidence
was revalidated with the current authoring code and unchanged coordinate conversion; it was not
replaced by a fitted geometric scale. See the current-results section of
[ORDER-WATERMARK-DIAGNOSTICS.md](ORDER-WATERMARK-DIAGNOSTICS.md) for counts and limitations.
Compact v3 camera reports now allow 512 MiB for expanded archive rows plus the final document;
the probe writer, shared archive visitor, native client and Python readers enforce the same budget.
Encoded archives remain limited to 256 MiB, v3 tail documents and unarchived reports to 64 MiB,
and legacy v2 full documents to 320 MiB. Both client read sites retain all archive integrity checks.
This 2026-09-16 adjustment follows an operator-requested estimate, without memory/capacity stress
testing or another game run. Rebuild before use; existing sealed tools keep the previous limit.
`Validate-Cp2077SelectedGeometry.py` checks the current native selected-Present data without converting
it to the legacy 900/450 contract. Existing walk and rotation sessions passed this local geometry
check with original file hashes unchanged; the new report never authorizes production or delivery.
See the report-capacity and geometry section of the diagnostics document for the manifest, command,
sampling policy, unchanged thresholds and corrected coverage definition. Production camera binding
is deferred to ordinary client integration. No post-recording depth compression was implemented;
the camera budget adjustment does not change depth frame limits or establish a 64-second pass.
