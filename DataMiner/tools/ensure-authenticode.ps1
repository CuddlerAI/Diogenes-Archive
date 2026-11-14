# Makes `Get-AuthenticodeSignature` callable, or fails loudly.
#
# Why this exists — the dataminer-v0.2.0 and v0.3.0 release builds both died here:
#
#   1. The release step's shell is `pwsh` (PowerShell 7), which exports a
#      PSModulePath containing PowerShell 7's own module directory.
#   2. `pnpm bundle:windows:dev` spawns **Windows PowerShell 5.1** (see the
#      package.json script), which inherits that PSModulePath.
#   3. 5.1 then resolves `Microsoft.PowerShell.Security` to PowerShell 7's copy.
#      Loading that into 5.1 fails with duplicate extended type data
#      ("The member AccessToString is already present").
#
# So `Get-AuthenticodeSignature` could neither autoload (v0.2.0:
# `CouldNotAutoloadMatchingModule`) nor be imported by name (v0.3.0:
# `FormatXmlUpdateException`). Putting *this host's own* module directory first makes
# 5.1 resolve its own copy, which loads cleanly.
#
# This fails closed on purpose. The callers use the cmdlet to assert that the bundle
# we publish is *not* Authenticode-signed (the honest UNSIGNED_NON_PRODUCTION label,
# windows-unsigned-dev-installer.md §5). Degrading to "cmdlet missing, skip the
# assertion" would ship an artifact whose signing state was never checked, so a
# missing cmdlet must stop the build instead.
#
# Dot-source it (`. (Join-Path $PSScriptRoot 'ensure-authenticode.ps1')`); CI runs it
# through the same pwsh -> Windows PowerShell 5.1 chain, so a regression fails a PR
# instead of a tagged release.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Prefer this host's own modules over anything inherited from a parent shell.
$hostModules = Join-Path $PSHOME 'Modules'
$currentModulePath = [string]$env:PSModulePath
if (-not $currentModulePath.StartsWith($hostModules, [System.StringComparison]::OrdinalIgnoreCase)) {
    $env:PSModulePath = $hostModules + [System.IO.Path]::PathSeparator + $currentModulePath
}

if (-not (Get-Command -Name Get-AuthenticodeSignature -ErrorAction SilentlyContinue)) {
    # An import failure must not abort the script under $ErrorActionPreference='Stop';
    # the availability re-check below is the real gate.
    try {
        Import-Module -Name Microsoft.PowerShell.Security -ErrorAction Stop -WarningAction SilentlyContinue
    } catch {
        Write-Host "Import-Module Microsoft.PowerShell.Security failed: $($_.Exception.Message)"
    }
}

if (-not (Get-Command -Name Get-AuthenticodeSignature -ErrorAction SilentlyContinue)) {
    throw "Get-AuthenticodeSignature is unavailable (Microsoft.PowerShell.Security could not be imported); refusing to skip the unsigned-artifact assertions. PSHOME='$PSHOME'; PSModulePath='$env:PSModulePath'"
}
