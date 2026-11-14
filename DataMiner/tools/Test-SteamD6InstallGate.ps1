<#
.SYNOPSIS
  Evaluate the Steam installation gates that must pass before D6 authoring can begin.

.DESCRIPTION
  This probe is read-only and fail-closed. It reads appmanifest files and, only after
  StateFlags=4, buildid/SizeOnDisk are non-zero, and the explicit steamapps/common
  installation directory exists, enumerates installed executable candidates. It never
  falls back to steamapps/downloading or staging bytes.

  Executable hashes are emitted only after two observations show that the manifest and
  every installed executable candidate have stable length and last-write time. A passing
  result closes only the D6 installation gate; the shipping process, window class,
  settings, mapping, and D7/Depth gates still require separate evidence.

  Exit code 0: every requested AppID passed the installation gate.
  Exit code 2: at least one requested AppID is blocked.
  Exit code 1: invalid input or an unexpected probe failure.
#>
[CmdletBinding(DefaultParameterSetName = 'Probe')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Probe')]
    [ValidateNotNullOrEmpty()]
    [ValidateScript({ $_ -match '^\d+$' })]
    [string[]]$AppId,

    [Parameter(ParameterSetName = 'Probe')]
    [string]$SteamPath = '',

    [Parameter(ParameterSetName = 'Probe')]
    [ValidateRange(1, 300)]
    [int]$StabilitySeconds = 10,

    [Parameter(ParameterSetName = 'Probe')]
    [switch]$SkipExecutableHashes,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$MaxLibraryFoldersBytes = 2MB
$MaxAppManifestBytes = 256KB

function Get-AcfField {
    param(
        [Parameter(Mandatory = $true)][string]$Text,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $match = [regex]::Match($Text, ('"' + [regex]::Escape($Name) + '"\s+"([^"]*)"'))
    if (-not $match.Success) { return '' }
    return $match.Groups[1].Value
}

function ConvertTo-NonNegativeInt64 {
    param([string]$Value)
    [long]$parsed = 0
    if (-not [long]::TryParse($Value, [ref]$parsed) -or $parsed -lt 0) { return $null }
    return $parsed
}

function Test-PathWithinRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Candidate
    )
    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $candidateFull = [IO.Path]::GetFullPath($Candidate)
    return $candidateFull.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)
}

function Get-SteamLibraryRoots {
    param([string]$ExplicitSteamPath)

    $root = $ExplicitSteamPath
    if (-not $root) {
        $root = [string](Get-ItemProperty 'HKCU:\Software\Valve\Steam' -ErrorAction Stop).SteamPath
    }
    if (-not $root) { throw 'SteamPath is empty.' }
    $root = [IO.Path]::GetFullPath($root)

    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $roots = [Collections.Generic.List[string]]::new()
    if ($seen.Add($root)) { $roots.Add($root) }

    $libraryFile = Join-Path $root 'steamapps\libraryfolders.vdf'
    if (Test-Path -LiteralPath $libraryFile -PathType Leaf) {
        $libraryFileItem = Get-Item -LiteralPath $libraryFile -Force
        if (($libraryFileItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'steam_libraryfolders_reparse_point'
        }
        if ($libraryFileItem.Length -gt $MaxLibraryFoldersBytes) {
            throw 'steam_libraryfolders_too_large'
        }
        $text = [IO.File]::ReadAllText($libraryFile)
        foreach ($match in [regex]::Matches($text, '"path"\s+"([^"]+)"')) {
            $candidate = $match.Groups[1].Value.Replace('\\', '\')
            if (-not $candidate) { continue }
            $candidate = [IO.Path]::GetFullPath($candidate)
            if ($seen.Add($candidate)) { $roots.Add($candidate) }
        }
    }
    return @($roots)
}

function Get-ExecutableSnapshot {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)

    $root = Get-Item -LiteralPath $InstallDirectory -Force -ErrorAction Stop
    if (($root.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'install_tree_reparse_point'
    }

    $directories = [Collections.Generic.Stack[IO.DirectoryInfo]]::new()
    $directories.Push($root)
    $executables = [Collections.Generic.List[object]]::new()
    while ($directories.Count -ne 0) {
        $directory = $directories.Pop()
        foreach ($child in Get-ChildItem -LiteralPath $directory.FullName -Force -ErrorAction Stop) {
            if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'install_tree_reparse_point'
            }
            if ($child.PSIsContainer) {
                $directories.Push($child)
            } elseif ($child.Extension -ieq '.exe') {
                $executables.Add([pscustomobject]@{
                    path = $child.FullName
                    relative_path = $child.FullName.Substring($InstallDirectory.TrimEnd('\', '/').Length).TrimStart('\', '/')
                    length = [long]$child.Length
                    last_write_utc = $child.LastWriteTimeUtc.ToString('o')
                })
            }
        }
    }
    return @($executables | Sort-Object path)
}

function Compare-ExecutableSnapshots {
    param(
        [Parameter(Mandatory = $true)][object[]]$Before,
        [Parameter(Mandatory = $true)][object[]]$After
    )
    if ($Before.Count -ne $After.Count) { return $false }
    for ($index = 0; $index -lt $Before.Count; $index++) {
        $left = $Before[$index]
        $right = $After[$index]
        if (
            -not $left.path.Equals($right.path, [StringComparison]::OrdinalIgnoreCase) -or
            $left.length -ne $right.length -or
            $left.last_write_utc -ne $right.last_write_utc
        ) {
            return $false
        }
    }
    return $true
}

function Invoke-SteamD6InstallProbe {
    param(
        [Parameter(Mandatory = $true)][string[]]$LibraryRoots,
        [Parameter(Mandatory = $true)][string]$TargetAppId,
        [Parameter(Mandatory = $true)][int]$ObservationSeconds,
        [Parameter(Mandatory = $true)][bool]$HashExecutables,
        [scriptblock]$BeforeFinalSnapshot
    )

    $manifests = @(
        foreach ($libraryRoot in $LibraryRoots) {
            $candidate = Join-Path $libraryRoot "steamapps\appmanifest_$TargetAppId.acf"
            if (Test-Path -LiteralPath $candidate) {
                $candidateItem = Get-Item -LiteralPath $candidate -Force
                if (
                    -not $candidateItem.PSIsContainer -or
                    ($candidateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
                ) {
                    [IO.Path]::GetFullPath($candidate)
                }
            }
        }
    )

    $blocking = [Collections.Generic.List[string]]::new()
    if ($manifests.Count -eq 0) {
        $blocking.Add('appmanifest_missing')
    } elseif ($manifests.Count -gt 1) {
        $blocking.Add('appmanifest_ambiguous')
    }

    if ($blocking.Count -ne 0) {
        return [pscustomobject]@{
            app_id = $TargetAppId
            status = 'blocked'
            name = $null
            manifest = $null
            state_flags = $null
            build_id = $null
            size_on_disk = $null
            install_directory = $null
            executable_candidates = @()
            blocking = @($blocking)
            remaining_human_gates = @('confirm_shipping_process', 'window_identity', 'settings', 'default_mapping')
        }
    }

    $manifestPath = $manifests[0]
    $manifestBefore = Get-Item -LiteralPath $manifestPath -Force
    $manifestMetadataBlocker = if (($manifestBefore.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        'appmanifest_reparse_point'
    } elseif ($manifestBefore.Length -gt $MaxAppManifestBytes) {
        'appmanifest_too_large'
    } else {
        $null
    }
    if ($manifestMetadataBlocker) {
        return [pscustomobject]@{
            app_id = $TargetAppId
            status = 'blocked'
            name = $null
            manifest = $manifestPath
            state_flags = $null
            build_id = $null
            size_on_disk = $null
            install_directory = $null
            executable_candidates = @()
            blocking = @($manifestMetadataBlocker)
            remaining_human_gates = @('confirm_shipping_process', 'window_identity', 'settings', 'default_mapping')
        }
    }
    $manifestText = [IO.File]::ReadAllText($manifestPath)
    $manifestAppId = Get-AcfField -Text $manifestText -Name 'appid'
    $name = Get-AcfField -Text $manifestText -Name 'name'
    $stateFlags = ConvertTo-NonNegativeInt64 (Get-AcfField -Text $manifestText -Name 'StateFlags')
    $buildId = ConvertTo-NonNegativeInt64 (Get-AcfField -Text $manifestText -Name 'buildid')
    $sizeOnDisk = ConvertTo-NonNegativeInt64 (Get-AcfField -Text $manifestText -Name 'SizeOnDisk')
    $installDirName = Get-AcfField -Text $manifestText -Name 'installdir'
    $libraryRoot = Split-Path (Split-Path $manifestPath -Parent) -Parent
    $steamAppsRoot = Join-Path $libraryRoot 'steamapps'
    $commonRoot = Join-Path $libraryRoot 'steamapps\common'
    $installDirectory = $null

    if ($manifestAppId -ne $TargetAppId) { $blocking.Add("manifest_app_id_mismatch:$manifestAppId") }
    if ($null -eq $stateFlags) { $blocking.Add('state_flags_invalid') }
    elseif ($stateFlags -ne 4) { $blocking.Add("state_flags_not_fully_installed:$stateFlags") }
    if ($null -eq $buildId -or $buildId -le 0) { $blocking.Add('build_id_missing_or_zero') }
    if ($null -eq $sizeOnDisk -or $sizeOnDisk -le 0) { $blocking.Add('size_on_disk_missing_or_zero') }
    foreach ($trustedChild in @($steamAppsRoot, $commonRoot)) {
        if (Test-Path -LiteralPath $trustedChild) {
            $childItem = Get-Item -LiteralPath $trustedChild -Force
            if (($childItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                $blocking.Add('steam_library_child_reparse_point')
                break
            }
        }
    }
    if (
        -not $installDirName -or
        [IO.Path]::IsPathRooted($installDirName) -or
        $installDirName -in @('.', '..') -or
        $installDirName -match '[\\/]'
    ) {
        $blocking.Add('install_dir_invalid')
    } else {
        $installDirectory = [IO.Path]::GetFullPath((Join-Path $commonRoot $installDirName))
        if (-not (Test-PathWithinRoot -Root $commonRoot -Candidate $installDirectory)) {
            $blocking.Add('install_dir_outside_common')
            $installDirectory = $null
        } elseif (-not (Test-Path -LiteralPath $installDirectory -PathType Container)) {
            $blocking.Add('install_dir_missing')
        }
    }

    # Do not enumerate or hash any executable until every manifest/common-directory gate passes.
    $candidates = @()
    if ($blocking.Count -eq 0) {
        try {
            $before = @(Get-ExecutableSnapshot -InstallDirectory $installDirectory)
        } catch {
            $before = @()
            $blocking.Add($_.Exception.Message)
        }
        if ($before.Count -eq 0) {
            if ($blocking.Count -eq 0) { $blocking.Add('installed_executable_missing') }
        } else {
            Start-Sleep -Seconds $ObservationSeconds
            $after = @(Get-ExecutableSnapshot -InstallDirectory $installDirectory)
            $manifestAfter = Get-Item -LiteralPath $manifestPath
            $manifestStable = (
                $manifestBefore.Length -eq $manifestAfter.Length -and
                $manifestBefore.LastWriteTimeUtc -eq $manifestAfter.LastWriteTimeUtc
            )
            if (-not $manifestStable) { $blocking.Add('appmanifest_changed_during_observation') }
            if (-not (Compare-ExecutableSnapshots -Before $before -After $after)) {
                $blocking.Add('installed_executables_changed_during_observation')
            }
            if ($blocking.Count -eq 0) {
                $candidateResults = @(
                    foreach ($candidate in $after) {
                        $hash = $null
                        if ($HashExecutables) {
                            $beforeHash = Get-Item -LiteralPath $candidate.path
                            $hash = 'sha256:' + (Get-FileHash -LiteralPath $candidate.path -Algorithm SHA256).Hash.ToLowerInvariant()
                            $afterHash = Get-Item -LiteralPath $candidate.path
                            if (
                                $beforeHash.Length -ne $afterHash.Length -or
                                $beforeHash.LastWriteTimeUtc -ne $afterHash.LastWriteTimeUtc -or
                                $afterHash.Length -ne $candidate.length -or
                                $afterHash.LastWriteTimeUtc.ToString('o') -ne $candidate.last_write_utc
                            ) {
                                $blocking.Add("installed_executable_changed_while_hashing:$($candidate.relative_path)")
                                break
                            }
                        }
                        [pscustomobject]@{
                            relative_path = $candidate.relative_path
                            length = $candidate.length
                            file_version = [Diagnostics.FileVersionInfo]::GetVersionInfo($candidate.path).FileVersion
                            sha256 = $hash
                        }
                    }
                )
                if ($null -ne $BeforeFinalSnapshot) {
                    & $BeforeFinalSnapshot $installDirectory
                }
                $manifestFinal = Get-Item -LiteralPath $manifestPath
                if (
                    $manifestAfter.Length -ne $manifestFinal.Length -or
                    $manifestAfter.LastWriteTimeUtc -ne $manifestFinal.LastWriteTimeUtc
                ) {
                    $blocking.Add('appmanifest_changed_while_hashing')
                }
                try {
                    $finalSnapshot = @(Get-ExecutableSnapshot -InstallDirectory $installDirectory)
                    if (-not (Compare-ExecutableSnapshots -Before $after -After $finalSnapshot)) {
                        $blocking.Add('installed_executables_changed_while_hashing')
                    }
                } catch {
                    $blocking.Add($_.Exception.Message)
                }
                if ($blocking.Count -eq 0) { $candidates = @($candidateResults) }
            }
        }
    }

    return [pscustomobject]@{
        app_id = $TargetAppId
        status = $(if ($blocking.Count -eq 0) { 'install_gate_passed' } else { 'blocked' })
        name = $name
        manifest = $manifestPath
        state_flags = $stateFlags
        build_id = $buildId
        size_on_disk = $sizeOnDisk
        install_directory = $installDirectory
        stability_observation_seconds = $ObservationSeconds
        executable_candidates = @($candidates)
        blocking = @($blocking)
        remaining_human_gates = @('confirm_shipping_process', 'window_identity_twice', 'settings', 'default_mapping')
    }
}

function Invoke-SelfTest {
    $temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("dm-steam-d6-gate-" + [Guid]::NewGuid().ToString('N'))
    $resolvedTempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $resolvedFixture = [IO.Path]::GetFullPath($temporaryRoot)
    if (-not $resolvedFixture.StartsWith($resolvedTempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing unsafe self-test directory: $resolvedFixture"
    }

    try {
        New-Item -ItemType Directory -Path (Join-Path $temporaryRoot 'steamapps\common\Ready Game') -Force | Out-Null
        New-Item -ItemType Directory -Path (Join-Path $temporaryRoot 'steamapps\downloading\100') -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\libraryfolders.vdf'), '"libraryfolders" { }')

        $blockedManifest = @'
"AppState"
{
  "appid" "100"
  "name" "Downloading Game"
  "StateFlags" "1026"
  "installdir" "Downloading Game"
  "SizeOnDisk" "0"
  "buildid" "0"
}
'@
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\appmanifest_100.acf'), $blockedManifest)
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\downloading\100\Staging.exe'), 'not stable install bytes')
        $blocked = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '100' -ObservationSeconds 1 -HashExecutables $true
        if ($blocked.status -ne 'blocked') { throw 'Self-test expected incomplete download to block.' }
        if ($blocked.executable_candidates.Count -ne 0) { throw 'Self-test exposed a staging executable.' }
        if (-not ($blocked.blocking -contains 'build_id_missing_or_zero')) { throw 'Self-test missed zero build id.' }

        [IO.File]::WriteAllBytes(
            (Join-Path $temporaryRoot 'steamapps\appmanifest_225.acf'),
            [byte[]]::new($MaxAppManifestBytes + 1)
        )
        $oversizedManifest = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '225' -ObservationSeconds 1 -HashExecutables $true
        if ($oversizedManifest.status -ne 'blocked' -or -not ($oversizedManifest.blocking -contains 'appmanifest_too_large')) {
            throw 'Self-test failed to reject an oversized appmanifest before parsing.'
        }

        $manifestLinkTarget = Join-Path $temporaryRoot 'linked-appmanifest-target'
        New-Item -ItemType Directory -Path $manifestLinkTarget -Force | Out-Null
        $manifestLink = Join-Path $temporaryRoot 'steamapps\appmanifest_230.acf'
        New-Item -ItemType Junction -Path $manifestLink -Target $manifestLinkTarget | Out-Null
        $linkedManifest = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '230' -ObservationSeconds 1 -HashExecutables $true
        if ($linkedManifest.status -ne 'blocked' -or -not ($linkedManifest.blocking -contains 'appmanifest_reparse_point')) {
            throw 'Self-test failed to reject an appmanifest file reparse point before parsing.'
        }
        if ($linkedManifest.executable_candidates.Count -ne 0) { throw 'Self-test exposed candidates through an appmanifest reparse point.' }

        $unsafeManifest = @'
"AppState"
{
  "appid" "200"
  "name" "Unsafe Game"
  "StateFlags" "4"
  "installdir" "..\downloading\100"
  "SizeOnDisk" "1"
  "buildid" "2"
}
'@
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\appmanifest_200.acf'), $unsafeManifest)
        $unsafe = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '200' -ObservationSeconds 1 -HashExecutables $true
        if ($unsafe.status -ne 'blocked' -or -not ($unsafe.blocking -contains 'install_dir_invalid')) {
            throw 'Self-test failed to reject an install directory outside steamapps/common.'
        }

        $reparseTarget = Join-Path $temporaryRoot 'outside-common'
        New-Item -ItemType Directory -Path $reparseTarget -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $reparseTarget 'Escaped.exe'), 'must never be hashed')
        $reparseInstall = Join-Path $temporaryRoot 'steamapps\common\Reparse Game'
        New-Item -ItemType Junction -Path $reparseInstall -Target $reparseTarget | Out-Null
        $reparseManifest = @'
"AppState"
{
  "appid" "250"
  "name" "Reparse Game"
  "StateFlags" "4"
  "installdir" "Reparse Game"
  "SizeOnDisk" "4"
  "buildid" "5"
}
'@
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\appmanifest_250.acf'), $reparseManifest)
        $reparse = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '250' -ObservationSeconds 1 -HashExecutables $true
        if ($reparse.status -ne 'blocked' -or -not ($reparse.blocking -contains 'install_tree_reparse_point')) {
            throw 'Self-test failed to reject a reparse-point installation tree.'
        }
        if ($reparse.executable_candidates.Count -ne 0) { throw 'Self-test exposed an executable through a reparse point.' }

        $ancestorLibrary = Join-Path $temporaryRoot 'ancestor-library'
        $ancestorSteamAppsTarget = Join-Path $temporaryRoot 'ancestor-steamapps-target'
        New-Item -ItemType Directory -Path (Join-Path $ancestorSteamAppsTarget 'common\Ancestor Game') -Force | Out-Null
        New-Item -ItemType Directory -Path $ancestorLibrary -Force | Out-Null
        New-Item -ItemType Junction -Path (Join-Path $ancestorLibrary 'steamapps') -Target $ancestorSteamAppsTarget | Out-Null
        $ancestorManifest = @'
"AppState"
{
  "appid" "275"
  "name" "Ancestor Reparse Game"
  "StateFlags" "4"
  "installdir" "Ancestor Game"
  "SizeOnDisk" "4"
  "buildid" "5"
}
'@
        [IO.File]::WriteAllText((Join-Path $ancestorSteamAppsTarget 'appmanifest_275.acf'), $ancestorManifest)
        [IO.File]::WriteAllText((Join-Path $ancestorSteamAppsTarget 'common\Ancestor Game\Escaped.exe'), 'must never be hashed')
        $ancestorReparse = Invoke-SteamD6InstallProbe -LibraryRoots @($ancestorLibrary) -TargetAppId '275' -ObservationSeconds 1 -HashExecutables $true
        if ($ancestorReparse.status -ne 'blocked' -or -not ($ancestorReparse.blocking -contains 'steam_library_child_reparse_point')) {
            throw 'Self-test failed to reject a reparse point in the steamapps/common ancestor chain.'
        }
        if ($ancestorReparse.executable_candidates.Count -ne 0) { throw 'Self-test exposed an executable through an ancestor reparse point.' }

        $commonLibrary = Join-Path $temporaryRoot 'common-library'
        $commonTarget = Join-Path $temporaryRoot 'common-target'
        New-Item -ItemType Directory -Path (Join-Path $commonLibrary 'steamapps') -Force | Out-Null
        New-Item -ItemType Directory -Path (Join-Path $commonTarget 'Common Game') -Force | Out-Null
        New-Item -ItemType Junction -Path (Join-Path $commonLibrary 'steamapps\common') -Target $commonTarget | Out-Null
        [IO.File]::WriteAllText((Join-Path $commonTarget 'Common Game\Escaped.exe'), 'must never be hashed')
        $commonManifest = @'
"AppState"
{
  "appid" "280"
  "name" "Common Reparse Game"
  "StateFlags" "4"
  "installdir" "Common Game"
  "SizeOnDisk" "4"
  "buildid" "5"
}
'@
        [IO.File]::WriteAllText((Join-Path $commonLibrary 'steamapps\appmanifest_280.acf'), $commonManifest)
        $commonReparse = Invoke-SteamD6InstallProbe -LibraryRoots @($commonLibrary) -TargetAppId '280' -ObservationSeconds 1 -HashExecutables $true
        if ($commonReparse.status -ne 'blocked' -or -not ($commonReparse.blocking -contains 'steam_library_child_reparse_point')) {
            throw 'Self-test failed to reject a reparse point at steamapps/common.'
        }
        if ($commonReparse.executable_candidates.Count -ne 0) { throw 'Self-test exposed an executable through a common reparse point.' }

        $readyManifest = @'
"AppState"
{
  "appid" "300"
  "name" "Ready Game"
  "StateFlags" "4"
  "installdir" "Ready Game"
  "SizeOnDisk" "4"
  "buildid" "5"
}
'@
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\appmanifest_300.acf'), $readyManifest)
        [IO.File]::WriteAllBytes((Join-Path $temporaryRoot 'steamapps\common\Ready Game\Game.exe'), [byte[]](0, 1, 2, 3))
        $ready = Invoke-SteamD6InstallProbe -LibraryRoots @($temporaryRoot) -TargetAppId '300' -ObservationSeconds 1 -HashExecutables $true
        if ($ready.status -ne 'install_gate_passed') { throw "Self-test ready fixture blocked: $($ready.blocking -join ', ')" }
        if ($ready.executable_candidates.Count -ne 1 -or $ready.executable_candidates[0].sha256 -notmatch '^sha256:[0-9a-f]{64}$') {
            throw 'Self-test ready fixture did not produce one stable installed hash.'
        }

        New-Item -ItemType Directory -Path (Join-Path $temporaryRoot 'steamapps\common\Changing Game') -Force | Out-Null
        $changingManifest = @'
"AppState"
{
  "appid" "310"
  "name" "Changing Game"
  "StateFlags" "4"
  "installdir" "Changing Game"
  "SizeOnDisk" "4"
  "buildid" "5"
}
'@
        [IO.File]::WriteAllText((Join-Path $temporaryRoot 'steamapps\appmanifest_310.acf'), $changingManifest)
        [IO.File]::WriteAllBytes((Join-Path $temporaryRoot 'steamapps\common\Changing Game\Game.exe'), [byte[]](0, 1, 2, 3))
        $changing = Invoke-SteamD6InstallProbe `
            -LibraryRoots @($temporaryRoot) `
            -TargetAppId '310' `
            -ObservationSeconds 1 `
            -HashExecutables $true `
            -BeforeFinalSnapshot {
                param($installDirectory)
                [IO.File]::WriteAllText((Join-Path $installDirectory 'Added.exe'), 'changed during hashing')
            }
        if (
            $changing.status -ne 'blocked' -or
            $changing.executable_candidates.Count -ne 0 -or
            -not ($changing.blocking -contains 'installed_executables_changed_while_hashing')
        ) {
            throw 'Self-test failed to reject an executable tree that changed while hashing.'
        }

        'Steam D6 install gate self-test passed (download/staging, bounded metadata, unsafe path, ancestor/install-tree reparse points, hashing-time tree changes, and ready-install cases).'
    } finally {
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
    $libraryRoots = @(Get-SteamLibraryRoots -ExplicitSteamPath $SteamPath)
    $reports = @(
        foreach ($target in $AppId) {
            Invoke-SteamD6InstallProbe `
                -LibraryRoots $libraryRoots `
                -TargetAppId $target `
                -ObservationSeconds $StabilitySeconds `
                -HashExecutables (-not $SkipExecutableHashes)
        }
    )
    $reports | ConvertTo-Json -Depth 8
    if (@($reports | Where-Object { $_.status -ne 'install_gate_passed' }).Count -ne 0) { exit 2 }
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
