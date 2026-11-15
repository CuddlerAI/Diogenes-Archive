This directory is a source candidate and portable-test record. It is not a capture build receipt or a deployable GTA stack.

The source snapshot selects medium third-person follow-ped mode before scenario completion and again after recording warmup. Preparation requires two consecutive game-frame observations of the active follow-ped camera in mode 1, within 60 game frames. During recording, an observed mode/camera change causes the clip to be discarded. Probe and recording results expose the observed mode; pixel visibility remains explicitly unverified.

The MSVC Release build of `gta_recording_camera_selftest` succeeded. CTest ran `gta_recording_camera_contract`: 1/1 passed. It covers a setter that has no effect, delayed/transient camera changes, loss of Story Mode before another native observation, inactive follow-ped cameras and unsupported camera modes. `step-1.log`, `step-2.log` and `step-3.log` contain the actual invocation output. The complete copied source inputs and hashes are recorded in `staging-receipt.json`.

The ScriptHook adapter and ASI target were not compiled or exercised in GTA. Existing local SDK directories were empty, and the official SDK link redirected to HTML. No replacement SDK or runtime was obtained. There is no `DiogenesGtaController.asi` artifact in this candidate.

The installed capture stack, original build receipt, dependency manifests, replay clips and live GTA process remain unchanged. The copied parent receipt retains its original SHA. This new source-candidate schema is deliberately not eligible for real preflight: the existing capture receipt writer requires committed, clean native source and a complete approved rebuild. No preflight acceptance rule was changed. Do not substitute this file or any portable test executable into a real dependency manifest.

The earlier sibling staging directory contains an interrupted reporting attempt caused by console text encoding; this v2 directory contains the successful complete portable-test receipt. Neither directory contains an ASI build or deployment.
