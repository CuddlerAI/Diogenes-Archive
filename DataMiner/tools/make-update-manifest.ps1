# Builds the update-channel pointer `latest.json` for a single published
# installer, in the format consumed by tauri-plugin-updater plus the two
# DataMiner extension fields (`minSupported`, `sha256`) from docs/15 §2.2.
#
# The minisign signature is the only update trust root (docs/15 §2.1): this
# script refuses to emit a manifest with an empty or placeholder signature, so
# the release workflow cannot silently publish an unverifiable artifact.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    # Path to the detached minisign signature produced by `tauri signer sign`
    # (the raw .sig file content goes into latest.json verbatim).
    [Parameter(Mandatory = $true)][string]$SignaturePath,
    [Parameter(Mandatory = $true)][string]$OutputPath,
    # Channel origin. The final download URL is
    # <BaseUrl>/releases/v/<version>/<installer file name>.
    [string]$BaseUrl = 'https://releases.diogenesmodel.com',
    # Channel policy file carrying minSupported; see config/release-channel.json.
    [string]$ChannelConfigPath,
    # Optional UTF-8 release-notes file (typically the annotated tag message).
    [string]$NotesPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$dataMinerRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$semverPattern = '^\d+\.\d+\.\d+$'

function ConvertTo-SemverParts {
    param([Parameter(Mandatory = $true)][string]$Version)
    return @($Version.Split('.') | ForEach-Object { [int]$_ })
}

function Compare-Semver {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )
    $leftParts = ConvertTo-SemverParts $Left
    $rightParts = ConvertTo-SemverParts $Right
    for ($i = 0; $i -lt 3; $i++) {
        if ($leftParts[$i] -ne $rightParts[$i]) {
            return [Math]::Sign($leftParts[$i] - $rightParts[$i])
        }
    }
    return 0
}

if ($Version -notmatch $semverPattern) {
    throw "Version '$Version' is not MAJOR.MINOR.PATCH."
}
if ($BaseUrl -notmatch '^https://[^/]+$') {
    throw "BaseUrl must be an https origin without a trailing path, got '$BaseUrl'."
}

$installer = Get-Item -LiteralPath $InstallerPath
if ($installer.Name -notlike "*_${Version}_*-setup.exe") {
    throw "Installer file name '$($installer.Name)' does not carry version $Version; refusing to publish a mismatched pointer."
}

$signature = (Get-Content -Raw -Encoding UTF8 -LiteralPath $SignaturePath).Trim()
if ([string]::IsNullOrWhiteSpace($signature)) {
    throw "Signature file $SignaturePath is empty; the channel must not publish unsigned manifests."
}
if ($signature -match 'PLACEHOLDER') {
    throw "Signature file $SignaturePath still contains a placeholder token."
}

if (-not $ChannelConfigPath) {
    $ChannelConfigPath = Join-Path $dataMinerRoot 'config/release-channel.json'
}
$channelConfig = Get-Content -Raw -Encoding UTF8 -LiteralPath $ChannelConfigPath | ConvertFrom-Json
if ($channelConfig.schema -ne 'dm-release-channel-1') {
    throw "Unexpected channel config schema '$($channelConfig.schema)' in $ChannelConfigPath"
}
$minSupported = [string]$channelConfig.minSupported
if ($minSupported -notmatch $semverPattern) {
    throw "minSupported '$minSupported' in $ChannelConfigPath is not MAJOR.MINOR.PATCH."
}
if ((Compare-Semver $minSupported $Version) -gt 0) {
    throw "minSupported $minSupported is newer than the version being published ($Version)."
}

$notes = ''
if ($NotesPath -and (Test-Path -LiteralPath $NotesPath -PathType Leaf)) {
    $notes = (Get-Content -Raw -Encoding UTF8 -LiteralPath $NotesPath).Trim()
}

$sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer.FullName).Hash.ToLowerInvariant()
$manifest = [ordered]@{
    version  = $Version
    pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'")
    notes    = $notes
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            url       = "$BaseUrl/releases/v/$Version/$($installer.Name)"
            signature = $signature
        }
    }
    # DataMiner extensions (ignored by tauri-plugin-updater, docs/15 §2.2):
    # sha256 mirrors the build manifest for human audit; it is not the trust root.
    minSupported = $minSupported
    sha256       = $sha256
}

$outputDirectory = Split-Path -Parent $OutputPath
if ($outputDirectory -and -not (Test-Path -LiteralPath $outputDirectory)) {
    New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 -LiteralPath $OutputPath

$staged = Get-Content -Raw -Encoding UTF8 -LiteralPath $OutputPath | ConvertFrom-Json
if ($staged.version -ne $Version -or
    $staged.platforms.'windows-x86_64'.signature -ne $signature -or
    $staged.sha256 -ne $sha256) {
    throw 'Staged latest.json failed round-trip validation.'
}

Write-Host "PASS: update manifest for $Version staged at $OutputPath"
Write-Host "  url:          $($manifest.platforms.'windows-x86_64'.url)"
Write-Host "  sha256:       $sha256"
Write-Host "  minSupported: $minSupported"
