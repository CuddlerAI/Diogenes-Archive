# Historical capture source snapshots

This directory preserves historical GTA capture/controller variants, modified Rockstar Editor+ (REPlus) source trees, experiment helpers and patches, plus the owner-authored ETS2 telemetry recorder. The GTA paths retain the labels from a 2026-09-22 handoff archive. They are snapshots of experiments, including failed and superseded attempts; labels and historical comments are not new quality, delivery or production-certification claims.

The source manifest covers **895 archived source/attribution files (16,220,753 bytes; 256 distinct public SHA-256 values)**. Multiple versions intentionally retain common files so each archived source tree remains understandable. Seven REPlus variant trees retain their original GPL license; their bundled MinHook/HDE licenses and author lists are also present. The original private copies remain available to the archive owner. No original Git history is imported.

## Finding and verifying a version

- `gta/2026-09-22/mc10/` and `mc2/` contain named controller and REPlus variants and their historical helpers.
- `gta/2026-09-22/capture-gta/` contains additional capture-integration patches and experiment scripts.
- `ets2-telemetry/TelemetryRecorder.cs` is the historical owner-authored recorder. Its upstream dependency is [nlhans/ets2-sdk-plugin](https://github.com/nlhans/ets2-sdk-plugin/tree/d7216cb16c178be34feb04d9d6f67dd8e71ab8de); the downloaded SDK clone is not bundled here.
- `SOURCE_MANIFEST.jsonl` lists each archived logical path, original and public byte counts/SHA-256 values, privacy replacements, and license scope.
- `VERSIONS.json` groups the files by artifact label and records a deterministic file-list hash. These hashes identify these exact snapshots, not a claimed upstream Git revision.
- `PATCH_LICENSES.json` records every patch, its target paths and applicable license. All 26 archived patches/diffs retain their original bytes.

Eleven helper files replace twelve occurrences of a private home directory with an explicit `researcher` placeholder. Their original and new SHA-256 values are recorded. All other archived source/attribution files retain their original bytes. Original historical hash literals inside scripts and reports are retained as historical provenance; use the public manifest to verify these distributed copies. Public copyright and upstream author attribution are preserved.

## Licenses and attribution

The root repository Apache-2.0 license applies to owner-authored controllers, experiment code and owner-only patches. It does **not** relicense the following third-party code or derivative patches:

| Material | Applicable terms |
|---|---|
| Seven `replus-*/source/` trees and REPlus patches | GPL-3.0-only; retain each tree's original `LICENSE`, copyright headers, modification context and source |
| `source/src/vendor/minhook/` within those trees | BSD-2-Clause notices, including the bundled HDE32/HDE64 terms; retain `LICENSE.txt` and `AUTHORS.txt` |
| `capture-gta/igcs-formal-diff-poc056.patch` | IGCS Connector MIT terms; retain `licenses/LICENSE-IgcsConnector-MIT.txt` |
| Controller/integration-only patches | Owner-authored Apache-2.0, as enumerated in `PATCH_LICENSES.json` |

REPlus identifies CoreFX as its upstream author. Diogenes authored the capture integration modifications. The preserved REPlus GPL license exactly matches the verified reference at [REPlus c6eba57160ca444817b149b3304ed1454e0ba2d9](https://github.com/crxhvrd/REPlus/tree/c6eba57160ca444817b149b3304ed1454e0ba2d9). IGCS licensing is retained from [simplecamera 13c92ef2565403ffcc2501f779ca7cd935e5d598](https://github.com/crxhvrd/simplecamera/tree/13c92ef2565403ffcc2501f779ca7cd935e5d598). These references establish retained licensing/provenance references; they do not assert that all historical snapshot contents equal one upstream commit.

## Build and historical limits

Source CMakeLists, compiler resources and source-level self-tests are retained. Generated build trees/project caches, executable/runtime binaries, transfer logs and generated media-review HTML are outside this source release. Script Hook V SDK/runtime, GTA installations, ReShade, FFmpeg, the full IGCS upstream tree and paid game assets must be obtained separately under their own terms.

Upstream README files are preserved as historical source documentation. References in them to “bundled” binaries describe upstream releases; those binaries are absent from this source archive. Old experiment scripts refer to retired paths/resources and are not a current deployment service. The closeout review parsed all 117 archived Python files and verified every source/public SHA-256; it did not rebuild or rerun these historical Windows capture experiments. Historical QC failures and non-trainable or non-deliverable states remain unchanged.
