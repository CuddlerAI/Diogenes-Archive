param(
    [Parameter(Mandatory = $true)][string]$GameDir,
    [Parameter(Mandatory = $true)][string]$ScriptHookZip,
    [Parameter(Mandatory = $true)][string]$RePlusBuildDir,
    [Parameter(Mandatory = $true)][string]$CaptureAddon,
    [Parameter(Mandatory = $true)][string]$IgcsDofShader,
    [Parameter(Mandatory = $true)][string]$ControllerAsi,
    [Parameter(Mandatory = $true)][string]$ReShadeInstallReceipt,
    [Parameter(Mandatory = $true)][string]$CaptureBuildReceipt,
    [string]$ExpectedScriptHookZipSha256 = `
        "b64c97c3353906f14621e7e9511e4aec2a7d436ecc21ed124d3816585e2e6188",
    [string]$ExpectedGameFileVersion = "1.0.3889.0",
    # Third-party sources are pinned to exact commits, but our own controller is
    # only required to be committed and clean at receipt time. Pass the approved
    # commit here to also pin which controller build may be deployed.
    [string]$ExpectedControllerCommit = "",
    [ValidateSet("ReShade64.asi", "d3d11.dll", "dxgi.dll")]
    [string]$ReShadeModuleName = "ReShade64.asi",
    [string]$RenderOutputFolder = "",
    [switch]$RecoverStaleLock,
    [switch]$AcknowledgeFailedRollback
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "render-profile.ps1")
$resolvedGameDir = (Resolve-Path -LiteralPath $GameDir).Path
$gameRootPrefix = $resolvedGameDir.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
if (((Get-Item -LiteralPath $resolvedGameDir).Attributes -band
        [IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw "GameDir itself may not be a symlink, junction, or reparse point"
}
$ancestor = Split-Path -Parent $resolvedGameDir
while ($ancestor) {
    $ancestorItem = Get-Item -LiteralPath $ancestor -Force
    if (($ancestorItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "GameDir ancestor may not be a symlink, junction, or reparse point: $ancestor"
    }
    $ancestor = Split-Path -Parent $ancestor
}
$failedMarkerPath = Join-Path $resolvedGameDir ".diogenes-deploy-failed.json"
if (Test-Path -LiteralPath $failedMarkerPath -PathType Leaf) {
    if (-not $AcknowledgeFailedRollback) {
        throw ("A previous deployment failed AND could not fully roll back; the game " +
            "directory may be inconsistent. Inspect $failedMarkerPath and the backup " +
            "root it names, restore manually, then rerun with -AcknowledgeFailedRollback")
    }
    Remove-Item -LiteralPath $failedMarkerPath -Force
}

function Resolve-RequiredFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label not found: $Path"
    }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Assert-FileHash([string]$Path, [string]$Expected, [string]$Label) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected.ToLowerInvariant()) {
        throw "$Label SHA-256 mismatch: expected $Expected, found $actual"
    }
}

function Assert-ApprovedReShadeSignature($Signature, $ReceiptInstaller) {
    $thumbprint = if ($Signature.SignerCertificate) {
        $Signature.SignerCertificate.Thumbprint.ToUpperInvariant()
    } else { "" }
    $chain = New-Object Security.Cryptography.X509Certificates.X509Chain
    if ($Signature.SignerCertificate) { $null = $chain.Build($Signature.SignerCertificate) }
    $chainStatus = @($chain.ChainStatus | ForEach-Object { [string]$_.Status })
    $onlyUntrustedRoot = ($chainStatus.Count -eq 1 -and $chainStatus[0] -eq "UntrustedRoot")
    $statusAllowed = ($Signature.Status -eq "Valid" -or
        ($Signature.Status -eq "UnknownError" -and $onlyUntrustedRoot))
    # X509Chain state depends on the machine trust store, so receipt time and
    # deploy time may legitimately differ (e.g. the self-signed root was trusted
    # in between). Require BOTH observations to be independently approved; the
    # actual anchors are the pinned thumbprint and the installer SHA-256.
    $receiptChain = @($ReceiptInstaller.signature_chain_status | ForEach-Object { [string]$_ })
    $receiptOnlyUntrustedRoot = ($receiptChain.Count -eq 1 -and
        $receiptChain[0] -eq "UntrustedRoot")
    $receiptStatusAllowed = ([string]$ReceiptInstaller.signature_status -eq "Valid" -or
        ([string]$ReceiptInstaller.signature_status -eq "UnknownError" -and
            $receiptOnlyUntrustedRoot))
    if (-not $statusAllowed -or -not $receiptStatusAllowed -or
        $thumbprint -ne "589690208A5E52FB96980C4A6698F50ACD47C49F") {
        throw "ReShade installer Authenticode signature/chain is not approved"
    }
}

function Assert-NoReparsePath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    if (-not ($full.Equals($resolvedGameDir, [StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($gameRootPrefix, [StringComparison]::OrdinalIgnoreCase))) {
        throw "Path escaped the game directory: $full"
    }
    if ($full.Equals($resolvedGameDir, [StringComparison]::OrdinalIgnoreCase)) { return }
    $relative = $full.Substring($gameRootPrefix.Length)
    $current = $resolvedGameDir
    foreach ($part in $relative.Split([char[]]@('\', '/'),
            [StringSplitOptions]::RemoveEmptyEntries)) {
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing to traverse reparse point during deployment: $current"
            }
        }
    }
}

function Backup-And-Copy([string]$Source, [string]$Destination, [string]$BackupRoot) {
    $backup = $null
    $destinationFull = [IO.Path]::GetFullPath($Destination)
    if (-not $destinationFull.StartsWith(
            $gameRootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to deploy outside the game directory: $destinationFull"
    }
    Assert-NoReparsePath $destinationFull
    $parent = Split-Path -Parent $Destination
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    Assert-NoReparsePath $destinationFull
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $relative = $destinationFull.Substring($gameRootPrefix.Length)
        $backup = Join-Path $BackupRoot $relative
        Assert-NoReparsePath $backup
        $backupParent = Split-Path -Parent $backup
        if ($backupParent) { New-Item -ItemType Directory -Force -Path $backupParent | Out-Null }
        Assert-NoReparsePath $backup
        Copy-Item -LiteralPath $Destination -Destination $backup
    }
    $script:deployJournal.Add([pscustomobject]@{
        Destination = $destinationFull
        Backup = if ($backup) { $backup } else { $null }
        Existed = [bool]$backup
    }) | Out-Null
    $staging = "$Destination.diogenes-deploying"
    if (Test-Path -LiteralPath $staging) {
        # A pre-planted reparse point here would redirect the staged write.
        $stagingItem = Get-Item -LiteralPath $staging -Force
        if (($stagingItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing a reparse-point staging path: $staging"
        }
        Remove-Item -LiteralPath $staging -Force
    }
    Copy-Item -LiteralPath $Source -Destination $staging -Force
    Assert-NoReparsePath ([IO.Path]::GetFullPath($staging))
    Move-Item -LiteralPath $staging -Destination $Destination -Force
    Assert-FileHash $destinationFull `
        ((Get-FileHash -LiteralPath $Source -Algorithm SHA256).Hash.ToLowerInvariant()) `
        "post-deploy destination"
}

function Assert-BuildArtifact($Record, [string]$Path, [string]$Label) {
    if (-not $Record -or -not ([string]$Record.sha256 -match '^[0-9a-f]{64}$') -or
        -not ($Record.bytes -is [ValueType]) -or [int64]$Record.bytes -le 0) {
        throw "$Label is missing a valid record in the capture build receipt"
    }
    Assert-FileHash $Path ([string]$Record.sha256) $Label
    if ((Get-Item -LiteralPath $Path).Length -ne [int64]$Record.bytes) {
        throw "$Label byte length differs from the capture build receipt"
    }
    $receiptArtifactPath = [IO.Path]::GetFullPath([string]$Record.path)
    if (-not $receiptArtifactPath.Equals($Path, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label path differs from the capture build receipt"
    }
}

function Assert-ReShadePayload([string]$Installer, [string]$Installed, $PayloadRecord) {
    if (-not $PayloadRecord -or $PayloadRecord.archive_entry -ne "ReShade64.dll" -or
        -not ([string]$PayloadRecord.sha256 -match '^[0-9a-f]{64}$') -or
        -not ($PayloadRecord.bytes -is [ValueType]) -or [int64]$PayloadRecord.bytes -le 0) {
        throw "ReShade receipt has no valid embedded payload record"
    }
    $tar = (Get-Command tar.exe -ErrorAction Stop).Source
    $payloadRoot = Join-Path ([IO.Path]::GetTempPath()) `
        ("diogenes-reshade-payload-" + [Guid]::NewGuid().ToString("N"))
    try {
        New-Item -ItemType Directory -Path $payloadRoot | Out-Null
        & $tar -xf $Installer -C $payloadRoot ReShade64.dll
        if ($LASTEXITCODE -ne 0) { throw "cannot extract ReShade64.dll from signed installer" }
        $payload = Resolve-RequiredFile (Join-Path $payloadRoot "ReShade64.dll") `
            "signed installer ReShade64.dll payload"
        Assert-FileHash $payload ([string]$PayloadRecord.sha256) `
            "receipt-bound ReShade64.dll payload"
        Assert-FileHash $Installed ([string]$PayloadRecord.sha256) `
            "installed ReShade dxgi.dll"
        if ((Get-Item $payload).Length -ne [int64]$PayloadRecord.bytes -or
            (Get-Item $Installed).Length -ne [int64]$PayloadRecord.bytes) {
            throw "ReShade payload/installed byte length differs from the receipt"
        }
    } finally {
        if (Test-Path -LiteralPath $payloadRoot -PathType Container) {
            Remove-Item -LiteralPath $payloadRoot -Recurse -Force
        }
    }
}

$gameExe = Resolve-RequiredFile (Join-Path $resolvedGameDir "GTA5.exe") "GTA5.exe"
$gameVersion = [string](Get-Item -LiteralPath $gameExe).VersionInfo.FileVersion
if ($gameVersion.Trim() -ne $ExpectedGameFileVersion) {
    throw "GTA5.exe FileVersion mismatch: expected $ExpectedGameFileVersion, found $gameVersion"
}
$reshadeDll = Resolve-RequiredFile (Join-Path $resolvedGameDir $ReShadeModuleName) `
    "ReShade $ReShadeModuleName (install the official full add-on build first)"
$receiptPath = Resolve-RequiredFile $ReShadeInstallReceipt "ReShade install receipt"
$receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
if ($receipt.schema -ne "dm-gta-reshade-install-receipt/1" -or
    $receipt.installer.sha256 -ne "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445" -or
    $receipt.installer.signer_thumbprint -ne "589690208A5E52FB96980C4A6698F50ACD47C49F") {
    throw "ReShade install receipt does not identify the approved signed 6.8.0 installer"
}
$receiptInstaller = Resolve-RequiredFile ([string]$receipt.installer.path) `
    "receipt-bound ReShade installer"
Assert-FileHash $receiptInstaller ([string]$receipt.installer.sha256) `
    "receipt-bound ReShade installer"
$installerSignature = Get-AuthenticodeSignature -LiteralPath $receiptInstaller
Assert-ApprovedReShadeSignature $installerSignature $receipt.installer
Assert-ReShadePayload $receiptInstaller $reshadeDll $receipt.payload
$receiptGame = [IO.Path]::GetFullPath([string]$receipt.target.path)
$receiptDxgi = [IO.Path]::GetFullPath([string]$receipt.installed_dxgi.path)
if (-not $receiptGame.Equals($gameExe, [StringComparison]::OrdinalIgnoreCase) -or
    -not $receiptDxgi.Equals($reshadeDll, [StringComparison]::OrdinalIgnoreCase)) {
    throw "ReShade install receipt targets another GTA executable or proxy module"
}
Assert-FileHash $gameExe ([string]$receipt.target.sha256) "receipt-bound GTA5.exe"
Assert-FileHash $reshadeDll ([string]$receipt.installed_dxgi.sha256) "receipt-bound ReShade dxgi.dll"
if ([int64]$receipt.installed_dxgi.bytes -ne (Get-Item $reshadeDll).Length -or
    [string]$receipt.installed_dxgi.sha256 -ne [string]$receipt.payload.sha256) {
    throw "ReShade receipt installed module differs from its signed-installer payload"
}
$scriptHookZipPath = Resolve-RequiredFile $ScriptHookZip "Script Hook V runtime zip"
$scriptHookZipSha256 = (Get-FileHash -LiteralPath $scriptHookZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
if ($scriptHookZipSha256 -ne $ExpectedScriptHookZipSha256.ToLowerInvariant()) {
    throw "Script Hook V runtime zip SHA-256 mismatch: $scriptHookZipSha256"
}
$rePlusBuild = (Resolve-Path -LiteralPath $RePlusBuildDir).Path
$rePlusAsi = Resolve-RequiredFile (Join-Path $rePlusBuild "RockstarEditorPlus.asi") `
    "RockstarEditorPlus.asi"
$rePlusIni = Resolve-RequiredFile (Join-Path $rePlusBuild "RockstarEditorPlus.ini") `
    "RockstarEditorPlus.ini"
$renderIniSource = Resolve-RequiredFile (Join-Path $rePlusBuild "Render.ini") "RE+ Render.ini"
$lightsIni = Resolve-RequiredFile (Join-Path $rePlusBuild "Lights.ini") "RE+ Lights.ini"
$settingsExe = Resolve-RequiredFile (Join-Path $rePlusBuild "RE+ Render Settings.exe") `
    "RE+ Render Settings.exe"
$captureAddonPath = Resolve-RequiredFile $CaptureAddon "IgcsConnector.addon64"
$shaderPath = Resolve-RequiredFile $IgcsDofShader "IgcsDof.fx"
$controllerPath = Resolve-RequiredFile $ControllerAsi "DiogenesGtaController.asi"
$buildReceiptPath = Resolve-RequiredFile $CaptureBuildReceipt "capture build receipt"
$buildReceipt = Get-Content -LiteralPath $buildReceiptPath -Raw | ConvertFrom-Json
$approvedSources = [ordered]@{
    replus_commit = "c6eba57160ca444817b149b3304ed1454e0ba2d9"
    replus_patch_sha256 = "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa"
    igcs_fork_commit = "13c92ef2565403ffcc2501f779ca7cd935e5d598"
    igcs_patch_sha256 = "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5"
}
if ($buildReceipt.schema -ne "dm-gta-capture-build-receipt/1" -or
    -not ([string]$buildReceipt.sources.controller_git_commit -match '^[0-9a-f]{40}$') -or
    -not [string]$buildReceipt.toolchain.msbuild_version) {
    throw "capture build receipt schema/source/toolchain is incomplete"
}
if ($ExpectedControllerCommit) {
    if ($ExpectedControllerCommit -notmatch '^[0-9a-f]{40}$') {
        throw "-ExpectedControllerCommit must be a full 40-hex git commit"
    }
    if ([string]$buildReceipt.sources.controller_git_commit -ne $ExpectedControllerCommit) {
        throw ("capture build receipt controller commit " +
            "$([string]$buildReceipt.sources.controller_git_commit) is not the approved " +
            "$ExpectedControllerCommit")
    }
}
foreach ($property in $approvedSources.GetEnumerator()) {
    if ([string]$buildReceipt.sources.($property.Key) -ne [string]$property.Value) {
        throw "capture build receipt source pin mismatch: $($property.Key)"
    }
}
Assert-BuildArtifact $buildReceipt.artifacts.replus_asi $rePlusAsi `
    "receipt-bound RockstarEditorPlus.asi"
Assert-BuildArtifact $buildReceipt.artifacts.replus_capture_addon $captureAddonPath `
    "receipt-bound IgcsConnector.addon64"
Assert-BuildArtifact $buildReceipt.artifacts.diogenes_controller_asi $controllerPath `
    "receipt-bound DiogenesGtaController.asi"
Assert-BuildArtifact $buildReceipt.artifacts.replus_main_ini $rePlusIni `
    "receipt-bound RockstarEditorPlus.ini"
Assert-BuildArtifact $buildReceipt.artifacts.replus_render_ini $renderIniSource `
    "receipt-bound Render.ini"
Assert-BuildArtifact $buildReceipt.artifacts.replus_lights_ini $lightsIni `
    "receipt-bound Lights.ini"
Assert-BuildArtifact $buildReceipt.artifacts.replus_settings_exe $settingsExe `
    "receipt-bound RE+ Render Settings.exe"
Assert-BuildArtifact $buildReceipt.artifacts.igcs_dof_shader $shaderPath `
    "receipt-bound IgcsDof.fx"
Assert-DiogenesRePlusRenderProfile $renderIniSource `
    $buildReceipt.configuration_transforms.replus_render_ini `
    $buildReceipt.artifacts.replus_render_ini $RenderOutputFolder

$allowedAsi = @("RockstarEditorPlus.asi", "DiogenesGtaController.asi")
if ([IO.Path]::GetExtension($ReShadeModuleName).Equals(
        ".asi", [StringComparison]::OrdinalIgnoreCase)) {
    $allowedAsi += $ReShadeModuleName
}
$unexpectedAsi = @(Get-ChildItem -LiteralPath $resolvedGameDir -Filter "*.asi" -File |
    Where-Object { $allowedAsi -notcontains $_.Name } | Select-Object -ExpandProperty FullName)
if ($unexpectedAsi.Count -gt 0) {
    throw "Unapproved ASI modules are present; remove them before capture: $($unexpectedAsi -join ', ')"
}
$unexpectedAddon = @(Get-ChildItem -LiteralPath $resolvedGameDir -Filter "*.addon64" -File |
    Where-Object { $_.Name -ne "IgcsConnector.addon64" } | Select-Object -ExpandProperty FullName)
if ($unexpectedAddon.Count -gt 0) {
    throw "Unapproved ReShade addons are present: $($unexpectedAddon -join ', ')"
}

$deploymentId = (Get-Date -Format "yyyyMMdd-HHmmss") + "-" + [Guid]::NewGuid().ToString("N")
$backupRoot = Join-Path $resolvedGameDir ".diogenes-backup\$deploymentId"
$extractRoot = Join-Path ([IO.Path]::GetTempPath()) "diogenes-scripthook-$deploymentId"
$script:deployJournal = [Collections.Generic.List[object]]::new()
$deployLockPath = Join-Path $resolvedGameDir ".diogenes-deploy.lock"
$deployLock = $null
if (Test-Path -LiteralPath $deployLockPath -PathType Leaf) {
    if (-not $RecoverStaleLock) {
        throw "Deployment lock exists; use -RecoverStaleLock only after confirming no deployment is active"
    }
    try {
        $stale = [IO.File]::Open($deployLockPath, [IO.FileMode]::Open,
            [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $stale.Dispose()
        Remove-Item -LiteralPath $deployLockPath -Force
    } catch {
        throw "Deployment lock is active and cannot be recovered: $deployLockPath"
    }
}
try {
    $deployLock = [IO.File]::Open($deployLockPath, [IO.FileMode]::CreateNew,
                                 [IO.FileAccess]::Write, [IO.FileShare]::None)
    $lockText = [Text.Encoding]::UTF8.GetBytes(
        "pid=$PID created=$((Get-Date).ToUniversalTime().ToString('o'))")
    $deployLock.Write($lockText, 0, $lockText.Length)
    $deployLock.Flush($true)
} catch {
    throw "Another capture-stack deployment is active: $deployLockPath"
}
try {
    New-Item -ItemType Directory -Force -Path $extractRoot | Out-Null
    Expand-Archive -LiteralPath $scriptHookZipPath -DestinationPath $extractRoot
    $scriptHookDll = Resolve-RequiredFile (Join-Path $extractRoot "bin\ScriptHookV.dll") `
        "ScriptHookV.dll in runtime zip"
    $asiLoader = Resolve-RequiredFile (Join-Path $extractRoot "bin\dinput8.dll") `
        "dinput8.dll in runtime zip"

    Backup-And-Copy $scriptHookDll (Join-Path $resolvedGameDir "ScriptHookV.dll") $backupRoot
    Backup-And-Copy $asiLoader (Join-Path $resolvedGameDir "dinput8.dll") $backupRoot
    Backup-And-Copy $rePlusAsi (Join-Path $resolvedGameDir "RockstarEditorPlus.asi") $backupRoot
    Backup-And-Copy $captureAddonPath (Join-Path $resolvedGameDir "IgcsConnector.addon64") $backupRoot
    Backup-And-Copy $controllerPath `
        (Join-Path $resolvedGameDir "DiogenesGtaController.asi") $backupRoot

    $rePlusDir = Join-Path $resolvedGameDir "RockstarEditorPlus"
    Backup-And-Copy $rePlusIni (Join-Path $rePlusDir "RockstarEditorPlus.ini") $backupRoot
    Backup-And-Copy $renderIniSource (Join-Path $rePlusDir "Render.ini") $backupRoot
    Assert-DiogenesRePlusRenderProfile (Join-Path $rePlusDir "Render.ini") `
        $buildReceipt.configuration_transforms.replus_render_ini `
        $buildReceipt.artifacts.replus_render_ini $RenderOutputFolder
    Backup-And-Copy $lightsIni (Join-Path $rePlusDir "Lights.ini") $backupRoot
    Backup-And-Copy $settingsExe (Join-Path $rePlusDir "RE+ Render Settings.exe") $backupRoot
    Backup-And-Copy $shaderPath `
        (Join-Path $resolvedGameDir "reshade-shaders\Shaders\IgcsDof.fx") $backupRoot
    $provenanceDir = Join-Path $resolvedGameDir ".diogenes"
    $deployedBuildReceipt = Join-Path $provenanceDir "capture-build-receipt.json"
    $deployedReShadeReceipt = Join-Path $provenanceDir "reshade-install-receipt.json"
    $deployedReShadeInstaller = Join-Path $provenanceDir "ReShade_Setup_6.8.0_Addon.exe"
    Backup-And-Copy $receiptInstaller $deployedReShadeInstaller $backupRoot
    $localizedReceipt = Join-Path $extractRoot "reshade-install-receipt.json"
    $receipt.installer.path = $deployedReShadeInstaller
    $localizedJson = ($receipt | ConvertTo-Json -Depth 8) + [Environment]::NewLine
    [IO.File]::WriteAllText($localizedReceipt, $localizedJson,
        [Text.UTF8Encoding]::new($false))
    Backup-And-Copy $buildReceiptPath $deployedBuildReceipt $backupRoot
    Backup-And-Copy $localizedReceipt $deployedReShadeReceipt $backupRoot

    Write-Host "Story Mode capture stack deployed to: $resolvedGameDir"
    Write-Host "GTA5.exe: $((Get-FileHash -LiteralPath $gameExe -Algorithm SHA256).Hash.ToLowerInvariant())"
    Write-Host "ReShade: $((Get-FileHash -LiteralPath $reshadeDll -Algorithm SHA256).Hash.ToLowerInvariant())"
    Write-Host "ReShade receipt: $((Get-FileHash -LiteralPath $receiptPath -Algorithm SHA256).Hash.ToLowerInvariant())"
    Write-Host "Capture build receipt: $((Get-FileHash -LiteralPath $buildReceiptPath -Algorithm SHA256).Hash.ToLowerInvariant())"
    Write-Host "Pinned receipts deployed under: $provenanceDir"
    if (Test-Path -LiteralPath $backupRoot) {
        Write-Host "Previous files were backed up under: $backupRoot"
    }
    Write-Host "Do not enter GTA Online. Launch Story Mode only."
} catch {
    $originalError = $_
    $rollbackErrors = [Collections.Generic.List[string]]::new()
    for ($index = $script:deployJournal.Count - 1; $index -ge 0; $index--) {
        try {
            $entry = $script:deployJournal[$index]
            $destinationFull = [IO.Path]::GetFullPath($entry.Destination)
            Assert-NoReparsePath $destinationFull
            $staging = "$destinationFull.diogenes-deploying"
            if (Test-Path -LiteralPath $staging -PathType Leaf) {
                Remove-Item -LiteralPath $staging -Force
            }
            if ($entry.Existed -and (Test-Path -LiteralPath $entry.Backup -PathType Leaf)) {
                Assert-NoReparsePath $entry.Backup
                Copy-Item -LiteralPath $entry.Backup -Destination $destinationFull -Force
            } elseif (Test-Path -LiteralPath $destinationFull -PathType Leaf) {
                Remove-Item -LiteralPath $destinationFull -Force
            }
        } catch {
            $rollbackErrors.Add($_.Exception.Message) | Out-Null
        }
    }
    if ($rollbackErrors.Count -gt 0) {
        # The game directory is now in a half-rolled-back state. Record it so
        # the next deployment refuses to run on top of it silently.
        $marker = [pscustomobject]@{
            schema = "dm-gta-deploy-failed-rollback/1"
            deployment_id = $deploymentId
            at = (Get-Date).ToUniversalTime().ToString('o')
            backup_root = $backupRoot
            original_error = [string]$originalError.Exception.Message
            rollback_errors = @($rollbackErrors)
        }
        try {
            [IO.File]::WriteAllText($failedMarkerPath,
                (($marker | ConvertTo-Json -Depth 4) + [Environment]::NewLine),
                [Text.UTF8Encoding]::new($false))
        } catch {
            $rollbackErrors.Add("cannot write failed-rollback marker: " +
                $_.Exception.Message) | Out-Null
        }
        throw ("$($originalError.Exception.Message); rollback errors: " +
            "$($rollbackErrors -join ' | '); state recorded in $failedMarkerPath")
    }
    throw $originalError
} finally {
    if (Test-Path -LiteralPath $extractRoot) {
        Remove-Item -LiteralPath $extractRoot -Recurse -Force
    }
    if ($deployLock) { $deployLock.Dispose() }
    if (Test-Path -LiteralPath $deployLockPath -PathType Leaf) {
        Remove-Item -LiteralPath $deployLockPath -Force
    }
}
