# Capture integration patches — separate licenses

These diffs include modifications to third-party source. The root Apache-2.0 license does not
replace their upstream terms. Patch bytes are preserved exactly: build receipts and the patch
application script pin their SHA-256 values.

| Patch | Source / applicable license |
| --- | --- |
| `replus-*.patch` | REPlus at `c6eba57160ca444817b149b3304ed1454e0ba2d9`; GPL-3.0-only |
| `igcsconnector-v18-evidence-fence.patch` | crxhvrd/simplecamera at `13c92ef2565403ffcc2501f779ca7cd935e5d598`, `external/IgcsConnector-main`; MIT |

Preserve `LICENSE-REPlus-GPL-3.0.txt` and `LICENSE-IgcsConnector-MIT.txt` with these patches.
REPlus identifies CoreFX as its author; the IGCS Connector MIT notice names Frans Bouma.
Diogenes authored the capture-evidence integration changes. GPL-covered modifications and their
upstream context remain GPL-covered, rather than being relicensed under the root Apache-2.0 license.

Upstream sources and licenses:

- https://github.com/crxhvrd/REPlus/tree/c6eba57160ca444817b149b3304ed1454e0ba2d9
- https://github.com/crxhvrd/simplecamera/tree/13c92ef2565403ffcc2501f779ca7cd935e5d598

Acquire the exact upstream source independently, then apply the preserved patches using the
provided script. No third-party runtime binaries, Script Hook V SDK, game installation, FFmpeg,
ReShade, or paid game assets are bundled. Their individual distribution/use terms still apply.
