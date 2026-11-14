param(
    [Parameter(Mandatory = $true)][string]$GameDir,
    [Parameter(Mandatory = $true)][string]$Output,
    [Parameter(Mandatory = $true)][string]$ReShadeInstallReceipt,
    [Parameter(Mandatory = $true)][string]$CaptureBuildReceipt,
    [string]$RePlusAsi = "",
    [string]$CaptureAddon = "",
    [string]$ReShadeDll = "",
    [string]$RenderIni = "",
    [string]$ControllerAsi = "",
    [string]$GtaSettingsXml = "",
    [string]$ProjectionEvidence = ""
)

$ErrorActionPreference = "Stop"
$moduleRoot = Split-Path -Parent $PSScriptRoot
$template = Join-Path $moduleRoot "examples/dependency-manifest.local.example.json"
$resolvedGameDir = (Resolve-Path -LiteralPath $GameDir).Path

function Resolve-RequiredFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label not found: $Path"
    }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Pin-File($Node, [string]$Path) {
    $Node.path = $Path
    $Node.sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-FileHash([string]$Path, [string]$Expected, [string]$Label) {
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
    if ($actual -ne $Expected) { throw "$Label SHA-256 mismatch: expected $Expected, found $actual" }
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
    if (-not $statusAllowed -or
        $thumbprint -ne "589690208A5E52FB96980C4A6698F50ACD47C49F" -or
        [string]$ReceiptInstaller.signature_status -ne [string]$Signature.Status -or
        (@($ReceiptInstaller.signature_chain_status) -join ',') -ne ($chainStatus -join ',')) {
        throw "ReShade installer Authenticode signature/chain is not approved"
    }
}

function Assert-BuildArtifact($Record, [string]$Path, [string]$Label) {
    if (-not $Record -or -not ([string]$Record.sha256 -match '^[0-9a-f]{64}$') -or
        -not ($Record.bytes -is [ValueType]) -or [int64]$Record.bytes -le 0) {
        throw "$Label is missing a valid capture build record"
    }
    Assert-FileHash $Path ([string]$Record.sha256) $Label
    if ((Get-Item -LiteralPath $Path).Length -ne [int64]$Record.bytes) {
        throw "$Label byte length differs from the capture build receipt"
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

$manifest = Get-Content -LiteralPath $template -Raw | ConvertFrom-Json
$gameExe = Resolve-RequiredFile (Join-Path $resolvedGameDir "GTA5.exe") "GTA5.exe"
Pin-File $manifest.game $gameExe
$manifest.game | Add-Member -NotePropertyName actual_file_version `
    -NotePropertyValue (Get-Item -LiteralPath $gameExe).VersionInfo.FileVersion -Force
Pin-File $manifest.components.scripthook_dll `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "ScriptHookV.dll") "ScriptHookV.dll")
Pin-File $manifest.components.asi_loader `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "dinput8.dll") "ASI loader")

if (-not $RePlusAsi) { $RePlusAsi = Join-Path $resolvedGameDir "RockstarEditorPlus.asi" }
if (-not $CaptureAddon) { $CaptureAddon = Join-Path $resolvedGameDir "IgcsConnector.addon64" }
if (-not $ReShadeDll) { $ReShadeDll = Join-Path $resolvedGameDir "ReShade64.asi" }
if (-not $RenderIni) {
    $RenderIni = Join-Path $resolvedGameDir "RockstarEditorPlus\Render.ini"
}
if (-not $ControllerAsi) { $ControllerAsi = Join-Path $resolvedGameDir "DiogenesGtaController.asi" }
$resolvedRePlus = Resolve-RequiredFile $RePlusAsi "RockstarEditorPlus.asi"
$resolvedAddon = Resolve-RequiredFile $CaptureAddon "IgcsConnector.addon64"
$resolvedController = Resolve-RequiredFile $ControllerAsi "DiogenesGtaController.asi"
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
foreach ($property in $approvedSources.GetEnumerator()) {
    if ([string]$buildReceipt.sources.($property.Key) -ne [string]$property.Value) {
        throw "capture build receipt source pin mismatch: $($property.Key)"
    }
}
Assert-BuildArtifact $buildReceipt.artifacts.replus_asi $resolvedRePlus `
    "receipt-bound RockstarEditorPlus.asi"
Assert-BuildArtifact $buildReceipt.artifacts.replus_capture_addon $resolvedAddon `
    "receipt-bound IgcsConnector.addon64"
Assert-BuildArtifact $buildReceipt.artifacts.diogenes_controller_asi $resolvedController `
    "receipt-bound DiogenesGtaController.asi"
Pin-File $manifest.components.replus_asi $resolvedRePlus
Pin-File $manifest.components.replus_capture_addon $resolvedAddon
$resolvedReShade = Resolve-RequiredFile $ReShadeDll "ReShade proxy module"
$actualReShadeSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedReShade).Hash.ToLowerInvariant()
$receiptPath = Resolve-RequiredFile $ReShadeInstallReceipt "ReShade install receipt"
$receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
if ($receipt.schema -ne "dm-gta-reshade-install-receipt/1" -or
    $receipt.installer.sha256 -ne "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445" -or
    $receipt.installer.signer_thumbprint -ne "589690208A5E52FB96980C4A6698F50ACD47C49F" -or
    [IO.Path]::GetFullPath([string]$receipt.target.path) -ne $gameExe -or
    [IO.Path]::GetFullPath([string]$receipt.installed_dxgi.path) -ne $resolvedReShade -or
    $receipt.target.sha256 -ne $manifest.game.sha256 -or
    $receipt.installed_dxgi.sha256 -ne $actualReShadeSha256) {
    throw "ReShade receipt does not bind the approved installer to this GTA5.exe and dxgi.dll"
}
$receiptInstaller = Resolve-RequiredFile ([string]$receipt.installer.path) `
    "receipt-bound ReShade installer"
Assert-FileHash $receiptInstaller ([string]$receipt.installer.sha256) `
    "receipt-bound ReShade installer"
$installerSignature = Get-AuthenticodeSignature -LiteralPath $receiptInstaller
Assert-ApprovedReShadeSignature $installerSignature $receipt.installer
Assert-ReShadePayload $receiptInstaller $resolvedReShade $receipt.payload
if ([int64]$receipt.installed_dxgi.bytes -ne (Get-Item $resolvedReShade).Length -or
    [string]$receipt.installed_dxgi.sha256 -ne [string]$receipt.payload.sha256) {
    throw "ReShade receipt installed module differs from its signed-installer payload"
}
Pin-File $manifest.components.reshade_dxgi $resolvedReShade
$manifest.source_pins.reshade_dll_sha256 = $actualReShadeSha256
Pin-File $manifest.components.replus_render_ini (Resolve-RequiredFile $RenderIni "RE+ Render.ini")
Pin-File $manifest.components.diogenes_controller_asi $resolvedController
Pin-File $manifest.components.replus_main_ini `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "RockstarEditorPlus\RockstarEditorPlus.ini") `
        "RE+ main ini")
Pin-File $manifest.components.replus_lights_ini `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "RockstarEditorPlus\Lights.ini") `
        "RE+ Lights.ini")
Pin-File $manifest.components.replus_settings_exe `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "RockstarEditorPlus\RE+ Render Settings.exe") `
        "RE+ Render Settings.exe")
Pin-File $manifest.components.igcs_dof_shader `
    (Resolve-RequiredFile (Join-Path $resolvedGameDir "reshade-shaders\Shaders\IgcsDof.fx") `
        "IgcsDof.fx")
Pin-File $manifest.components.reshade_install_receipt $receiptPath
Pin-File $manifest.components.capture_build_receipt $buildReceiptPath
$manifest.source_pins.controller_git_commit = `
    [string]$buildReceipt.sources.controller_git_commit
if (-not $GtaSettingsXml) {
    $documents = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
    $GtaSettingsXml = Join-Path $documents "Rockstar Games\GTA V\settings.xml"
}
Pin-File $manifest.components.gta_settings_xml `
    (Resolve-RequiredFile $GtaSettingsXml "GTA settings.xml")

if ($ProjectionEvidence) {
    Pin-File $manifest.calibration.projection_evidence `
        (Resolve-RequiredFile $ProjectionEvidence "projection calibration evidence")
}

$ffmpeg = (Get-Command ffmpeg -ErrorAction Stop).Source
Pin-File $manifest.components.ffmpeg $ffmpeg
$manifest.components.ffmpeg.allow_path_lookup = $false
$ffprobe = (Get-Command ffprobe -ErrorAction Stop).Source
Pin-File $manifest.components.ffprobe $ffprobe
$manifest.components.ffprobe.allow_path_lookup = $false

$parent = Split-Path -Parent $Output
if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
$temporary = "$Output.writing"
$json = ($manifest | ConvertTo-Json -Depth 10) + [Environment]::NewLine
[IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
Move-Item -LiteralPath $temporary -Destination $Output -Force
Write-Host "Pinned dependency manifest: $Output"
Write-Host "Next: fill the calibration block, then run gta_capture preflight --mode real."
