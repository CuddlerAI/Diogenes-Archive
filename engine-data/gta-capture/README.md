# GTA Legacy capture and MC4 source archive

This directory contains the project's own Python capture orchestration, C++ controller and mock,
worldline/camera planning, delivery validators, scenario text, PowerShell helpers, and tests.
It does not include game recordings, third-party SDKs, game assets, or runtime binaries.
The only `.bin` file is the small synthetic golden fixture for the camera-tape wire format.

## Source layout

- `gta_capture/`: protocol, policy, stage/take/worldline orchestration, camera tapes and validators.
- `native/`: C++ controller, portable mock/self-tests and optional Windows Script Hook V adapter.
- `schemas/`, `examples/`: authored formats/scenario text and calibration examples.
- `tests/`, `tools/`: synthetic contract tests, geometry/route checks and offline inspection tools.
- `scripts/`: local build/install, dependency-pinning and operator helpers. Cloud VM provisioning
  scripts and private capture-machine handoffs are omitted. Remote helpers require an explicit
  `MC4_HOST` belonging to the operator.
- `patches/`: unchanged integration diffs with GPL-3.0-only/MIT exceptions, explained in their README.

## Portable checks

Run from this directory. These targets do not require GTA or Script Hook V:

```bash
python3 -m unittest discover -s tests -p test_native_tape_golden.py
cmake -S native -B build
cmake --build build
ctest --test-dir build --output-on-failure
```

The full Python test suite can require additional scientific/image packages used by the related
QC/trajectory modules. Native tests check source contracts; they do not prove a game capture pass.

## Windows capture adapter and external dependencies

The actual ASI adapter is Windows-only and opt-in. Obtain the required Script Hook V SDK/runtime
from its vendor yourself and pass `-DDIOGENES_BUILD_SCRIPT_HOOK_V_ASI=ON` plus
`-DSCRIPT_HOOK_V_SDK_ROOT=<your-sdk-root>` to CMake. `native/README.md` describes the protocol and
pinned dependency versions. REPlus and the IGCS Connector fork are external sources; their exact
commits and unchanged patch hashes remain in the example manifest and build-receipt code.
Missing SDKs, binaries, evidence or receipts must remain a failure, not be replaced with dummy
certification. No game, VM, recording, export or installation is started by this archive.

Existing story-mode-only, non-trainable, version/hash, calibration and evidence gates are retained.
The source does not grant rights to GTA content or make an old recording trainable. Consult the
per-component licenses and `patches/README.md`; the root code license does not relicense upstream
GPL/MIT dependencies or game/SDK assets.
