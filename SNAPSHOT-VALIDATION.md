# Snapshot validation

This archive preserves source and selected test fixtures. It does not claim a production release
or a successful game capture.
Only the following checks have been performed:

- Cargo workspace metadata resolved offline using the installed stable toolchain: all 17 members
  and their path dependencies are present (`cargo metadata --no-deps --offline`).
- All literal Rust `include_str!` and `include_bytes!` file references resolve.
- 609 literal JS/TS relative import references resolve. The six pnpm application manifests and
  workspace lockfile are present. `dist/` files are expected build outputs, not missing source.
- 450 Python files parse successfully. Both pyproject package declarations resolve to included
  source directories. Five small engine-data QC/schema fixtures are included.
- GTA/MC4: 198 of 208 tracked files are included. The 10 exclusions are private operational
  handoffs/runbooks/instructions and cloud VM provisioning scripts. All 24 CMake-referenced
  source files exist. The six upstream integration patches and golden tape fixture bytes are
  unchanged; 2 golden wire-format tests and 32 stage-group tests pass locally.
- Matrix-Game vendor licensing is resolved against the official Matrix-Game-3 subproject README
  and LICENSE at revision `51913b2bfa78a6d54de912b8348b1d318f4d36e2`; code comparison matches the
  documented import/CPU-device adaptations. Dedicated license, notice, and provenance are included.
- The archive website's assets, internal anchors and 23 historical redirects were checked. Its
  desktop and 390px mobile layouts were visually inspected and both public HF videos played.
- Secret-pattern scans found only recognized metric names and test fixtures; no live credential was confirmed. This is not a complete rights/privacy audit.

## Evidence and fixture handling

Four files excluded by the initial documentation filter were required by compile-time Rust
includes. Their handling is explicit:

- CP2077 runtime D6 observation: original bytes retained; it contains game build/runtime facts,
  with no device/session/account identifier found in its field inventory.
- Fallout 4 render-camera evidence: only validator-required technical fields are retained.
  `_archive_note` and `_archive_source_sha256` identify this as a derived subset, not the original
  evidence file. Session/run IDs, adapter diagnostics and unrelated artifact hashes were removed.
  Every field consumed by the gate remains identical to the source. The referenced camera-binding
  hash still matches the unchanged binding bytes. No pass/fail, authorization or certification
  flag was added, changed or relaxed. The original full-file hash is not asserted for the subset.
- Two Fallout 4 geometry fixtures: all original row positions and stage/slot/offset/present-sequence
  fields are retained, while buffer addresses and unused matrices are removed. The same 384
  stable samples are selected by the existing test. These are numerical test fixtures, not a
  substitute for full recording evidence or a new certification.

No reference to the removed original evidence-file hashes was found in the archive source.
Any external workflow requiring the original evidence bytes/hashes is outside this snapshot and
cannot be reproduced from the sanitized subsets. No signatures or declared hashes were forged.

## Limits

No dependency installation, full Rust/TypeScript build, full test suite, Windows hardware capture,
Unity project build, GPU training, or production deployment of the complete archived application stack was performed.
The Windows capture code remains Windows-only. Engine samples require their external engine/game
projects and licensed assets; those are not bundled. The optional GTA ASI target requires a
separately obtained Script Hook V SDK. CMake is unavailable in the current host environment, so
its portable native tests were checked structurally but not compiled/executed.
Weights and datasets are separate downloads with separate licenses and provenance.

Personal home paths, private chat IDs, and personal email contacts found by the targeted scan were
replaced with example values. Consent templates therefore have different bytes and must not be
used to validate historical consent hashes. Historical infrastructure deployment guides were
replaced with local source guides. The public archive uses Apache-2.0 for owner-authored code, preserving the third-party
exceptions documented in `THIRD-PARTY-NOTICES.md`.
