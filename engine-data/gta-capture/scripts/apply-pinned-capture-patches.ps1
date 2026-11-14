param(
    [Parameter(Mandatory = $true)][string]$RePlusSource,
    [Parameter(Mandatory = $true)][string]$SimpleCameraSource
)

$ErrorActionPreference = "Stop"
$rePlusCommit = "c6eba57160ca444817b149b3304ed1454e0ba2d9"
$simpleCameraCommit = "13c92ef2565403ffcc2501f779ca7cd935e5d598"
$patchRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\patches")).Path

function Apply-PinnedPatch {
    param(
        [string]$Source,
        [string]$ExpectedCommit,
        [string]$PatchName,
        [string]$ExpectedPatchSha256
    )
    $root = (Resolve-Path -LiteralPath $Source).Path
    $actualCommit = (& git -C $root rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $actualCommit -ne $ExpectedCommit) {
        throw "$PatchName requires source commit $ExpectedCommit; found $actualCommit"
    }
    $patch = Join-Path $patchRoot $PatchName
    $actualPatchSha256 = (Get-FileHash -LiteralPath $patch -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualPatchSha256 -ne $ExpectedPatchSha256) {
        throw "$PatchName SHA-256 mismatch: expected $ExpectedPatchSha256, found $actualPatchSha256"
    }
    if ($PatchName -eq "replus-v18-evidence-fence.patch" -and
        (Test-Path -LiteralPath (Join-Path $root "src/capture/diogenesbridge.cpp")) -and
        (Test-Path -LiteralPath (Join-Path $root "src/capture/diogenesbridge.h"))) {
        # Make an already-applied patch with new files visible to reverse-check.
        & git -C $root add -N -- src/capture/diogenesbridge.cpp src/capture/diogenesbridge.h
        if ($LASTEXITCODE -ne 0) { throw "cannot register existing RE+ bridge sources" }
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & git -C $root apply --reverse --check --whitespace=nowarn $patch 2>$null
    $reverseExit = $LASTEXITCODE
    $ErrorActionPreference = $previousErrorAction
    if ($reverseExit -eq 0) {
        if ($PatchName -eq "replus-v18-evidence-fence.patch") {
            & git -C $root add -N -- src/capture/diogenesbridge.cpp src/capture/diogenesbridge.h
            if ($LASTEXITCODE -ne 0) { throw "cannot register RE+ bridge sources for diff sealing" }
        }
        Write-Host "$PatchName is already applied"
        return
    }
    $ErrorActionPreference = "Continue"
    & git -C $root apply --check --whitespace=nowarn $patch
    $checkExit = $LASTEXITCODE
    $ErrorActionPreference = $previousErrorAction
    if ($checkExit -ne 0) { throw "$PatchName does not apply cleanly" }
    & git -C $root apply --whitespace=nowarn $patch
    if ($LASTEXITCODE -ne 0) { throw "$PatchName failed to apply" }
    if ($PatchName -eq "replus-v18-evidence-fence.patch") {
        # The approved patch adds two Diogenes-owned source files. Intent-to-add
        # keeps them in `git diff HEAD --binary` without committing third-party
        # source; the build receipt can then compare the exact approved diff.
        & git -C $root add -N -- src/capture/diogenesbridge.cpp src/capture/diogenesbridge.h
        if ($LASTEXITCODE -ne 0) { throw "cannot register RE+ bridge sources for diff sealing" }
    }
    Write-Host "Applied $PatchName to $actualCommit"
}

Apply-PinnedPatch $RePlusSource $rePlusCommit "replus-v18-evidence-fence.patch" `
    "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa"
Apply-PinnedPatch $SimpleCameraSource $simpleCameraCommit "igcsconnector-v18-evidence-fence.patch" `
    "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5"
