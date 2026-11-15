# Executes the already authorized, intentionally lossy disposal of six abandoned RAW attempts.
# This is NOT the remote-backup release workflow. It makes no upload or recoverability claim.
# ROOT runs this script explicitly; the authoring agent has not executed it.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$baseDirectory = [IO.Path]::GetFullPath($PSScriptRoot)
$planDirectory = Join-Path $baseDirectory 'review/failed-raw-retention-20260910-01'
$reportDirectory = Join-Path $baseDirectory 'review/abandoned-raw-discard-20260910-01'
$receiptPath = Join-Path $reportDirectory 'discard-receipt.json'
$journalPath = Join-Path $reportDirectory 'discard-journal.jsonl'
$rawParent = 'C:\capture\gta\replus-output'
$renderIds = @('0052', '0053', '0058', '0059', '0060', '0061')
$keepCounts = @{ '0052' = 79; '0053' = 79; '0058' = 84; '0059' = 83; '0060' = 82; '0061' = 97 }
$planPins = [ordered]@{
    'retention-plan.json' = '0ac13bd8a5291c3afa8a50fb931d34971b72ec5333862a0863b38a50dbf20150'
    'discard-candidates.jsonl' = 'eeace8a21e40bdb7ac41735a1d016a9f54f1e86d6d6f7f3a4176d89f1da479ef'
    'retained-files.jsonl' = 'ce69eb8894eee66b221c4a5174fc3025912ec667fadd5a633c13b5b7b23fe6ba'
    'protected-scope.json' = '4685d965a8091e97680f853ce25bf9a206af88523f8d4aa37748ba8104b400dc'
}
$utf8 = [Text.UTF8Encoding]::new($false)
$receiptStream = $null
$journalStream = $null
$removedCount = 0
[long]$removedBytes = 0
$pendingIntent = $null
$receipt = $null

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

function Assert-RawPath($Row, [bool]$Png) {
    $path = Resolve-SafePath ([string]$Row.original_path) $true
    $parent = [IO.Path]::GetDirectoryName($path)
    $render = [IO.Path]::GetFileName($parent)
    Assert-Condition ($render -match '^render_(0052|0053|0058|0059|0060|0061)$') "Outside six-render whitelist: $path"
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
    $before = Get-Item -LiteralPath $path -Force
    Assert-Condition ($before.Length -eq [long]$Row.bytes) "RAW length changed: $path"
    $mtimeNs = ([decimal]($before.LastWriteTimeUtc.Ticks - 621355968000000000L)) * 100
    Assert-Condition ($mtimeNs -eq [decimal]$Row.current_stat.mtime_ns) "RAW mtime changed: $path"
    $hash = Get-Sha256 $path
    $after = Get-Item -LiteralPath $path -Force
    Assert-Condition (($before.Length -eq $after.Length) -and ($before.LastWriteTimeUtc.Ticks -eq $after.LastWriteTimeUtc.Ticks)) "RAW changed while hashing: $path"
    Assert-Condition ($hash -ceq [string]$Row.sha256) "RAW SHA changed: $path"
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
                $snapshot.Add($item.FullName, "$($item.Length)|$($item.LastWriteTimeUtc.Ticks)")
            }
        }
    }
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
        schema = 'dm-gta-abandoned-raw-discard/1'; status = 'validating_all_sources_before_any_removal'
        started_at_utc = [DateTime]::UtcNow.ToString('o'); completed_at_utc = $null
        authorization = 'ROOT execution under the user instruction to intelligently clean local space; six abandoned/superseded attempts explicitly selected.'
        complete_remote_backup_verified = $false; exact_pixel_reconstruction_available = $false
        loss_notice = 'Unretained original PNG pixel bytes are intentionally discarded without a demonstrated complete R2 backup; replay/events/hashes do not reconstruct exact discarded pixels.'
        plan_pins = $planPins; render_whitelist = $renderIds
        expected_source_pngs = 10800; expected_retained_pngs = 504; expected_discard_pngs = 10296
        expected_discard_logical_bytes = 38540943408L; expected_retained_bytes = 1903294501L
        deleted_files = 0; deleted_logical_bytes = 0L; pending_intent = $null
        all_sources_prevalidated = $false; retained_and_sidecar_final_verification = $false
        original_events_unchanged = $false; protected_run_stat_snapshot_unchanged = $false
        formal_delivery_eligible = $false; upload_performed = $false; error = $null
    }
    Write-Receipt
    $plan = Get-Content -LiteralPath (Join-Path $planDirectory 'retention-plan.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $candidates = @(Read-JsonLines (Join-Path $planDirectory 'discard-candidates.jsonl'))
    $retained = @(Read-JsonLines (Join-Path $planDirectory 'retained-files.jsonl'))
    $protected = Get-Content -LiteralPath (Join-Path $planDirectory 'protected-scope.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert-Condition ($plan.status -ceq 'PLAN_ONLY_NO_DELETION') 'Unexpected plan schema/status'
    Assert-Condition (($candidates.Count -eq 10296) -and ($retained.Count -eq 504)) 'Partition count mismatch'
    Assert-Condition (([long](($candidates | Measure-Object -Property bytes -Sum).Sum)) -eq 38540943408L) 'Candidate bytes mismatch'
    Assert-Condition (([long](($retained | Measure-Object -Property bytes -Sum).Sum)) -eq 1903294501L) 'Retained bytes mismatch'
    Assert-Condition (@($protected.raw_ancillary_files).Count -eq 12) 'Expected exactly 12 protected audio/assemble files'
    $allRows = @($retained) + @($candidates)
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
        (Join-Path $baseDirectory 'scene01-airport/run-02'),
        (Join-Path $baseDirectory 'scene02-port/run-02'),
        (Join-Path $baseDirectory 'scene02-port/run-02-reexport-01')
    )
    Assert-Condition (@($protected.run_roots).Count -eq 3) 'Protected root count changed'
    foreach ($root in $protectedRoots) {
        Assert-Condition (@($protected.run_roots | Where-Object { [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($_.path), [IO.Path]::GetFullPath($root)) }).Count -eq 1) "Protected run root mismatch: $root"
    }
    $protectedBefore = Get-ProtectedRunSnapshot $protectedRoots
    foreach ($render in $plan.renders) { Assert-FileRecord $render.native_event }
    foreach ($sidecar in $protected.raw_ancillary_files) { $null = Assert-RawBytes $sidecar $false }
    $verified = 0
    foreach ($row in $allRows) {
        $null = Assert-RawBytes $row $true
        $verified++
        if (($verified % 1000) -eq 0) { Write-Host "Verified $verified / 10800 original PNGs; nothing removed yet." }
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
        $pendingIntent = [ordered]@{ source_path = $candidatePath; bytes = [long]$row.bytes; sha256 = [string]$row.sha256; frame = [int]$row.frame; render = [string]$row.render }
        Write-Journal ([ordered]@{ action = 'intent'; at_utc = [DateTime]::UtcNow.ToString('o'); file = $pendingIntent; complete_remote_backup_verified = $false })
        # Only this literal, validated PNG leaf is removed. No recursive or directory deletion exists.
        Remove-Item -LiteralPath $candidatePath -ErrorAction Stop -Confirm:$false
        Assert-Condition (-not (Test-Path -LiteralPath $candidatePath)) "Candidate still present after removal: $candidatePath"
        $removedCount++
        $removedBytes += [long]$row.bytes
        Write-Journal ([ordered]@{ action = 'deleted'; at_utc = [DateTime]::UtcNow.ToString('o'); file = $pendingIntent })
        $pendingIntent = $null
        if (($removedCount % 100) -eq 0) { Write-Receipt }
        if (($removedCount % 1000) -eq 0) { Write-Host "Removed $removedCount / 10296 explicitly abandoned PNGs." }
    }
    Assert-Condition (($removedCount -eq 10296) -and ($removedBytes -eq 38540943408L)) 'Final discard count/bytes mismatch'
    foreach ($row in $retained) { $null = Assert-RawBytes $row $true }
    foreach ($sidecar in $protected.raw_ancillary_files) { $null = Assert-RawBytes $sidecar $false }
    foreach ($id in $renderIds) {
        $root = Resolve-SafePath (Join-Path $rawParent "render_$id") $false
        $remaining = @(Get-ChildItem -LiteralPath $root -Force)
        Assert-Condition ($remaining.Count -eq ($keepCounts[$id] + 2)) "Remaining member count mismatch: $id"
        foreach ($file in $remaining) { Assert-Condition (($retainedSet.Contains($file.FullName)) -or ($file.Name -cin @('audio.wav', 'assemble.txt'))) "Unexpected remaining file: $($file.FullName)" }
    }
    $receipt.retained_and_sidecar_final_verification = $true
    foreach ($render in $plan.renders) { Assert-FileRecord $render.native_event }
    $receipt.original_events_unchanged = $true
    $protectedAfter = Get-ProtectedRunSnapshot $protectedRoots
    Assert-Condition ($protectedBefore.Count -eq $protectedAfter.Count) 'Protected run file membership changed'
    foreach ($key in $protectedBefore.Keys) { Assert-Condition ($protectedAfter.ContainsKey($key) -and ($protectedBefore[$key] -ceq $protectedAfter[$key])) "Protected run file state changed: $key" }
    $receipt.protected_run_stat_snapshot_unchanged = $true
    $receipt.status = 'completed_intentional_lossy_discard'
    $receipt.completed_at_utc = [DateTime]::UtcNow.ToString('o')
    Write-Receipt
    Write-Host "Completed: 10296 abandoned PNGs removed; 504 PNGs, 12 sidecars and all protected run files retained. No complete remote backup was claimed."
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
