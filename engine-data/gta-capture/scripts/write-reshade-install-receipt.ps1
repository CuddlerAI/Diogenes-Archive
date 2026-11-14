param(
    [Parameter(Mandatory = $true)][string]$GameDir,
    [Parameter(Mandatory = $true)][string]$ReShadeSetup,
    [Parameter(Mandatory = $true)][string]$Output,
    [ValidateSet("ReShade64.asi", "d3d11.dll", "dxgi.dll")]
    [string]$ReShadeModuleName = "ReShade64.asi"
)

$ErrorActionPreference = "Stop"
$expectedSetupSha256 = "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445"
$expectedSignerThumbprint = "589690208A5E52FB96980C4A6698F50ACD47C49F"

function Resolve-RequiredFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label not found: $Path" }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Assert-ApprovedReShadeSignature($Signature) {
    $thumbprint = if ($Signature.SignerCertificate) {
        $Signature.SignerCertificate.Thumbprint.ToUpperInvariant()
    } else { "" }
    $chain = New-Object Security.Cryptography.X509Certificates.X509Chain
    if ($Signature.SignerCertificate) { $null = $chain.Build($Signature.SignerCertificate) }
    $chainStatus = @($chain.ChainStatus | ForEach-Object { [string]$_.Status })
    $onlyUntrustedRoot = ($chainStatus.Count -eq 1 -and $chainStatus[0] -eq "UntrustedRoot")
    if ($thumbprint -ne $expectedSignerThumbprint -or
        -not ($Signature.Status -eq "Valid" -or
              ($Signature.Status -eq "UnknownError" -and $onlyUntrustedRoot))) {
        throw "ReShade installer signature is not approved: status $($Signature.Status), signer $thumbprint, chain $($chainStatus -join ',')"
    }
    return [ordered]@{ thumbprint = $thumbprint; chain_status = $chainStatus }
}

$resolvedGameDir = (Resolve-Path -LiteralPath $GameDir).Path
$setup = Resolve-RequiredFile $ReShadeSetup "ReShade full add-on installer"
$gameExe = Resolve-RequiredFile (Join-Path $resolvedGameDir "GTA5.exe") "GTA5.exe"
$module = Resolve-RequiredFile (Join-Path $resolvedGameDir $ReShadeModuleName) `
    "installed ReShade proxy module"
$setupSha256 = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
if ($setupSha256 -ne $expectedSetupSha256) {
    throw "ReShade installer SHA-256 mismatch: expected $expectedSetupSha256, found $setupSha256"
}
$signature = Get-AuthenticodeSignature -LiteralPath $setup
$signatureApproval = Assert-ApprovedReShadeSignature $signature
$tar = (Get-Command tar.exe -ErrorAction Stop).Source
$extractRoot = Join-Path ([IO.Path]::GetTempPath()) `
    ("diogenes-reshade-payload-" + [Guid]::NewGuid().ToString("N"))
try {
    New-Item -ItemType Directory -Path $extractRoot | Out-Null
    & $tar -xf $setup -C $extractRoot ReShade64.dll
    if ($LASTEXITCODE -ne 0) { throw "cannot extract ReShade64.dll from the signed installer" }
    $payload = Resolve-RequiredFile (Join-Path $extractRoot "ReShade64.dll") `
        "signed installer ReShade64.dll payload"
    $payloadSha256 = (Get-FileHash -LiteralPath $payload -Algorithm SHA256).Hash.ToLowerInvariant()
    $moduleSha256 = (Get-FileHash -LiteralPath $module -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($payloadSha256 -ne $moduleSha256 -or
        (Get-Item -LiteralPath $payload).Length -ne (Get-Item -LiteralPath $module).Length) {
        throw "installed $ReShadeModuleName is not byte-identical to ReShade64.dll in the signed installer"
    }
    $receipt = [ordered]@{
        schema = "dm-gta-reshade-install-receipt/1"
        created_at = (Get-Date).ToUniversalTime().ToString("o")
        installer = [ordered]@{
            path = $setup
            sha256 = $setupSha256
            signer_thumbprint = $signatureApproval.thumbprint
            signature_status = [string]$signature.Status
            signature_chain_status = $signatureApproval.chain_status
        }
        payload = [ordered]@{
            archive_entry = "ReShade64.dll"
            bytes = (Get-Item -LiteralPath $payload).Length
            sha256 = $payloadSha256
        }
        target = [ordered]@{
            path = $gameExe
            sha256 = (Get-FileHash -LiteralPath $gameExe -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        installed_dxgi = [ordered]@{
            path = $module
            proxy_module_name = $ReShadeModuleName
            bytes = (Get-Item -LiteralPath $module).Length
            sha256 = $moduleSha256
        }
    }
} finally {
    if (Test-Path -LiteralPath $extractRoot -PathType Container) {
        Remove-Item -LiteralPath $extractRoot -Recurse -Force
    }
}
$parent = Split-Path -Parent $Output
if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
$temporary = "$Output.writing"
$json = ($receipt | ConvertTo-Json -Depth 6) + [Environment]::NewLine
[IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
Move-Item -LiteralPath $temporary -Destination $Output -Force
Write-Host "Sealed ReShade install receipt: $Output"
