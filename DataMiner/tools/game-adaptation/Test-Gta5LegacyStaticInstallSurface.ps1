<#
.SYNOPSIS
  Reject a GTA V Legacy root that still contains known root-level user-mode instrumentation.

.DESCRIPTION
  This read-only, GTA-specific gate verifies pinned GTA5.exe bytes and inventories only a
  bounded set of root-level loader/proxy/add-on patterns. It never launches the game, loads
  a module, changes files, or authorizes injection. Output is sanitized and contains no path.

  A pass does not prove that the full Steam install tree matches an official manifest and does
  not clear clean_official_install_surface_missing. Runtime protection, representative Story
  Mode, online_session=false, window identity, D7 and Depth gates remain independently mandatory.

  Exit code 0: pinned executable and known bounded root-level patterns absent.
  Exit code 2: pinned executable or known root-pattern gate blocked.
  Exit code 1: invalid input or unexpected validation failure.
#>
[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$GameRoot,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:PinnedExecutableSha256 = '677e4e355cfbdb13273b1d992407e3c261b3a108dc4dd5c8a0c4c1da651802e5'
$script:PinnedExecutableLengthBytes = 47467128L
$script:MaxRootEntries = 4096
$script:SensitiveRootFiles = @(
    'ScriptHookV.dll',
    'dinput8.dll',
    'dxgi.dll',
    'd3d11.dll',
    'version.dll',
    'winmm.dll',
    'dsound.dll',
    'xinput1_3.dll',
    'RockstarEditorPlus.asi',
    'DiogenesGtaController.asi',
    'ReShade64.asi',
    'IgcsConnector.addon64'
)
$script:SensitiveRootDirectories = @(
    '.diogenes',
    '.diogenes-backup',
    'RockstarEditorPlus',
    'reshade-shaders',
    'mods',
    'scripts',
    'plugins'
)

function Test-IsReparsePoint {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    $item = Get-Item -LiteralPath $LiteralPath -Force -ErrorAction Stop
    return ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
}

function Test-PathChainHasReparsePoint {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $current = [IO.Path]::GetFullPath($LiteralPath)

    while (-not [string]::IsNullOrWhiteSpace($current)) {
        if (Test-IsReparsePoint -LiteralPath $current) { return $true }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ([string]::IsNullOrWhiteSpace($parent) -or $parent -ceq $current) { break }
        $current = $parent
    }
    return $false
}

function Get-StaticSurfaceReport {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralGameRoot,
        [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][long]$ExpectedLengthBytes,
        [ValidateRange(1, 4096)][int]$RootEntryCeiling = $script:MaxRootEntries
    )

    if ($ExpectedLengthBytes -le 0) { throw 'Expected executable length must be positive.' }

    $resolvedRoot = [IO.Path]::GetFullPath($LiteralGameRoot)
    if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
        throw 'GameRoot is not an existing directory.'
    }

    $blockers = [Collections.Generic.List[string]]::new()
    $observed = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $rootEntryCount = 0
    $rootEntryCeilingExceeded = $false
    $rootEnumerationComplete = $false
    $pathChainHasReparsePoint = Test-PathChainHasReparsePoint -LiteralPath $resolvedRoot
    if ($pathChainHasReparsePoint) {
        $blockers.Add('game_root_or_ancestor_reparse_point')
    }

    $gameExecutable = Join-Path $resolvedRoot 'GTA5.exe'
    $actualHash = $null
    $actualLength = $null
    if ($pathChainHasReparsePoint) {
        $blockers.Add('shipping_executable_not_inspected_through_reparse_chain')
    } elseif (-not (Test-Path -LiteralPath $gameExecutable -PathType Leaf)) {
        $blockers.Add('shipping_executable_missing')
    } elseif (Test-IsReparsePoint -LiteralPath $gameExecutable) {
        $blockers.Add('shipping_executable_reparse_point')
    } else {
        $actualLength = (Get-Item -LiteralPath $gameExecutable -Force -ErrorAction Stop).Length
        if ($actualLength -ne $ExpectedLengthBytes) {
            $blockers.Add('shipping_executable_length_mismatch')
        } else {
            $actualHash = (Get-FileHash -LiteralPath $gameExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actualHash -ne $ExpectedSha256) { $blockers.Add('shipping_executable_hash_mismatch') }
        }
    }

    if (-not $pathChainHasReparsePoint) {
        foreach ($name in $script:SensitiveRootFiles) {
            if (Test-Path -LiteralPath (Join-Path $resolvedRoot $name) -PathType Leaf) {
                [void]$observed.Add($name)
            }
        }
        foreach ($name in $script:SensitiveRootDirectories) {
            if (Test-Path -LiteralPath (Join-Path $resolvedRoot $name) -PathType Container) {
                [void]$observed.Add($name + '/')
            }
        }
        $knownSensitiveNames = [Collections.Generic.HashSet[string]]::new(
            [string[]]$script:SensitiveRootFiles,
            [StringComparer]::OrdinalIgnoreCase
        )
        $otherAsiCount = 0
        $otherAddonCount = 0
        $enumerator = [IO.Directory]::EnumerateFileSystemEntries($resolvedRoot).GetEnumerator()
        try {
            while ($enumerator.MoveNext()) {
                if ($rootEntryCount -ge $RootEntryCeiling) {
                    $rootEntryCeilingExceeded = $true
                    $blockers.Add('root_entry_ceiling_exceeded')
                    break
                }
                $rootEntryCount++
                $entry = [string]$enumerator.Current
                $attributes = [IO.File]::GetAttributes($entry)
                if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { continue }
                $name = [IO.Path]::GetFileName($entry)
                if ($knownSensitiveNames.Contains($name)) { continue }
                $extension = [IO.Path]::GetExtension($name)
                if ($extension -ieq '.asi') { $otherAsiCount++ }
                if ($extension -ieq '.addon64') { $otherAddonCount++ }
            }
            $rootEnumerationComplete = -not $rootEntryCeilingExceeded
        } finally {
            if ($enumerator -is [IDisposable]) { $enumerator.Dispose() }
        }
        if ($otherAsiCount -gt 0) { [void]$observed.Add("other-root-asi(count=$otherAsiCount)") }
        if ($otherAddonCount -gt 0) { [void]$observed.Add("other-root-addon64(count=$otherAddonCount)") }
    }
    if ($observed.Count -gt 0) { $blockers.Add('clean_official_install_surface_missing') }

    $sortedObserved = @($observed | Sort-Object)
    $sortedBlockers = @($blockers | Sort-Object -Unique)
    return [pscustomobject][ordered]@{
        schema = 'dm-gta5-legacy-static-install-surface/v1'
        game_id = 'gta5-legacy-pc'
        app_id = '271590'
        expected_steam_build_id = '24129523'
        identity_scope = 'exact_executable_bytes_only'
        executable_basename = 'GTA5.exe'
        executable_length_bytes = $actualLength
        expected_executable_length_bytes = $ExpectedLengthBytes
        executable_sha256 = $(if ($null -eq $actualHash) { $null } else { 'sha256:' + $actualHash })
        expected_executable_sha256 = 'sha256:' + $ExpectedSha256
        root_entry_ceiling = $RootEntryCeiling
        root_entries_scanned = $rootEntryCount
        status = $(if ($sortedBlockers.Count -eq 0) { 'pass_known_root_surface_only' } else { 'blocked' })
        known_root_instrumentation_absent = $rootEnumerationComplete -and $sortedObserved.Count -eq 0
        observed_instrumentation_artifacts = $sortedObserved
        blockers = $sortedBlockers
        restrictions = @(
            'This sanitized report contains no local path, PID, account, host, device, save, or captured-content identifier.',
            'The expected Steam build is contextual; this gate does not read a Steam manifest and proves only exact GTA5.exe bytes.',
            'A pass proves only that the enumerated known root-level loader/proxy/add-on patterns were absent within the fixed entry ceiling.',
            'A pass does not validate the full Steam file manifest or install tree and cannot clear clean_official_install_surface_missing.',
            'A pass does not prove Story Mode, online_session=false, a clean runtime module set, or absence of active anti-cheat.',
            'A pass does not authorize injection, Depth capture, recording readiness, publication, delivery, or training.'
        )
    }
}

function Assert-SelfTest {
    param([Parameter(Mandatory = $true)][bool]$Condition, [Parameter(Mandatory = $true)][string]$Message)
    if (-not $Condition) { throw "Self-test failed: $Message" }
}

function Invoke-SelfTest {
    $temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ('dm-gta5-static-surface-' + [Guid]::NewGuid().ToString('N'))
    $tempBoundary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $resolvedFixture = [IO.Path]::GetFullPath($temporaryRoot)
    if (-not $resolvedFixture.StartsWith($tempBoundary, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing an unsafe self-test directory.'
    }
    $junctionPath = $null
    try {
        $publicParameters = (Get-Command -Name $PSCommandPath -ErrorAction Stop).Parameters
        Assert-SelfTest (-not $publicParameters.ContainsKey('ExpectedExecutableSha256')) 'production hash override parameter is exposed'
        Assert-SelfTest (-not $publicParameters.ContainsKey('OutputPath')) 'production report-file write parameter is exposed'
        Assert-SelfTest (-not $publicParameters.ContainsKey('RootEntryCeiling')) 'production root-entry ceiling override parameter is exposed'
        Assert-SelfTest (
            $script:PinnedExecutableSha256 -ceq '677e4e355cfbdb13273b1d992407e3c261b3a108dc4dd5c8a0c4c1da651802e5'
        ) 'pinned production executable hash drifted'
        Assert-SelfTest ($script:PinnedExecutableLengthBytes -eq 47467128L) 'pinned production executable length drifted'

        [IO.Directory]::CreateDirectory($temporaryRoot) | Out-Null
        $fixtureExe = Join-Path $temporaryRoot 'GTA5.exe'
        [IO.File]::WriteAllBytes($fixtureExe, [Text.Encoding]::UTF8.GetBytes('synthetic-gta5-executable'))
        $fixtureHash = (Get-FileHash -LiteralPath $fixtureExe -Algorithm SHA256).Hash.ToLowerInvariant()
        $fixtureLength = (Get-Item -LiteralPath $fixtureExe -Force).Length

        $clean = Get-StaticSurfaceReport -LiteralGameRoot $temporaryRoot -ExpectedSha256 $fixtureHash -ExpectedLengthBytes $fixtureLength
        Assert-SelfTest ($clean.status -ceq 'pass_known_root_surface_only') 'clean exact fixture did not pass'
        Assert-SelfTest $clean.known_root_instrumentation_absent 'clean fixture did not prove the bounded known-root claim'
        Assert-SelfTest ($clean.observed_instrumentation_artifacts.Count -eq 0) 'clean fixture reported instrumentation'
        Assert-SelfTest (-not (($clean | ConvertTo-Json -Depth 8).Contains($resolvedFixture))) 'report leaked the fixture path'

        foreach ($case in @(
            @{ Name = 'dinput8.dll'; Kind = 'file'; Expected = 'dinput8.dll' },
            @{ Name = 'UnknownCapture.asi'; Kind = 'file'; Expected = 'other-root-asi(count=1)' },
            @{ Name = 'UnknownCapture.addon64'; Kind = 'file'; Expected = 'other-root-addon64(count=1)' },
            @{ Name = '.diogenes'; Kind = 'directory'; Expected = '.diogenes/' }
        )) {
            $candidate = Join-Path $temporaryRoot $case.Name
            if ($case.Kind -eq 'directory') {
                [IO.Directory]::CreateDirectory($candidate) | Out-Null
            } else {
                [IO.File]::WriteAllBytes($candidate, [byte[]](1, 2, 3))
            }
            $blocked = Get-StaticSurfaceReport -LiteralGameRoot $temporaryRoot -ExpectedSha256 $fixtureHash -ExpectedLengthBytes $fixtureLength
            Assert-SelfTest ($blocked.status -ceq 'blocked') "$($case.Name) did not block"
            Assert-SelfTest ($blocked.blockers -ccontains 'clean_official_install_surface_missing') "$($case.Name) lacks blocker"
            Assert-SelfTest ($blocked.observed_instrumentation_artifacts -ccontains $case.Expected) "$($case.Name) was not reported"
            if ($case.Kind -eq 'directory') {
                Remove-Item -LiteralPath $candidate -Recurse -Force
            } else {
                Remove-Item -LiteralPath $candidate -Force
            }
        }

        $wrongHash = '0' * 64
        $hashBlocked = Get-StaticSurfaceReport -LiteralGameRoot $temporaryRoot -ExpectedSha256 $wrongHash -ExpectedLengthBytes $fixtureLength
        Assert-SelfTest ($hashBlocked.blockers -ccontains 'shipping_executable_hash_mismatch') 'hash drift did not block'

        [IO.File]::WriteAllBytes($fixtureExe, [Text.Encoding]::UTF8.GetBytes('synthetic-gta5-executable-length-drift'))
        $lengthBlocked = Get-StaticSurfaceReport -LiteralGameRoot $temporaryRoot -ExpectedSha256 $fixtureHash -ExpectedLengthBytes $fixtureLength
        Assert-SelfTest ($lengthBlocked.blockers -ccontains 'shipping_executable_length_mismatch') 'length drift did not block'
        Assert-SelfTest ($null -eq $lengthBlocked.executable_sha256) 'length drift did not suppress executable hashing'
        [IO.File]::WriteAllBytes($fixtureExe, [Text.Encoding]::UTF8.GetBytes('synthetic-gta5-executable'))

        foreach ($index in 1..3) {
            [IO.File]::WriteAllBytes((Join-Path $temporaryRoot ("ceiling-fixture-$index.bin")), [byte[]](1))
        }
        $ceilingBlocked = Get-StaticSurfaceReport -LiteralGameRoot $temporaryRoot -ExpectedSha256 $fixtureHash -ExpectedLengthBytes $fixtureLength -RootEntryCeiling 3
        Assert-SelfTest ($ceilingBlocked.blockers -ccontains 'root_entry_ceiling_exceeded') 'bounded root entry ceiling did not block'
        Assert-SelfTest (-not $ceilingBlocked.known_root_instrumentation_absent) 'truncated enumeration overclaimed known-root absence'
        foreach ($index in 1..3) {
            Remove-Item -LiteralPath (Join-Path $temporaryRoot ("ceiling-fixture-$index.bin")) -Force
        }

        $junctionTarget = Join-Path $temporaryRoot 'junction-target'
        $junctionGameRoot = Join-Path $junctionTarget 'game'
        [IO.Directory]::CreateDirectory($junctionGameRoot) | Out-Null
        [IO.File]::WriteAllBytes((Join-Path $junctionGameRoot 'GTA5.exe'), [Text.Encoding]::UTF8.GetBytes('synthetic-gta5-executable'))
        $junctionPath = Join-Path $temporaryRoot 'junction-parent'
        New-Item -ItemType Junction -Path $junctionPath -Target $junctionTarget -ErrorAction Stop | Out-Null
        $linkedGameRoot = Join-Path $junctionPath 'game'
        $reparseBlocked = Get-StaticSurfaceReport -LiteralGameRoot $linkedGameRoot -ExpectedSha256 $fixtureHash -ExpectedLengthBytes $fixtureLength
        Assert-SelfTest ($reparseBlocked.blockers -ccontains 'game_root_or_ancestor_reparse_point') 'ancestor reparse point did not block'
        Assert-SelfTest ($reparseBlocked.blockers -ccontains 'shipping_executable_not_inspected_through_reparse_chain') 'executable was not blocked behind ancestor reparse point'
        [IO.Directory]::Delete($junctionPath)
        $junctionPath = $null

        'GTA V Legacy known-root surface self-test passed (pinned production length/hash, ancestor reparse chain, bounded enumeration, exact hash, known proxy, arbitrary ASI/add-on, instrumentation directory, length/hash drift, and sanitized output).'
    } finally {
        if ($null -ne $junctionPath -and (Test-Path -LiteralPath $junctionPath)) {
            [IO.Directory]::Delete($junctionPath)
        }
        if (Test-Path -LiteralPath $resolvedFixture) {
            Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
        }
    }
}

if ($PSCmdlet.ParameterSetName -eq 'SelfTest') {
    Invoke-SelfTest
    exit 0
}

try {
    $report = Get-StaticSurfaceReport -LiteralGameRoot $GameRoot -ExpectedSha256 $script:PinnedExecutableSha256 -ExpectedLengthBytes $script:PinnedExecutableLengthBytes
    $json = $report | ConvertTo-Json -Depth 8
    Write-Output $json
    if ($report.status -cne 'pass_known_root_surface_only') { exit 2 }
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
