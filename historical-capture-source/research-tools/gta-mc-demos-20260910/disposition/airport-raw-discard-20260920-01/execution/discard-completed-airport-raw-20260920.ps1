#requires -Version 7.0
# Executes the already authorized, intentionally lossy disposal of four completed airport RAW lanes.
# This is NOT the remote-backup release workflow. It makes no upload or recoverability claim.
# Executes only the parent-delegated airport scope; no upload or game access occurs.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$baseDirectory = [IO.Path]::GetFullPath($PSScriptRoot)
$planDirectory = Join-Path $baseDirectory 'review/completed-raw-retention-20260910-01'
$reportDirectory = Join-Path $baseDirectory 'review/airport-raw-discard-20260920-01'
$receiptPath = Join-Path $reportDirectory 'discard-receipt.json'
$journalPath = Join-Path $reportDirectory 'discard-journal.jsonl'
$rawParent = 'C:\capture\gta\replus-output'
$renderIds = @('0054', '0055', '0056', '0057')
$keepCounts = @{ '0054' = 83; '0055' = 83; '0056' = 83; '0057' = 95 }
$planPins = [ordered]@{
    'retention-plan.json' = 'a47b0ae412e54dac5e79a1c8456a90ff46c4d77a70ac9665663f5d0616aafefc'
    'discard-candidates.jsonl' = 'bef30adc161234396cc019a01e46146b2668680cd7bb555d5206b68d7fee3b0f'
    'retained-files.jsonl' = '808220b5f1fcf402550bb093f9510347d45019bc0fc5e5020692ae5d09ac388e'
    'protected-scope.json' = 'feb35778951ddac6a09a90a4e334b6af97ff3e0495aa57ab7b0d8a294f81cac5'
}

$finishedPins = [ordered]@{
 'review/r2-upload-20260910-01/finished-attempt02/upload-report.json' = '9746a889f6f114d5c0fbf166db67e2e158265825de836f3746c2f1a57c1e1e6f'
 'review/r2-upload-20260910-01/finished-attempt02/rclone-check.txt' = '1de5900342d96e6db62aa32239105c3c8b0a23709474a8b795f47d86fd77660f'
 'review/r2-upload-20260910-01/finished-attempt02/rclone.log' = '102004e6ee12724cd3aa71144e1906a2ffd4e06fae7d94126b8564ddcee96b2b'
 'review/r2-upload-20260910-01/finished-independent-verification.json' = '9188cb4b2438d07062ad524d176133bc2b779bad1d2587cb7ba4540c422c93f3'
 'review/r2-delivery-inventory-20260910-01/completed-media.jsonl' = 'b242462f030ddd1d693e1fb054467e2d13805b7e9a4921d98211d2626ab5088c'
 'r2-staging-20260910-01/finished/UPLOAD-MANIFEST.jsonl' = '67072a00b9a981ae8d4012ce3ccd124a335fe7b33dec39514165f56f60eec991'
 'r2-staging-20260910-01/finished/SOURCE-RESTORE-MAPPING.jsonl' = '250acbd86d7e59535c959354b2f4b66d872549a50a2a6fb7fc73df8ca14ce7a1'
 'r2-staging-20260910-01/finished/README-INTERNAL-DEMO.md' = 'c305c793785210396e0c05aa47f6f0154c3bf763b6155e94c29b81b830fbed6c'
}

$utf8 = [Text.UTF8Encoding]::new($false)
$receiptStream = $null
$journalStream = $null
$removedCount = 0
[long]$removedBytes = 0
$pendingIntent = $null
$receipt = $null
$script:lastVerifiedRawIdentity = $null

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Resolve-SafePath([string]$LiteralPath, [bool]$Leaf) {
    Assert-Condition ([IO.Path]::IsPathRooted($LiteralPath)) "Absolute path required: $LiteralPath"
    $full = [IO.Path]::GetFullPath($LiteralPath)
    $resolved = (Resolve-Path -LiteralPath $full -ErrorAction Stop).ProviderPath
    Assert-Condition ([StringComparer]::OrdinalIgnoreCase.Equals($full, $resolved)) "Resolved path differs: $full"
    $node = $full
    while ($node) {
        $attributes = [IO.File]::GetAttributes($node)
        Assert-Condition (($attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse/symlink refused: $node"
        $parent = [IO.Path]::GetDirectoryName($node.TrimEnd('\'))
        if ($parent -eq $node) { break }
        $node = $parent
    }
    $item = Get-Item -LiteralPath $full -Force -ErrorAction Stop
    Assert-Condition ($item.PSIsContainer -ne $Leaf) "Unexpected file/directory kind: $full"
    return $full
}

function Get-Sha256([string]$LiteralPath) {
    return (Get-FileHash -LiteralPath $LiteralPath -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
}

function Assert-FileRecord($Record) {
    $path = Resolve-SafePath ([string]$Record.path) $true
    $before = Get-Item -LiteralPath $path -Force
    Assert-Condition ($before.Length -eq [long]$Record.bytes) "Length mismatch: $path"
    $hash = Get-Sha256 $path
    $after = Get-Item -LiteralPath $path -Force
    Assert-Condition (($before.Length -eq $after.Length) -and ($before.LastWriteTimeUtc.Ticks -eq $after.LastWriteTimeUtc.Ticks)) "File changed during SHA read: $path"
    Assert-Condition ($hash -ceq [string]$Record.sha256) "SHA mismatch: $path"
}

function Get-RawIdentity([string]$LiteralPath) {
    # FILE_ID_INFO supplies the same 64-bit volume serial / 128-bit ID used by Python stat.
    # The native handle has FILE_READ_ATTRIBUTES only; it cannot write or remove a file.
    if (-not ('AirportRawFileIdentity20260920' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class AirportRawFileIdentity20260920 {
    [StructLayout(LayoutKind.Sequential)] public struct Ft { public uint Low, High; }
    [StructLayout(LayoutKind.Sequential)] public struct HandleInfo {
        public uint Attributes; public Ft Creation, Access, Write;
        public uint IgnoredVolume32, SizeHigh, SizeLow, Links, IgnoredIdHigh, IgnoredIdLow;
    }
    [StructLayout(LayoutKind.Sequential)] public struct FileIdInfo { public ulong Volume64, IdLow64, IdHigh64; }
    public sealed class Observation {
        public ulong Device, IdLow, IdHigh; public uint Nlink;
        public long Bytes, CreationFiletime, WriteFiletime;
    }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileInformationByHandle(SafeFileHandle handle, out HandleInfo info);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileInformationByHandleEx(SafeFileHandle handle, int infoClass, out FileIdInfo info, uint size);
    public static Observation Read(string path) {
        using (var h = CreateFileW(path, 0x80, 7, IntPtr.Zero, 3, 0x00200000, IntPtr.Zero)) {
            if (h.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
            HandleInfo basic; FileIdInfo id;
            if (!GetFileInformationByHandle(h, out basic)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if (!GetFileInformationByHandleEx(h, 18, out id, 24)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if ((basic.Attributes & 0x400) != 0) throw new InvalidOperationException("Reparse handle refused");
            return new Observation { Device=id.Volume64, IdLow=id.IdLow64, IdHigh=id.IdHigh64,
                Nlink=basic.Links, Bytes=((long)basic.SizeHigh << 32) | basic.SizeLow,
                CreationFiletime=((long)basic.Creation.High << 32) | basic.Creation.Low,
                WriteFiletime=((long)basic.Write.High << 32) | basic.Write.Low };
        }
    }
}
'@
    }
    $nativeInfo = [AirportRawFileIdentity20260920]::Read($LiteralPath)
    $fileId = [System.Numerics.BigInteger]$nativeInfo.IdLow + ([System.Numerics.BigInteger]$nativeInfo.IdHigh * [System.Numerics.BigInteger]::Pow(2,64))
    return [pscustomobject]@{
        device = $nativeInfo.Device.ToString(); file_id = $fileId.ToString(); nlink = $nativeInfo.Nlink
        bytes = $nativeInfo.Bytes
        ctime_ns = (([decimal]$nativeInfo.CreationFiletime - 116444736000000000L) * 100).ToString('0')
        mtime_ns = (([decimal]$nativeInfo.WriteFiletime - 116444736000000000L) * 100).ToString('0')
    }
}

function Assert-SameRawIdentity($Actual, $Expected, [string]$Path) {
    foreach ($field in @('device','file_id','nlink','bytes','ctime_ns','mtime_ns')) {
        Assert-Condition ([string]$Actual.$field -ceq [string]$Expected.$field) "RAW identity $field changed: $Path"
    }
}

function Assert-RawPath($Row, [bool]$Png) {
    $path = Resolve-SafePath ([string]$Row.original_path) $true
    $parent = [IO.Path]::GetDirectoryName($path)
    $render = [IO.Path]::GetFileName($parent)
    Assert-Condition ($render -match '^render_(0054|0055|0056|0057)$') "Outside four-airport-render whitelist: $path"
    Assert-Condition ([StringComparer]::OrdinalIgnoreCase.Equals($parent, (Join-Path $rawParent $render))) "RAW parent mismatch: $path"
    Assert-Condition ([string]$Row.render -ceq $render) "Row render mismatch: $path"
    $name = [IO.Path]::GetFileName($path)
    if ($Png) {
        Assert-Condition ($name -match '^frame_(\d{6})\.png$') "Only exact PNG leaves may be removed: $path"
        $frame = [int]$Matches[1]
        Assert-Condition (($frame -ge 0) -and ($frame -lt 1800) -and ($frame -eq [int]$Row.frame)) "Frame index mismatch: $path"
        Assert-Condition ([string]$Row.relative_path -ceq $name) "Relative name mismatch: $path"
    } else {
        Assert-Condition ($name -cin @('audio.wav', 'assemble.txt')) "Unexpected protected RAW sidecar: $path"
    }
    return $path
}

function Assert-RawBytes($Row, [bool]$Png) {
    $path = Assert-RawPath $Row $Png
    $identityBefore = Get-RawIdentity $path
    Assert-SameRawIdentity $identityBefore $Row.current_stat $path
    if ($Png) { Assert-Condition ($identityBefore.nlink -eq 1) "RAW must have exactly one link: $path" }
    $before = Get-Item -LiteralPath $path -Force
    if ($Png) {
        Assert-Condition ($null -ne $before.PSObject.Properties['LinkType']) "RAW link status unavailable: $path"
        Assert-Condition ([string]::IsNullOrEmpty([string]$before.LinkType)) "Linked RAW refused; reclaimed space would be uncertain: $path"
    }
    Assert-Condition ($before.Length -eq [long]$Row.bytes) "RAW length changed: $path"
    $mtimeNs = ([decimal]($before.LastWriteTimeUtc.Ticks - 621355968000000000L)) * 100
    Assert-Condition ($mtimeNs -eq [decimal]$Row.current_stat.mtime_ns) "RAW mtime changed: $path"
    $hash = Get-Sha256 $path
    $after = Get-Item -LiteralPath $path -Force
    $identityAfter = Get-RawIdentity $path
    Assert-SameRawIdentity $identityAfter $identityBefore $path
    Assert-SameRawIdentity $identityAfter $Row.current_stat $path
    if ($Png) { Assert-Condition ([string]::IsNullOrEmpty([string]$after.LinkType)) "RAW became linked while hashing: $path" }
    Assert-Condition (($before.Length -eq $after.Length) -and ($before.LastWriteTimeUtc.Ticks -eq $after.LastWriteTimeUtc.Ticks)) "RAW changed while hashing: $path"
    Assert-Condition ($hash -ceq [string]$Row.sha256) "RAW SHA changed: $path"
    $script:lastVerifiedRawIdentity = $identityAfter
    return $path
}

function Read-JsonLines([string]$LiteralPath) {
    foreach ($line in [IO.File]::ReadLines($LiteralPath, $utf8)) {
        if (-not [string]::IsNullOrWhiteSpace($line)) { ConvertFrom-Json -InputObject $line }
    }
}

function Write-Receipt {
    if ($null -eq $receiptStream) { return }
    $receipt.deleted_files = $removedCount
    $receipt.deleted_logical_bytes = $removedBytes
    $receipt.pending_intent = $pendingIntent
    $payload = $utf8.GetBytes(($receipt | ConvertTo-Json -Depth 12) + "`n")
    $null = $receiptStream.Seek(0, [IO.SeekOrigin]::Begin)
    $receiptStream.SetLength(0)
    $receiptStream.Write($payload, 0, $payload.Length)
    $receiptStream.Flush($true)
}

function Write-Journal($Entry) {
    $payload = $utf8.GetBytes(($Entry | ConvertTo-Json -Depth 8 -Compress) + "`n")
    $journalStream.Write($payload, 0, $payload.Length)
    $journalStream.Flush($true)
}

function Get-ProtectedRunSnapshot([string[]]$Roots) {
    $snapshot = @{}
    foreach ($root in $Roots) {
        $safeRoot = Resolve-SafePath $root $false
        foreach ($item in Get-ChildItem -LiteralPath $safeRoot -Force -Recurse -ErrorAction Stop) {
            Assert-Condition (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Protected run has reparse item: $($item.FullName)"
            if (-not $item.PSIsContainer) {
                $snapshot.Add($item.FullName, ((Get-RawIdentity $item.FullName) | ConvertTo-Json -Compress))
            }
        }
    }
    return $snapshot
}

function Assert-FinishedPackage {
    foreach ($entry in $finishedPins.GetEnumerator()) {
        $safe = Resolve-SafePath (Join-Path $baseDirectory $entry.Key) $true
        Assert-Condition ((Get-Sha256 $safe) -ceq $entry.Value) "Finished-package fixed evidence changed: $($entry.Key)"
    }
    $stage = Resolve-SafePath (Join-Path $baseDirectory 'r2-staging-20260910-01/finished') $false
    $audit = Join-Path $baseDirectory 'review/r2-upload-20260910-01'
    $upload = Get-Content -LiteralPath (Join-Path $audit 'finished-attempt02/upload-report.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $verified = Get-Content -LiteralPath (Join-Path $audit 'finished-independent-verification.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($upload.status -ceq 'verified' -and $upload.copy_exit_code -eq 0 -and $upload.check_exit_code -eq 0) 'Finished R2 upload is not verified'
    Assert-Condition ($upload.destination -ceq 'r2:diogenes-tempbucket/gta-mc-demos-20260910/finished') 'Finished remote prefix changed'
    Assert-Condition ([StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($upload.source),$stage)) 'Finished upload source changed'
    foreach ($state in @($upload.source_before,$upload.source_after,$upload.destination_after)) {
        Assert-Condition ($state.count -eq 218 -and $state.bytes -eq 2476060486L) 'Finished upload count/bytes changed'
    }
    Assert-Condition ($upload.source_before.metadata_sha256 -ceq $upload.source_after.metadata_sha256) 'Finished upload source was unstable'
    Assert-Condition ($verified.status -ceq 'PASS' -and @($verified.failed).Count -eq 0 -and $verified.passed -eq 23) 'Independent finished verification is not PASS'
    foreach ($check in $verified.checks.PSObject.Properties) { Assert-Condition ($check.Value -eq $true) "Independent finished check failed: $($check.Name)" }
    $checkLines = @([IO.File]::ReadAllLines((Join-Path $audit 'finished-attempt02/rclone-check.txt'),$utf8))
    Assert-Condition ($checkLines.Count -eq 218) 'Expected 218 finished remote equality checks'
    $checkedNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($line in $checkLines) {
        Assert-Condition ($line.StartsWith('= ')) "R2 equality check contains nonmatching result: $line"
        Assert-Condition ($checkedNames.Add($line.Substring(2))) 'Duplicate remote equality result'
    }
    $log = [IO.File]::ReadAllText((Join-Path $audit 'finished-attempt02/rclone.log'),$utf8)
    Assert-Condition ($log -match '0 differences found' -and $log -match '218 matching files') 'Finished check completion not found'
    Assert-Condition ($log -notmatch '(?i)no common hash|hash.*(skipp|not supported)|checksum.*(skipp|not supported)') 'Finished check skipped content hashes'
    $stageFiles = @(Get-ChildItem -LiteralPath $stage -File -Recurse -Force)
    Assert-Condition ($stageFiles.Count -eq 218 -and [long](($stageFiles | Measure-Object Length -Sum).Sum) -eq 2476060486L) 'Finished stage membership changed'
    foreach ($file in $stageFiles) {
        $safe = Resolve-SafePath $file.FullName $true
        $relative = $safe.Substring($stage.Length+1).Replace('\','/')
        Assert-Condition ($checkedNames.Contains($relative)) "Local finished object not remotely verified: $relative"
    }
    $mapping = @(Read-JsonLines (Join-Path $stage 'SOURCE-RESTORE-MAPPING.jsonl'))
    $manifest = @(Read-JsonLines (Join-Path $stage 'UPLOAD-MANIFEST.jsonl'))
    $media = @(Read-JsonLines (Join-Path $baseDirectory 'review/r2-delivery-inventory-20260910-01/completed-media.jsonl'))
    Assert-Condition ($mapping.Count -eq 215 -and $manifest.Count -eq 215 -and $media.Count -eq 15) 'Finished source inventory membership changed'
    $mapped = @{}
    foreach ($row in $mapping) {
        Assert-Condition (-not $mapped.ContainsKey($row.object_relative_path)) 'Duplicate finished mapping key'
        $expectedStage = [IO.Path]::GetFullPath((Join-Path $stage $row.object_relative_path))
        Assert-Condition ($expectedStage.StartsWith($stage+'\',[StringComparison]::OrdinalIgnoreCase)) 'Finished object escapes stage'
        Assert-Condition ([StringComparer]::OrdinalIgnoreCase.Equals($expectedStage,[IO.Path]::GetFullPath($row.staged_path))) 'Finished mapped path changed'
        $bound = @($manifest | Where-Object { $_.object_relative_path -ceq $row.object_relative_path })
        Assert-Condition ($bound.Count -eq 1 -and $bound[0].sha256 -ceq $row.sha256 -and $bound[0].bytes -eq $row.bytes -and [StringComparer]::OrdinalIgnoreCase.Equals($bound[0].path,$row.source_path)) 'Finished manifest/mapping differ'
        Assert-FileRecord ([pscustomobject]@{path=$row.source_path; bytes=$row.bytes; sha256=$row.sha256})
        Assert-FileRecord ([pscustomobject]@{path=$row.staged_path; bytes=$row.bytes; sha256=$row.sha256})
        $mapped.Add($row.object_relative_path,$row)
    }
    foreach ($video in $media) {
        Assert-Condition ($video.path.EndsWith('.mp4',[StringComparison]::OrdinalIgnoreCase) -and $mapped.ContainsKey($video.object_relative_path)) 'Unexpected completed media'
        $row = $mapped[$video.object_relative_path]
        Assert-Condition ($row.sha256 -ceq $video.sha256 -and $row.bytes -eq $video.bytes -and [StringComparer]::OrdinalIgnoreCase.Equals($row.source_path,$video.path)) 'Actual MP4 source differs from frozen inventory'
    }
    foreach ($scene in @('beach','airport','sandy')) {
        $videos = @($media | Where-Object { $_.scene -ceq $scene })
        Assert-Condition ($videos.Count -eq 5 -and @($videos | Where-Object { $_.category -ceq 'lane_video' }).Count -eq 4) 'Each completed scene must retain four lane MP4s plus quad'
        $runManifest = Get-Content -LiteralPath $mapped["$scene/run-manifest.json"].source_path -Raw -Encoding UTF8 | ConvertFrom-Json
        Assert-Condition ($runManifest.schema -ceq 'dm-gta-run-manifest/1' -and $runManifest.dry_run -eq $false -and $runManifest.completed_at -and $runManifest.delivery_eligible -eq $false) 'Completed internal run seal changed'
        Assert-Condition (@($runManifest.videos).Count -eq 4 -and @($runManifest.metadata_files).Count -eq 8) 'Completed run file counts changed'
        foreach ($v in $runManifest.videos) {
            Assert-Condition (@($videos | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals($_.path,$v.path) -and $_.sha256 -ceq $v.sha256 -and $_.bytes -eq $v.bytes }).Count -eq 1) 'Sealed run video differs from protected MP4'
        }
        $quad = Get-Content -LiteralPath $mapped["$scene/preview/review-media-manifest.json"].source_path -Raw -Encoding UTF8 | ConvertFrom-Json
        Assert-Condition ($quad.status -ceq 'completed' -and $quad.source_videos_unchanged -eq $true -and $quad.preview_probe_verified.frame_count -eq 1800 -and $quad.preview_probe_verified.duration_s -eq 60) 'Completed quad seal/probe changed'
        $quadVideos = @($quad.outputs | Where-Object { $_.path.EndsWith('.mp4',[StringComparison]::OrdinalIgnoreCase) })
        Assert-Condition ($quadVideos.Count -eq 1) 'Exactly one final quad MP4 required'
        $v = $quadVideos[0]
        Assert-Condition (@($videos | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals($_.path,$v.path) -and $_.sha256 -ceq $v.sha256 -and $_.bytes -eq $v.bytes }).Count -eq 1) 'Sealed quad differs from protected MP4'
    }
}


$northPins = [ordered]@{
 'STAGING-RECEIPT.json' = 'd4e932dbcd31cb581105d226f6db580c63ed9c322e2551d52cc2956dd19e37dc'
 'UPLOAD-MANIFEST.jsonl' = 'becb80c04f137d72ae84c06df24a875ac49a34f85edab069111b2e174449f084'
 'SOURCE-RESTORE-MAPPING.jsonl' = '13bc96ef439b69bdec07914516e44ec1ca26bd0dec8f9a3ab07ecffc41e27e06'
 'SCENE-REGISTRY.json' = 'e557581b6df2426b95e5fab4e9a111ede5c283142e78588e0c6ea7b6f63336d8'
 'README-INTERNAL-DEMO.md' = 'b1df5ecb0bae1d7d273c0e4e601a2c69a9607ebdb1ed10fff9809040aca6ebb3'
}
function Write-NewJson([string]$Path, $Value) {
    $bytes = $utf8.GetBytes(($Value | ConvertTo-Json -Depth 16) + "`n")
    $stream = [IO.FileStream]::new($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
    try { $stream.Write($bytes,0,$bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
}
function Assert-NorthPackage {
    $stage = Resolve-SafePath (Join-Path $baseDirectory 'r2-staging-20260920-01/finished-north-beach') $false
    foreach ($pin in $northPins.GetEnumerator()) {
        Assert-Condition ((Get-Sha256 (Resolve-SafePath (Join-Path $stage $pin.Key) $true)) -ceq $pin.Value) "North package pin changed: $($pin.Key)"
    }
    $seal = Get-Content -LiteralPath (Join-Path $stage 'STAGING-RECEIPT.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($seal.schema -ceq 'dm-gta-single-scene-frozen-staging/1' -and $seal.status -ceq 'prepared_not_uploaded' -and $seal.source_stable -eq $true -and $seal.copied_video_count -eq 5 -and $seal.source_files -eq 135) 'North staging seal differs'
    Assert-Condition ($seal.destination -ceq 'r2:diogenes-tempbucket/gta-mc-demos-20260910/finished-20260920-north-beach') 'North destination mapping changed'
    $mapping = @(Read-JsonLines (Join-Path $stage 'SOURCE-RESTORE-MAPPING.jsonl'))
    $manifest = @(Read-JsonLines (Join-Path $stage 'UPLOAD-MANIFEST.jsonl'))
    Assert-Condition ($mapping.Count -eq 135 -and $manifest.Count -eq 135) 'North mapping must cover 135 real source files'
    $mapped = @{}
    foreach ($row in $mapping) {
        Assert-Condition (-not $mapped.ContainsKey($row.object_relative_path)) 'Duplicate north object mapping'
        $target = [IO.Path]::GetFullPath((Join-Path $stage $row.object_relative_path))
        Assert-Condition ($target.StartsWith($stage+'\',[StringComparison]::OrdinalIgnoreCase) -and [StringComparer]::OrdinalIgnoreCase.Equals($target,[IO.Path]::GetFullPath($row.staged_path))) 'North object escapes stage or path differs'
        Assert-Condition (-not ([IO.Path]::GetFullPath($row.source_path)).StartsWith($rawParent+'\',[StringComparison]::OrdinalIgnoreCase)) 'North protection inventory must not traverse current RAW'
        $bound = @($manifest | Where-Object { $_.object_relative_path -ceq $row.object_relative_path })
        Assert-Condition ($bound.Count -eq 1 -and $bound[0].sha256 -ceq $row.sha256 -and $bound[0].bytes -eq $row.bytes -and [StringComparer]::OrdinalIgnoreCase.Equals($bound[0].path,$row.source_path)) 'North source mapping differs'
        Assert-FileRecord ([pscustomobject]@{path=$row.source_path;bytes=$row.bytes;sha256=$row.sha256})
        Assert-FileRecord ([pscustomobject]@{path=$row.staged_path;bytes=$row.bytes;sha256=$row.sha256})
        $mapped.Add($row.object_relative_path,$row)
    }
    $stageFiles = @(Get-ChildItem -LiteralPath $stage -File -Recurse -Force)
    Assert-Condition ($stageFiles.Count -eq 140 -and [long](($stageFiles | Measure-Object Length -Sum).Sum) -eq 680081093L -and @($seal.files).Count -eq 139) 'North stage closed membership differs'
    $sealedPaths = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($file in $seal.files) {
        $full = [IO.Path]::GetFullPath($file.path)
        Assert-Condition ($full.StartsWith($stage+'\',[StringComparison]::OrdinalIgnoreCase) -and $sealedPaths.Add($full)) 'North stage sealed path escapes or duplicates'
        Assert-FileRecord $file
    }
    $null = $sealedPaths.Add((Join-Path $stage 'STAGING-RECEIPT.json'))
    foreach ($file in $stageFiles) { Assert-Condition ($sealedPaths.Contains((Resolve-SafePath $file.FullName $true))) 'North stage has unsealed member' }
    $videos = @($mapping | Where-Object { $_.source_path.EndsWith('.mp4',[StringComparison]::OrdinalIgnoreCase) })
    Assert-Condition ($videos.Count -eq 5) 'North five final videos must remain'
    $run = Get-Content -LiteralPath $mapped['north-beach/run-manifest.json'].source_path -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($run.schema -ceq 'dm-gta-run-manifest/1' -and $run.dry_run -eq $false -and $run.completed_at -and $run.delivery_eligible -eq $false -and @($run.videos).Count -eq 4 -and @($run.metadata_files).Count -eq 8) 'North final run seal differs'
    foreach ($video in $run.videos) { Assert-Condition (@($videos | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals($_.source_path,$video.path) -and $_.sha256 -ceq $video.sha256 -and $_.bytes -eq $video.bytes }).Count -eq 1) 'North video not bound to final run' }
    $quad = Get-Content -LiteralPath $mapped['north-beach/preview/review-media-manifest.json'].source_path -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($quad.status -ceq 'completed' -and $quad.source_videos_unchanged -eq $true -and $quad.preview_probe_verified.frame_count -eq 1800 -and $quad.preview_probe_verified.duration_s -eq 60) 'North final quad differs'
    $quadVideos = @($quad.outputs | Where-Object { $_.path.EndsWith('.mp4',[StringComparison]::OrdinalIgnoreCase) })
    Assert-Condition ($quadVideos.Count -eq 1) 'North quad count differs'
    $v = $quadVideos[0]
    Assert-Condition (@($videos | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals($_.source_path,$v.path) -and $_.sha256 -ceq $v.sha256 -and $_.bytes -eq $v.bytes }).Count -eq 1) 'North quad not in protected mapping'
}
function Get-MappedSourcesSnapshot {
    $snapshot = @{}
    foreach ($relative in @('r2-staging-20260910-01/finished/SOURCE-RESTORE-MAPPING.jsonl','r2-staging-20260920-01/finished-north-beach/SOURCE-RESTORE-MAPPING.jsonl')) {
        foreach ($row in @(Read-JsonLines (Join-Path $baseDirectory $relative))) {
            $path = Resolve-SafePath $row.source_path $true
            Assert-Condition (-not $path.StartsWith($rawParent+'\',[StringComparison]::OrdinalIgnoreCase)) 'Protected source unexpectedly references RAW'
            Assert-Condition (-not $snapshot.ContainsKey($path)) 'Duplicate protected source'
            $snapshot.Add($path, [ordered]@{bytes=[long]$row.bytes;sha256=[string]$row.sha256;identity=(Get-RawIdentity $path)})
        }
    }
    Assert-Condition ($snapshot.Count -eq 350) 'Expected 215 old plus 135 new protected sources'
    return $snapshot
}

try {
    # Pinned inputs and output reservation are independent from the old full-backup release plan.
    $null = Resolve-SafePath $planDirectory $false
    $null = Resolve-SafePath (Split-Path -Parent $reportDirectory) $false
    foreach ($entry in $planPins.GetEnumerator()) {
        $path = Resolve-SafePath (Join-Path $planDirectory $entry.Key) $true
        Assert-Condition ((Get-Sha256 $path) -ceq $entry.Value) "Fixed plan pin mismatch: $($entry.Key)"
    }
    Assert-Condition (-not (Test-Path -LiteralPath $reportDirectory)) "Report directory already exists; no replay/resume is allowed: $reportDirectory"
    $null = [IO.Directory]::CreateDirectory($reportDirectory)
    $null = Resolve-SafePath $reportDirectory $false
    $receiptStream = [IO.FileStream]::new($receiptPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $journalStream = [IO.FileStream]::new($journalPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $receipt = [ordered]@{
        schema = 'dm-gta-completed-airport-raw-discard/1'; status = 'validating_all_sources_before_any_removal'
        started_at_utc = [DateTime]::UtcNow.ToString('o'); completed_at_utc = $null
        authorization = 'ROOT execution under the user instruction to intelligently clean local space; only original completed airport render_0054..0057 selected for representative RAW retention.'
        complete_remote_backup_verified = $false; exact_pixel_reconstruction_available = $false
        loss_notice = 'Unretained original PNG pixel bytes are intentionally discarded without a demonstrated complete R2 backup; replay/events/hashes do not reconstruct exact discarded pixels.'
        plan_pins = $planPins; render_whitelist = $renderIds
        expected_source_pngs = 7200; expected_retained_pngs = 344; expected_discard_pngs = 6856
        expected_discard_logical_bytes = 24671764858L; expected_retained_bytes = 1249189662L
        deleted_files = 0; deleted_logical_bytes = 0L; pending_intent = $null
        all_sources_prevalidated = $false; retained_and_sidecar_final_verification = $false
        original_events_unchanged = $false; protected_run_stat_snapshot_unchanged = $false
        formal_delivery_eligible = $false; upload_performed = $false; error = $null
        finished_package_upload_verified_at_utc = $null; finished_package_verified_again_now = $false
        local_20_mp4_sha_verified = $false; finished_package_local_final_verification = $false
        north_package_local_final_verification = $false; mapped_350_sources_identity_unchanged = $false
        free_bytes_before = (Get-Volume -DriveLetter C).SizeRemaining; free_bytes_after = $null
        free_bytes_delta_not_exclusive_to_this_task = $null
        north_package_pins = $northPins; helper_sha256 = (Get-Sha256 $PSCommandPath)
        sidecars_pre_discard_sha256 = @(); finished_evidence_pins = $finishedPins
    }
    Write-Receipt
    $plan = Get-Content -LiteralPath (Join-Path $planDirectory 'retention-plan.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $candidateAll = @(Read-JsonLines (Join-Path $planDirectory 'discard-candidates.jsonl'))
    $candidates = @($candidateAll | Where-Object { $_.render -cin @('render_0054','render_0055','render_0056','render_0057') })
    $retainedAll = @(Read-JsonLines (Join-Path $planDirectory 'retained-files.jsonl'))
    $retained = @($retainedAll | Where-Object { $_.render -cin @('render_0054','render_0055','render_0056','render_0057') })
    $protected = Get-Content -LiteralPath (Join-Path $planDirectory 'protected-scope.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($plan.status -ceq 'PLAN_ONLY_WAITING_FINISHED_PACKAGE_VERIFIED') 'Unexpected plan schema/status'
    Assert-Condition (($candidates.Count -eq 6856) -and ($retained.Count -eq 344)) 'Partition count mismatch'
    Assert-Condition (([long](($candidates | Measure-Object -Property bytes -Sum).Sum)) -eq 24671764858L) 'Candidate bytes mismatch'
    Assert-Condition (([long](($retained | Measure-Object -Property bytes -Sum).Sum)) -eq 1249189662L) 'Retained bytes mismatch'
    $allRows = @($retained) + @($candidates)
    $selectedRenders = @($plan.renders | Where-Object { $_.render -cin @('render_0054','render_0055','render_0056','render_0057') })
    Assert-Condition ($selectedRenders.Count -eq 4) 'Exactly four airport render descriptors required'
    Assert-Condition (@($allRows | Where-Object { $_.scene -cne 'airport' }).Count -eq 0) 'Non-airport row refused'
    $sidecarRecords = @()
    $selectedSidecars = @($protected.raw_directory_ancillary_files | Where-Object { $_.render -cin @('render_0054','render_0055','render_0056','render_0057') })
    Assert-Condition ($selectedSidecars.Count -eq 8) 'Expected exactly eight protected audio/assemble files'
    foreach ($sidecar in $selectedSidecars) {
        $row = [pscustomobject]@{ original_path = $sidecar.path; render = $sidecar.render; bytes = $sidecar.stat.bytes; current_stat = $sidecar.stat; sha256 = '' }
        $safe = Assert-RawPath $row $false
        $row.sha256 = Get-Sha256 $safe
        $null = Assert-RawBytes $row $false
        $sidecarRecords += $row
    }
    $receipt.sidecars_pre_discard_sha256 = $sidecarRecords
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $retainedSet = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($row in $allRows) {
        $path = Assert-RawPath $row $true
        Assert-Condition ($seen.Add($path)) "Duplicate or overlapping partition: $path"
        Assert-Condition (($row.sha256 -cmatch '^[a-f0-9]{64}$') -and ([long]$row.bytes -gt 0)) "Malformed SHA/length: $path"
        Assert-Condition (($row.native_event.output_frame -eq $row.frame) -and ($row.native_event.output_bytes -eq $row.bytes) -and ($row.native_event.output_sha256 -ceq $row.sha256)) "Sealed event row binding differs: $path"
    }
    foreach ($row in $retained) { $null = $retainedSet.Add([IO.Path]::GetFullPath($row.original_path)) }
    foreach ($id in $renderIds) {
        foreach ($frame in (@(0..59 | ForEach-Object { $_ * 30 }) + @(1799,259,260,261,262,263,264,265,266,857,1206,1251,900))) {
            Assert-Condition ($retainedSet.Contains((Join-Path (Join-Path $rawParent "render_$id") ('frame_{0:d6}.png' -f $frame)))) "Required sample missing: $id/$frame"
        }
        $descriptor = @($selectedRenders | Where-Object { $_.render -ceq "render_$id" })
        Assert-Condition ($descriptor.Count -eq 1 -and @($descriptor[0].retained_frames).Count -eq $keepCounts[$id]) 'Pinned retained frame list differs'
        foreach ($frame in $descriptor[0].retained_frames) { Assert-Condition ($retainedSet.Contains((Join-Path (Join-Path $rawParent "render_$id") ('frame_{0:d6}.png' -f $frame)))) "Pinned keep frame missing: $id/$frame" }
        $root = Resolve-SafePath (Join-Path $rawParent "render_$id") $false
        $localRows = @($allRows | Where-Object { $_.render -ceq "render_$id" })
        Assert-Condition ($localRows.Count -eq 1800) "Render partition must cover 1800 PNGs: $id"
        Assert-Condition (@($retained | Where-Object { $_.render -ceq "render_$id" }).Count -eq $keepCounts[$id]) "Keep count mismatch: $id"
        foreach ($frame in 0..1799) { Assert-Condition ($seen.Contains((Join-Path $root ('frame_{0:d6}.png' -f $frame)))) "Missing partition frame: $id/$frame" }
        $existing = @(Get-ChildItem -LiteralPath $root -Force)
        Assert-Condition ($existing.Count -eq 1802) "Unexpected RAW directory members: $id"
        foreach ($file in $existing) {
            Assert-Condition (-not $file.PSIsContainer) "Unexpected subdirectory in RAW: $($file.FullName)"
            Assert-Condition (($seen.Contains($file.FullName)) -or ($file.Name -cin @('audio.wav', 'assemble.txt'))) "Unexpected RAW member: $($file.FullName)"
        }
    }
    $protectedRoots = @(
        (Join-Path $baseDirectory 'scene01-airport/run-03'),
        (Join-Path $baseDirectory 'scene03-sandy-airfield/run-02'),
        ([IO.Path]::GetFullPath((Join-Path $baseDirectory '../mc2-20260908/run-mc2-beach-002-reexport-02')))
    )
    Assert-Condition (@($protected.whole_run_trees).Count -eq 3) 'Protected root count changed'
    foreach ($root in $protectedRoots) {
        Assert-Condition (@($protected.whole_run_trees | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($_), [IO.Path]::GetFullPath($root)) }).Count -eq 1) "Protected run root mismatch: $root"
    }
    Assert-FinishedPackage
    $receipt.finished_package_upload_verified_at_utc = '2026-09-10T06:13:28.010818+00:00'
    Assert-NorthPackage
    $receipt.local_20_mp4_sha_verified = $true
    $mappedBefore = Get-MappedSourcesSnapshot
    Write-NewJson (Join-Path $reportDirectory 'protected-mapped-sources-before.json') $mappedBefore
    $protectedRoots += @((Join-Path $baseDirectory 'scene06-north-beach/run-02'), (Join-Path $baseDirectory 'r2-staging-20260910-01/finished'), (Join-Path $baseDirectory 'r2-staging-20260920-01/finished-north-beach'))
    # This verifies the preserved upload evidence and current local bytes, not a new network check.
    $protectedBefore = Get-ProtectedRunSnapshot $protectedRoots
    Write-NewJson (Join-Path $reportDirectory 'protected-tree-before.json') $protectedBefore
    foreach ($render in $selectedRenders) {
        Assert-FileRecord $render.native_event
        $events = @(Read-JsonLines $render.native_event.path)
        Assert-Condition ($events.Count -eq 1800) 'Sealed airport lane must have 1800 events'
        $rowsByFrame = @{}
        foreach ($row in @($allRows | Where-Object { $_.render -ceq $render.render })) { $rowsByFrame.Add([int]$row.frame,$row) }
        foreach ($event in $events) {
            $frame = [int]$event.output_frame
            Assert-Condition ($rowsByFrame.ContainsKey($frame)) 'Sealed event missing from RAW partition'
            $row = $rowsByFrame[$frame]
            Assert-Condition ([StringComparer]::OrdinalIgnoreCase.Equals($event.output_path,$row.original_path) -and $event.output_bytes -eq $row.bytes -and $event.output_sha256 -ceq $row.sha256 -and $event.replay_sha256 -ceq $row.native_event.replay_sha256 -and $event.editor_binding.session_id -ceq $row.native_event.editor_session_id) 'Actual sealed event row differs from plan'
            $rowsByFrame.Remove($frame)
        }
        Assert-Condition ($rowsByFrame.Count -eq 0) 'Unbound RAW partition rows remain'
    }
    foreach ($sidecar in $sidecarRecords) { $null = Assert-RawBytes $sidecar $false }
    $verified = 0
    foreach ($row in $allRows) {
        $null = Assert-RawBytes $row $true
        $verified++
        if (($verified % 1000) -eq 0) { Write-Host "Verified $verified / 7200 original PNGs; nothing removed yet." }
    }
    # Repeat small input pins after the full preflight to detect any concurrent plan change.
    foreach ($entry in $planPins.GetEnumerator()) { Assert-Condition ((Get-Sha256 (Join-Path $planDirectory $entry.Key)) -ceq $entry.Value) "Plan changed during validation: $($entry.Key)" }
    $receipt.all_sources_prevalidated = $true
    $receipt.status = 'discarding_exact_candidates'
    Write-Receipt
    foreach ($row in $candidates) {
        # Resolve/reparse, size/mtime and actual SHA are rechecked immediately before each removal.
        $candidatePath = Assert-RawBytes $row $true
        Assert-Condition (-not $retainedSet.Contains($candidatePath)) "Refusing retained frame: $candidatePath"
        $pendingIntent = [ordered]@{ source_path = $candidatePath; bytes = [long]$row.bytes; sha256 = [string]$row.sha256; frame = [int]$row.frame; render = [string]$row.render; identity = $script:lastVerifiedRawIdentity }
        Write-Journal ([ordered]@{ action = 'intent'; at_utc = [DateTime]::UtcNow.ToString('o'); file = $pendingIntent; complete_remote_backup_verified = $false })
        # Only this literal, validated PNG leaf is removed. No recursive or directory deletion exists.
        $null = Assert-RawPath $row $true
        Assert-SameRawIdentity (Get-RawIdentity $candidatePath) $pendingIntent.identity $candidatePath
        Remove-Item -LiteralPath $candidatePath -ErrorAction Stop -Confirm:$false
        Assert-Condition (-not (Test-Path -LiteralPath $candidatePath)) "Candidate still present after removal: $candidatePath"
        $removedCount++
        $removedBytes += [long]$row.bytes
        Write-Journal ([ordered]@{ action = 'deleted'; at_utc = [DateTime]::UtcNow.ToString('o'); file = $pendingIntent })
        $pendingIntent = $null
        if (($removedCount % 100) -eq 0) { Write-Receipt }
        if (($removedCount % 1000) -eq 0) { Write-Host "Removed $removedCount / 6856 completed airport PNGs." }
    }
    Assert-Condition (($removedCount -eq 6856) -and ($removedBytes -eq 24671764858L)) 'Final discard count/bytes mismatch'
    foreach ($row in $retained) { $null = Assert-RawBytes $row $true }
    foreach ($sidecar in $sidecarRecords) { $null = Assert-RawBytes $sidecar $false }
    foreach ($id in $renderIds) {
        foreach ($frame in (@(0..59 | ForEach-Object { $_ * 30 }) + @(1799,259,260,261,262,263,264,265,266,857,1206,1251,900))) {
            Assert-Condition ($retainedSet.Contains((Join-Path (Join-Path $rawParent "render_$id") ('frame_{0:d6}.png' -f $frame)))) "Required sample missing: $id/$frame"
        }
        $descriptor = @($selectedRenders | Where-Object { $_.render -ceq "render_$id" })
        Assert-Condition ($descriptor.Count -eq 1 -and @($descriptor[0].retained_frames).Count -eq $keepCounts[$id]) 'Pinned retained frame list differs'
        foreach ($frame in $descriptor[0].retained_frames) { Assert-Condition ($retainedSet.Contains((Join-Path (Join-Path $rawParent "render_$id") ('frame_{0:d6}.png' -f $frame)))) "Pinned keep frame missing: $id/$frame" }
        $root = Resolve-SafePath (Join-Path $rawParent "render_$id") $false
        $remaining = @(Get-ChildItem -LiteralPath $root -Force)
        Assert-Condition ($remaining.Count -eq ($keepCounts[$id] + 2)) "Remaining member count mismatch: $id"
        foreach ($file in $remaining) { Assert-Condition (($retainedSet.Contains($file.FullName)) -or ($file.Name -cin @('audio.wav', 'assemble.txt'))) "Unexpected remaining file: $($file.FullName)" }
    }
    $receipt.retained_and_sidecar_final_verification = $true
    foreach ($render in $selectedRenders) { Assert-FileRecord $render.native_event }
    $receipt.original_events_unchanged = $true
    $protectedAfter = Get-ProtectedRunSnapshot $protectedRoots
    Write-NewJson (Join-Path $reportDirectory 'protected-tree-after.json') $protectedAfter
    Assert-Condition ($protectedBefore.Count -eq $protectedAfter.Count) 'Protected run file membership changed'
    foreach ($key in $protectedBefore.Keys) { Assert-Condition ($protectedAfter.ContainsKey($key) -and ($protectedBefore[$key] -ceq $protectedAfter[$key])) "Protected run file state changed: $key" }
    $receipt.protected_run_stat_snapshot_unchanged = $true
    Assert-FinishedPackage
    $receipt.finished_package_local_final_verification = $true
    Assert-NorthPackage
    $receipt.north_package_local_final_verification = $true
    $mappedAfter = Get-MappedSourcesSnapshot
    Write-NewJson (Join-Path $reportDirectory 'protected-mapped-sources-after.json') $mappedAfter
    Assert-Condition ($mappedBefore.Count -eq $mappedAfter.Count) 'Mapped protected source count changed'
    foreach ($key in $mappedBefore.Keys) {
        Assert-Condition ($mappedAfter.ContainsKey($key)) 'Mapped protected source disappeared'
        Assert-SameRawIdentity $mappedAfter[$key].identity $mappedBefore[$key].identity $key
    }
    $receipt.mapped_350_sources_identity_unchanged = $true
    $receipt.free_bytes_after = (Get-Volume -DriveLetter C).SizeRemaining
    $receipt.free_bytes_delta_not_exclusive_to_this_task = [long]$receipt.free_bytes_after - [long]$receipt.free_bytes_before
    $receipt.status = 'completed_intentional_lossy_discard'
    $receipt.completed_at_utc = [DateTime]::UtcNow.ToString('o')
    Write-Receipt
    Write-Host "Completed: 6856 airport PNGs removed; 344 PNGs, 8 sidecars and all protected run files retained. No complete remote backup was claimed."
    Write-Host "Receipt: $receiptPath"
} catch {
    if ($null -ne $receipt) {
        $receipt.status = if ($null -ne $pendingIntent) {
            'failed_with_pending_intent_deletion_outcome_unknown'
        } elseif ($removedCount -gt 0) {
            'failed_after_partial_discard_no_automatic_resume'
        } else {
            'failed_before_any_discard'
        }
        $receipt.error = $_.Exception.Message
        $receipt.completed_at_utc = [DateTime]::UtcNow.ToString('o')
        Write-Receipt
    }
    throw
} finally {
    if ($null -ne $journalStream) { $journalStream.Dispose() }
    if ($null -ne $receiptStream) { $receiptStream.Dispose() }
}
