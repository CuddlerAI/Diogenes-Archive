# Game runtime gate evidence

`Get-GameRuntimeGateEvidence.ps1` records the point-in-time runtime facts needed before a
game can enter depth discovery:

- executable path and SHA-256;
- loaded module names and the observed D3D11, D3D12, DXGI, or Vulkan runtime;
- anti-cheat module names and matching Windows service/driver state.

The collector is read-only. It does not inject, attach a debugger, read process memory, or
record the process command line. A passing report closes only these runtime gates; it is not
legal permission to inject or collect game content.

Run it from Windows PowerShell after the game has reached the representative single-player
scene and before starting discovery:

```powershell
$game = Get-Process -Id 12345
$expectedExe = 'C:\Program Files (x86)\Steam\steamapps\common\Example Game\Game.exe'
$output = '.\runtime-gate-example-20260810T120000Z.json'

.\Get-GameRuntimeGateEvidence.ps1 `
  -ProcessId $game.Id `
  -ExpectedExecutablePath $expectedExe `
  -ExpectedExecutableSha256 '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' `
  -ExpectedGraphicsApi d3d11 `
  -RequireNoLoadedAntiCheat `
  -OutputPath $output
```

Use the real rendering process, not a launcher. Select `d3d11`, `d3d12`, or `vulkan` only
when that API is an explicit precondition; `any` still records all recognized graphics
modules. The output path must be new: the collector refuses to overwrite evidence.

On 64-bit Windows, the collector detects an i386 game executable from its PE header and
automatically delegates the full read-only collection to the system 32-bit Windows
PowerShell host. This avoids the WOW64 false negative where a 64-bit host sees only bridge
modules instead of the game's 32-bit D3D modules. The report records both the collector
process architecture and target PE machine.

Exit code `0` means all requested gates passed. Exit code `2` means collection was incomplete
or an identity, graphics API, or anti-cheat gate failed. Preserve blocked reports as evidence;
do not weaken the requested gates to obtain a passing result. Module, service, or driver
enumeration failures are recorded in the JSON and fail the report closed.

## Steam D6 installation gate

Before authoring a new Steam catalog, run the repository-level read-only installation probe.
It accepts only a unique manifest with `StateFlags=4`, non-zero `buildid` and `SizeOnDisk`,
and an installation under that library's explicit `steamapps/common` directory. It never
falls back to `steamapps/downloading`; bounded metadata files, a single-component install name,
and a reparse-point-free `steamapps/common` ancestor/install tree are required. Executable
candidates and hashes appear only after the manifest and installed executable metadata remain
stable across two observations.

```powershell
# Current quick status (does not hash even if the game becomes ready):
powershell -NoProfile -ExecutionPolicy Bypass -File ..\Test-SteamD6InstallGate.ps1 `
  -AppId 911400 -StabilitySeconds 10 -SkipExecutableHashes
powershell -NoProfile -ExecutionPolicy Bypass -File ..\Test-SteamD6InstallGate.ps1 `
  -AppId 360430 -StabilitySeconds 10 -SkipExecutableHashes

# Once StateFlags/buildid/SizeOnDisk pass, omit -SkipExecutableHashes. The command waits for
# the default 10-second stability observation and hashes only stable installed candidates:
powershell -NoProfile -ExecutionPolicy Bypass -File ..\Test-SteamD6InstallGate.ps1 -AppId 911400
powershell -NoProfile -ExecutionPolicy Bypass -File ..\Test-SteamD6InstallGate.ps1 -AppId 360430

# Negative regression coverage includes downloading/staging bytes, oversized metadata, path
# traversal, ancestor/install-tree NTFS reparse points, and hashing-time tree changes:
powershell -NoProfile -ExecutionPolicy Bypass -File ..\Test-SteamD6InstallGate.ps1 -SelfTest
```

A passing result is not an authored catalog. A human still has to identify the real shipping
process, verify the window class across two launches, confirm 1920x1080 SDR windowed/borderless
settings, and read the default keyboard/mouse mapping from the same installed build. Follow
`docs/51-annotator-codex-new-game-d6-runbook.md` for those gates. The client build-time
`catalog_gen.rs` embeds already-authored bundles; it is not a five-file D6 authoring generator.
The console report contains local absolute paths for operator use; do not copy those paths into
a catalog bundle or checked-in evidence document.

For a game that has passed those identity gates but has no DepthProfile or CameraBinding,
`Set-GenericD3D11DsvDiscovery.ps1` can arm one bounded D3D11 DSV inventory after consuming a new
passing `game-runtime-gate-evidence/v1` report and an exact-hash, fresh
`generic-d3d11-representative-scene-attestation/v1`. The operator must visually confirm the real
offline single-player representative scene and explicitly create that file with
`New-GenericD3D11RepresentativeSceneAttestation.ps1`; the helper cannot recognize gameplay. It
binds the discovery ID/output leaf, runtime evidence hash, process instance, target window class/client size, scene enum,
observation time and a random nonce with create-new semantics. Arm first selects an exact game/AppID/build/EXE and
capability row from `config/generic-d3d11-dsv-discovery-targets.json`, reconciles that row against
the shipped, rights, capability and final-classification ledgers, then reruns the D6 and runtime
protection gates. It refuses an existing fixed probe config or evidence output, and never launches
or injects a game itself. The renderer binding includes the exact evidence path/locator, and the
arm runtime report must still identify the live PID's exact process creation time. Validate this
control plane with
`Test-GenericD3D11DsvDiscoveryTargets.ps1 -SelfTest` and then without arguments.
Injection must then use `Invoke-GenericD3D11DsvInjection.ps1`; direct `dm-depth-inject` use is not
supported for this lane. The wrapper writes a second create-new, extremely fresh no-anti-cheat
runtime report for the same PID/start/path/hash. The complete armed probe config is also SHA-256-bound,
and embeds the original attestation hash, so changing either attestation or even a valid discovery
limit after Arm is rejected before injection. After the fresh runtime gate, the wrapper rechecks
the attestation bytes/freshness/live window and complete config, and repeats both checks immediately
before injection. It freezes the exact scene, arm-runtime, fresh-runtime and armed-config bytes in
an unrecognized `.pending-*` sibling directory, keeps the verified fixed config open without
write/delete sharing across the irreversible call, reasserts that the probe-owned output path does
not yet exist, and invokes the hash-bound injector with the process-creation FILETIME pinned. The
injector's exit `0` is admitted only when stdout is exactly one canonical
`loaded_probe_sha256=sha256:<lowercase hash>` line matching the selected DLL and the live process
instance still matches afterward.

Only then does the wrapper write `receipt.json` last and atomically rename the sibling to
`<OutputLeaf>-injection-receipt`. The receipt and its four local-only source snapshots are
create-new, bounded, flushed and hash-bound; raw snapshots have `publicationAllowed=false`.
Every snapshot locator is explicitly scoped to the receipt bundle and is a single safe leaf, not
an evidence-root-relative or host path. The receipt itself is also `publicationAllowed=false`.
The receipt schema is `dm-generic-d3d11-dsv-injection-receipt/v1`, and its status means only
`guarded_module_load_succeeded`. Because probe initialization is asynchronous after
`LoadLibraryW`, all probe-initialization, discovery-start, recording, deliverability, profile and
selection-binding flags remain false. The wrapper never creates, waits for, or interprets the
probe output directory or a discovery report. Validate a published bundle with
`Test-GenericD3D11DsvInjectionReceipt.ps1`, passing `EvidenceRoot`, `BundleLeaf`, and the expected
receipt SHA-256; this still does not prove initialization or discovery completion.
The wrapper exposes no resident-probe reuse override. Run its independent
negative suites with `Test-GenericD3D11RepresentativeSceneAttestation.ps1 -SelfTest`,
`New-GenericD3D11RepresentativeSceneAttestation.ps1 -SelfTest`,
`Test-GenericD3D11DsvInjectionReceipt.ps1 -SelfTest`, and the wrapper's `-SelfTest`.
This local operator assertion prevents accidental menu-page use in the supported workflow; it is
not visual proof and cannot stop a same-user operator from lying or deliberately invoking the raw
injector, which remains unsupported. The corresponding exact-instance command and seal/validation flow
are documented in
`docs/160-ac3-generic-d3d11-dsv-discovery-bringup.md`. This path is discovery-only and cannot emit
Ready, a profile, a binding, or a catalog/capability promotion. Its current v3 report keeps a bounded
twelve-pass ledger per candidate/Present; `depth-lab --features evidence-authoring`
`dsv-discovery-pass-summary` can bind those report bytes and expose the exact
`unbind_from_end` histogram without selecting a candidate or ordinal. Any thirteenth pass is a
durable overflow blocker rather than silent truncation.

## Generic D3D11 selection-to-camera v3 plan

After two independent DSV runs have been sealed, visually reviewed, and reduced to one strict
`d3d11-dsv-selection-attestation/v1`, use
`Invoke-GenericD3D11CameraDiscovery.ps1 -Plan`. It creates a non-executing
`generic-d3d11-camera-discovery-plan/v1` and three exact
`camera-discovery-config/v3` probe configs in the fixed order `POSE_STATIC`, `POSE_WALK`,
`POSE_TURN`. `-DryRun` performs the same validation without publishing the plan directory. Plan
and output leaves must differ under Windows ordinal-ignore-case, and every draw window must end at
or before draw 512.

The planner and `Test-GenericD3D11CameraDiscovery.ps1` replay the target registry, rights, stable
Steam D6 install, fresh runtime/protection report, fresh representative-scene attestation, and the
selection attestation's complete transitive source chain. The camera scene attestation must be new
and bind the camera discovery ID/output leaf and current process generation; neither an older DSV
scene file nor a DSV injection receipt authorizes camera loading. `depth-lab` must be built with
`selection-validation`, because the PowerShell scripts deliberately do not duplicate the Rust
attestation/report validators.
Every generated config is also passed through the Rust typed config validator before publication,
including any optional `GameCameraBinding`. The plan has a fixed create-new
`camera-discovery-plan.sha256` sidecar. Every consumer checks its exact bytes, exact
`evidenceRoot/planLeaf` parent, phase-config hashes, and a complete reconstruction of all allowed
command argv; changing both a command and the sidecar still fails. Both the typed-config and sealed-
phase Rust validators receive the plan-pinned config SHA-256 and echo `exactConfigSha256` from the
same bytes they parsed, so a config replacement between PowerShell and Rust validation fails closed.
The command list is an audit canary, not a trust bootstrap: never parse an unchecked plan and run
its `executable`/`arguments` to validate itself. Start every run/resume from the repository's fixed
`Test-GenericD3D11CameraDiscovery.ps1` path and explicit plan/depth-lab/report paths; the validator
checks the sidecar and reconstructs the command list before any optional offline reducer runs.

These scripts do not launch or drive a game, synthesize input, inject a DLL, write the fixed
ProgramData probe config, disable protection, or call the raw injector. The generated plan contains
only human action confirmations, exact config/report locators, and validation commands. A separate
approved exact-instance loader/controller must own initial loading and phase config transitions.
Each transition requires an explicit human confirmation, a create-new sealed report, and successful
`camera-discovery-phase-validate`; elapsed time is never evidence that STATIC/WALK/TURN completed.
Failures must not delete evidence or replace another session's config. Before plan, report, or
offline validation, the exact owner must safely retire its disabled fixed ProgramData config; a
remaining fixed config fails closed even after all three reports exist.

Once all three reports exist, run the plan's `validateSealedReports` command. It replays every gate
again and requires all reports to bind the same selection locator/hash, replayed 17-field descriptor,
and comparison reference. The plan's `offlineAnalysis` command always runs through the validating
wrapper. Without an exact comparison camera, `camera-evidence` emits a create-new
`camera-reference-free-candidate-review/v1` and exit `1`; the wrapper accepts that exit only after
checking the fixed human-review-only ceiling and absence of selection, metric, binding, recording,
delivery, and publication authority. With one exact reference, the same reference is embedded in
every phase and offline analysis requires exit `0` plus `camera-evidence-report/v2`. Full AC3 commands
and the remaining live boundary are in `docs/160-ac3-generic-d3d11-dsv-discovery-bringup.md`.

## Rights gate before discovery

`Test-GameDepthRightsGate.ps1` evaluates the machine-readable decisions in
`config/game-depth-rights-gates.json`. Run it for the selected AppID before loading any hook or
probe:

```powershell
.\Test-GameDepthRightsGate.ps1 -AppId 227300
```

Exit code `0` means the selected record has a publisher or counsel approval, all four required
scopes have explicit boundaries, the decision is in force, and an accountable reviewer is
recorded. Exit code `2` is a valid but blocked decision such as `not_requested`, `requested`,
denied, expired, or partial. Exit code `1` means the manifest is invalid. Never use
`-ValidateOnly` as a capture preflight; that switch exists solely for CI and repository review.

`evidence_url` is **optional** per [issue #855 §E.1](https://github.com/DiogenesModel/Diogenes/issues/855):
a missing, `null` or blank link does not block, because the written counsel document is still
being produced and is backfilled for traceability rather than as a precondition. A link that is
present must still be an absolute `http(s)` URL — a broken reference must not be able to pass as
evidence. This changes only whether the link has to exist yet; every other approval fact
(decision, four bounded scopes, reviewer, effective and review dates) still fails closed.

Every real game in the shipped catalog now carries the same unified approval, so the default
gate passes for an ordinary title. The fail-closed semantics are proven by the fixtures in
`-SelfTest`, not by pointing the gate at a real game and expecting it to be blocked.

`-ValidateOnly` additionally reconciles the ledger against
`config/catalogs/shipped-games.json`: every shipped `gameId` must be claimed by exactly one
rights record through that record's `catalog_game_ids`, `synthetic` must never appear, unknown
catalog ids are rejected, and Halo Infinite may not share a record with Halo MCC/Reach. A rights
record that owns no shipped catalog (DragonSword) stays legal and is reported separately instead
of being counted towards the 30.

The tool does not decide legal rights. It makes the written decision fail closed and reproducible.
Run its built-in semantic regression suite with:

```powershell
.\Test-GameDepthRightsGate.ps1 -SelfTest
```

## Final not-adaptable records

The seven observed-build exclusions are normalized in
`config/game-depth-final-classifications.json`. They record executable and Steam manifest
hashes, exclusion codes, official/repository evidence, hashed anti-cheat installation files where
applicable, and explicit reopen conditions. Validate the complete set with:

```powershell
python ..\check-game-depth-final-classifications.py --self-test
```

On the capture host, also rehash the observed installation:

```powershell
python ..\check-game-depth-final-classifications.py --self-test `
  --steamapps-root 'C:\Program Files (x86)\Steam\steamapps'
```

These records are final only for issue #609's current depth/action-camera taxonomy and the
identified installed builds. A listed reopen condition must be supported by new evidence before a
classification changes; no excluded title may be probed or injected merely to see whether the
classification still holds.
