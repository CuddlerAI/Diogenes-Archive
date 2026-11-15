# Diogenes — source archive

Diogenes is an archived collection of gameplay capture, data processing, geometry inspection,
and model research tools. Active project development and hosted collection services have ended.

This is a clean source archive published at [CuddlerAI/Diogenes-Archive](https://github.com/CuddlerAI/Diogenes-Archive).
Owner-authored code is licensed under Apache-2.0, with the third-party exceptions documented below.
The original private repository and its history are not part of this archive.

## Included components

| Directory | Contents |
| --- | --- |
| `DataMiner/` | Windows capture client, Rust libraries, TypeScript service source, contracts, and offline tools |
| `engine-data/` | Engine capture sources, capture orchestration, data delivery, QC, schemas, and trajectory generation |
| `ml/` | Visual, inverse-dynamics, world-model research, Player Model source, and synthetic tests |
| `data_demo/` | Annotation and dataset preparation scripts; recorded media are excluded |
| `tools/` | Selected offline conversion, visualization, and validation tools |
| `website/` | Static archive site with links to GitHub and Hugging Face |

This archive excludes the original Git history, private operations and business records,
cloud deployment automation, personal/device diagnostic reports, downloaded weights,
recorded datasets, and third-party scene assets. The GTA/MC4 source package is included with its own-source controller, tools, scenario text,
and unchanged integration patches; upstream GPL/MIT exceptions are explicitly documented. Some archived scripts describe now-retired cloud
services; configure your own resources before using them.

## Build and test entry points

The Windows client requires Windows, the Rust toolchain specified by `DataMiner/rust-toolchain.toml`,
MSVC/Windows SDK, the Node/pnpm versions in `DataMiner/package.json`, and NVIDIA hardware for
real capture. Static checks or synthetic tests do not verify game capture behavior.

```powershell
cd DataMiner
pnpm install --frozen-lockfile
pnpm typecheck:workspace
pnpm build
cargo test --workspace --all-targets
```

For the Python research packages, install only the extras needed by your task:

```bash
python -m pip install -e 'ml[dev]'
python -m pytest ml/tests
python -m pip install -e 'ml/player[dev]'
python -m pytest ml/player/tests
```

Some research tests need extra packages or external datasets; this snapshot does not include
model weights or datasets. The contracts keep existing trainability and provenance flags intact.

The static site can be previewed from this directory with:

```bash
python3 tools/preview.py website 8788
```

The website's manual container build files are included. No CI jobs or deployment credentials
are provided. See `SNAPSHOT-VALIDATION.md` for the checks actually performed on this snapshot.

## Data and licensing

Dataset availability and conditions are listed at [Hugging Face / DiogenesLab](https://huggingface.co/DiogenesLab).
The root Apache-2.0 license does not override the GPL-3.0-only/MIT licenses of GTA integration patches.
The source-code license does **not** license gameplay footage, recordings, model weights, datasets,
game assets, third-party software, trademarks, or private material. Each retains its own terms.

Owner-authored code is licensed under Apache-2.0 in `LICENSE`. Existing third-party copyrights,
licenses, and NOTICE files take precedence for their respective components and must be preserved.
See `THIRD-PARTY-NOTICES.md` and `NOTICE` before redistribution.

## Historical capture source

[Historical capture snapshots](historical-capture-source/README.md) preserve additional GTA controller and REPlus variants, experiment patches, and the owner-authored ETS2 telemetry recorder. Third-party GPL, BSD and MIT notices remain attached to their components; the root Apache-2.0 grant applies only to owner-authored material. These are historical experiments, including superseded and failed attempts.
