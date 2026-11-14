# Third-party notices and scope

The root Apache-2.0 license covers owner-authored source only. It does not relicense
third-party material. Existing component declarations and source copyright headers are retained.

| Component | Existing declaration / preserved notice |
| --- | --- |
| `DataMiner/crates/windows-capture-wgc` | Fork of windows-capture 2.0.0; MIT; preserve its `LICENSE` and `NOTICE.md` |
| `DataMiner/tools/depth-lab` | Native compression components; preserve `THIRD-PARTY-NOTICES.txt` (libdeflater / libdeflate) |
| `ml/diogenes_wm/wm01/vendor` | Matrix-Game action modules; preserve the provenance and modification record in `README.md`; Apache-2.0 explicitly declared by the official Matrix-Game-3 README and LICENSE at pinned revision `51913b2bfa78a6d54de912b8348b1d318f4d36e2`; preserve its `LICENSE.txt`, `NOTICE` and `UPSTREAM-PROVENANCE.json` |
| `website/fonts` | Inter, Space Grotesk, IBM Plex Mono; preserve their OFL license files |
| `DataMiner/apps/client/public/fonts` | Preserve the shipped OFL license files for the embedded fonts |
| `engine-data/gta-capture/patches/replus-*.patch` | GPL-3.0-only; preserve the pinned upstream license and patch attribution README; excluded from the root Apache grant |
| `engine-data/gta-capture/patches/igcsconnector-v18-evidence-fence.patch` | MIT; preserve the pinned IGCS Connector license and attribution README |
| npm, Rust, and Python dependencies | Obtain through the package manager; their individual licenses remain in force |

Downloaded game/scene assets, datasets, model weights, copied research-paper archives, and
recorded media are excluded from this source archive. Dataset releases have their own terms.
The archive-site posters remain demonstration images and are not covered by a code-license grant.

The Matrix-Game subproject declaration and GTA patch upstream licenses were verified against
their official sources and preserved. This inventory is not an exhaustive dependency license audit.
The Script Hook V SDK/runtime, REPlus/IGCS upstream source trees, ReShade, FFmpeg and GTA assets
are not bundled; obtain them independently under their respective terms.
