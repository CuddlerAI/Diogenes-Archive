The third-person controller candidate successfully compiled and linked with MSVC x64 Release against the existing SHA-pinned ScriptHook SDK. The portable recording-camera CTest passed 1/1. The ScriptHook adapter was compiled; it has not been exercised in GTA.

Artifact: `build/Release/DiogenesGtaController.asi`

- Bytes: 546816
- SHA-256: `fbe39574037998284cf85eba34e8114e5061dc95b91594cc048f39ef26fde4da`
- Build receipt SHA-256: `40fbb44734def204a1596e276b3beb6c8ed8527b4d046b0fc477973689a5b66b`
- SDK archive SHA-256: `56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22`

The SDK was recovered by the parent task from the SDK installer source used by ScriptHookVDotNet and matched the pre-existing approved SDK hash byte-for-byte. This did not change the SDK pin. The candidate was built in a fresh build directory from the portable-tested source snapshot; all 31 source/fixture hashes and byte lengths were independently checked again after the build.

`controller-staging-build-receipt.json` binds the SDK compiler inputs, source snapshot, actual command arguments, logs and artifact. It deliberately records `deployed=false`, `runtime_tested=false`, and `capture_build_receipt_eligible=false`. This is a controller-only working-tree candidate; the existing full-stack build receipt format requires committed clean native source and three real rebuild invocations. No existing manifest, preflight rule, installed component or original receipt was changed by this staging work. See the sibling `controller-third-person-asi-staging/RECEIPT-CONTINUATION.md` for the precise remaining provenance differences.

Camera preparation and the recording guard check the native follow-ped mode and fail on drift. These checks do not establish rendered character visibility. Any future runtime validation must inspect real PNG pixels before committing to all four exports.
