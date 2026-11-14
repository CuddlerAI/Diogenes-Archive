# Native controller

This directory owns the narrow Windows/ScriptHook boundary. The Python package owns
run state, contracts, evidence, media normalization, QC, and packaging.

## Build the dependency-free protocol mock

```powershell
cmake -S native -B native/build
cmake --build native/build --config Release
ctest --test-dir native/build -C Release --output-on-failure
```

## Build the optional ASI after ScriptHook V is installed

```powershell
cmake -S native -B native/build-asi `
  -DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON `
  -DSCRIPT_HOOK_V_SDK_ROOT=C:/path/to/ScriptHookV_SDK
cmake --build native/build-asi --config Release
```

Copy only the produced `DiogenesGtaController.asi` to a dedicated Story Mode game
installation. Never commit or redistribute ScriptHook/GTA binaries from this repository.

The verified build inputs for the 2026-08-25 POC are:

- Script Hook V SDK zip SHA-256 `56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22`;
- RE+ commit `c6eba57160ca444817b149b3304ed1454e0ba2d9` plus
  `../patches/replus-v18-evidence-fence.patch` SHA-256
  `4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa`;
- IgcsConnector fork commit `13c92ef2565403ffcc2501f779ca7cd935e5d598` plus
  `../patches/igcsconnector-v18-evidence-fence.patch` SHA-256
  `5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5`;
- ReShade 6.8.0 full add-on installer SHA-256
  `afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445`.

The patched stack uses version 18 of `Local\\SimpleCameraFxCapture` and adds two explicitly
versioned process-local channels. `Local\\DiogenesGtaExportPlan` v2 carries a seqlock-stable
672-byte header (including the sealed Render.ini digest and normalized output root) followed by
exact 128-byte tape samples; `Local\\DiogenesGtaEditorIdentity` v4 keeps the original prefix and
appends live Editor/menu/config/render truth, the final RE+ camera-writer pose, and the verified
profile/root facts. RE+ independently re-hashes the `Config::renderIniPath` bytes on every tick
(with writes/deletes denied for each read), compares that actual SHA-256 with plan v2, and
publishes the actual digest plus its normalized configured output root. It also publishes its
actual source frame and absolute GTA replay clock; the
observer compares replay-clock deltas with the 30fps grid (a 10-second plan is 300 frames,
0..299, ending at about 9966.667ms). The add-on captures that Present but withholds `ackId`;
the process-lifetime ASI Present callback verifies the pixel-ready request, final writer pose,
one-clip identity and output file, then writes only `observerReleaseId`. The next Present
acknowledges and lets RE+ advance. Unknown ABI versions, clock/frame skew, extra frames, or
missing completion markers fail closed.

Two consequences of that fence are intentional and worth knowing before debugging:

- The patched IgcsConnector withholds `ackId` indefinitely (including on failed captures), so a
  patched add-on **without** the Diogenes observer running will hang any RE+ export forever.
  That is fail-closed by design: the patched stack must never produce unfenced frames.
- If verification fails mid-export, native atomically marks the plan `FAILED`, RE+ cancels only
  the bridge-owned render, and native drains (but never seals) the failed request so the add-on
  cannot deadlock. The failed lane remains unusable and must be retried from a fresh GTA process.
- The main channel recheck has no write-in-progress bit; its safety depends on RE+ honoring
  the strict request→ready→release→ack alternation (`RenderSettleFrames >= 1`, deploy pins 3,
  real preflight enforces it). Do not relax the alternation without adding a real seqlock.

The export command must be issued while still in Story Mode. It seals the replay/tape/player
telemetry, publishes plan state `ARMED`, and retains the session in the process-lifetime adapter.
The operator then enters Editor and opens the exact Export Settings page. The Present callback
authorizes `TRIGGERED` only after two fresh RE+ identities report the real Start Export action
focused with the exact Frames/PNG/30fps/one-sample configuration. It does not send input.
The operator or Computer Use must complete the stock Export → name → confirmation flow.
Do not directly call `Playback::Open` from the pointer heartbeat: the POC-060 comparison found
that this bypassed menu teardown and retained a white pointer in native PNGs, whereas a stock-UI
diagnostic first frame was clean. That diagnostic alone does not prove four-lane QC.
The ARM-to-completion wall-clock budget is 60 minutes, including manual Editor
project creation/loading, the audio pass and frame rendering. The earlier 20-minute
budget could expire during manual setup before any PNG was captured. Expiry still
fails closed with `restart_required`; the separate 30-second failure drain and all
pixel/pose, replay, profile and identity checks are unchanged.
RE+'s final camera writer owns `TRIGGERED -> EXPORTING -> DONE/FAILED`. If ScriptHook
discards the original command fiber in Editor, return to Story and retry the identical
`export_lane`; it finalizes the retained terminal session rather than creating or claiming a
new export. A different lane/tape/replay/run while a session is retained is rejected.

Identity v4 still contains the live montage project name, one-clip count/index, stable raw clip
filename identity, publisher PID, request id, and heartbeat. The ASI compares it with the
original Rockstar `.clip` recorded and SHA-sealed by that same GTA process; lane 4 seals four
distinct session bindings only after every frame agrees. This avoids treating a caller-authored
replay hash as proof and does not depend on reverse-engineering the private project file format.

The initial ASI intentionally returns `adapter_ready=false`. That flag is changed only
after the installed Legacy build proves all three bindings:

1. Rockstar Editor clip record/seal;
2. RE+ exact fixed-step lane export;
3. protocol-v18 pixel-ready/add-on-ready-generation/final-rendered-pose/release/ack fence with per-frame RGB and controlled-entity hashing.

This prevents a protocol-only build from being mistaken for a real capture adapter.
