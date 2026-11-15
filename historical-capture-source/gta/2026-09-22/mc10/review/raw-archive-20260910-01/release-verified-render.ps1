# This script performs no network operations. Without -Execute it only writes a plan.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('0058','0059','0060','0061','0052','0053')][string]$Render,
    [Parameter(Mandatory)][string]$UploadReport,
    [Parameter(Mandatory)][string]$RestoreResults,
    [switch]$Execute
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$taskAudit = [IO.Path]::GetFullPath($PSScriptRoot)
$taskRoot = [IO.Path]::GetFullPath('C:/capture/gta/replus-output')
$taskRender = Join-Path $taskRoot ('render_' + $Render)
$taskReleaseDir = Join-Path $taskAudit ('release-render_' + $Render)
$taskJournalPath = Join-Path $taskReleaseDir 'journal.jsonl'
$taskReceiptPath = Join-Path $taskReleaseDir 'receipt.json'
if (Test-Path -LiteralPath $taskReleaseDir) { throw 'Existing release directory/journal; refuse replay. Audit any interrupted attempt separately.' }

function Assert-Unredirected([string]$Path) {
    $resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    $full = [IO.Path]::GetFullPath($Path)
    if (-not [string]::Equals($full, $resolved, [StringComparison]::OrdinalIgnoreCase)) { throw "Redirected path: $Path" }
    $cursor = $full
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point: $cursor" }
        $parent = Split-Path -Parent $cursor
        if (-not $parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
    return $resolved
}
function Get-Sha([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Get-Seal([string]$Path) {
    $safe = Assert-Unredirected $Path
    $item = Get-Item -LiteralPath $safe
    if ($item.PSIsContainer) { throw 'Expected regular evidence file' }
    return [ordered]@{path=$safe; bytes=[long]$item.Length; sha256=(Get-Sha $safe)}
}
function Write-NewJson([string]$Path, $Value) {
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        $data = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Depth 20) + "`n")
        $stream.Write($data, 0, $data.Length)
        $stream.Flush($true)
    } finally { $stream.Dispose() }
}
function Assert-SourceFile($Row, [switch]$Hash) {
    if ($Row.relative_path -notmatch '^(frame_\d{6}\.png|audio\.wav|assemble\.txt)$') { throw 'Unexpected source basename' }
    $expected = Join-Path $taskRender $Row.relative_path
    if (-not [string]::Equals([IO.Path]::GetFullPath($Row.original_path), $expected, [StringComparison]::OrdinalIgnoreCase)) { throw 'Inventory source outside exact render' }
    $safe = Assert-Unredirected $Row.original_path
    if (-not [string]::Equals((Split-Path -Parent $safe), $taskRender, [StringComparison]::OrdinalIgnoreCase)) { throw 'Resolved parent outside exact render' }
    $item = Get-Item -LiteralPath $safe
    if ($item.PSIsContainer -or $item.Length -ne [long]$Row.bytes) { throw "Source size/type changed: $safe" }
    if ($Hash -and (Get-Sha $safe) -cne $Row.sha256) { throw "Source SHA mismatch: $safe" }
    return $safe
}

$null = Assert-Unredirected $taskAudit
$null = Assert-Unredirected $taskRoot
$null = Assert-Unredirected $taskRender
$taskIndexPath = Join-Path $taskAudit 'index.json'
$taskIndexSha = '8a57c43f7323b654d77b8a192b3129771d118fb9b2f918cbb4a352ddd5a155d7'
if ((Get-Sha $taskIndexPath) -cne $taskIndexSha) { throw 'Sealed archive index changed' }
$taskIndex = Get-Content -LiteralPath $taskIndexPath -Raw | ConvertFrom-Json
$taskManifestPath = Join-Path $taskAudit ('render_' + $Render + '/manifest.json')
$taskManifestPins = @($taskIndex.render_manifests | Where-Object { [string]::Equals($_.path, $taskManifestPath, [StringComparison]::OrdinalIgnoreCase) })
if ($taskManifestPins.Count -ne 1) { throw 'No unique pinned render manifest' }
$taskManifestSeal = Get-Seal $taskManifestPath
if ($taskManifestSeal.sha256 -cne $taskManifestPins[0].sha256 -or $taskManifestSeal.bytes -ne $taskManifestPins[0].bytes) { throw 'Render manifest pin mismatch' }
$taskManifest = Get-Content -LiteralPath $taskManifestPath -Raw | ConvertFrom-Json
if ($taskManifest.status -cne 'PASS' -or $taskManifest.png_files -ne 1800 -or -not $taskManifest.source_stable -or -not $taskManifest.native_event_png_sha256_all_match -or $taskManifest.source_root -ne $taskRender) { throw 'Sealed RAW inventory incomplete' }
$taskFilesPath = Join-Path (Split-Path -Parent $taskManifestPath) 'files.sha256.jsonl'
if ($taskManifest.file_manifest.path -ne $taskFilesPath) { throw 'Unexpected file manifest path' }
$taskFilesSeal = Get-Seal $taskFilesPath
if ($taskFilesSeal.sha256 -cne $taskManifest.file_manifest.sha256 -or $taskFilesSeal.bytes -ne $taskManifest.file_manifest.bytes) { throw 'File inventory pin mismatch' }
$taskRows = @(Get-Content -LiteralPath $taskFilesPath | ForEach-Object { ConvertFrom-Json -InputObject $_ })
$taskByName = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
$taskTotal = [long]0
foreach ($taskRow in $taskRows) {
    if ($taskByName.ContainsKey($taskRow.relative_path)) { throw 'Duplicate inventory basename' }
    $taskByName.Add($taskRow.relative_path, $taskRow)
    $taskTotal += [long]$taskRow.bytes
}
if ($taskRows.Count -ne 1802 -or $taskRows.Count -ne $taskManifest.files -or $taskTotal -ne $taskManifest.bytes) { throw 'Inventory count/bytes mismatch' }
$taskPngRows = @($taskRows | Where-Object { $_.relative_path -match '^frame_\d{6}\.png$' } | Sort-Object relative_path)
if ($taskPngRows.Count -ne 1800) { throw 'Expected exactly 1800 PNGs' }
for ($taskFrame=0; $taskFrame -lt 1800; $taskFrame++) {
    if ($taskPngRows[$taskFrame].relative_path -cne ('frame_{0:D6}.png' -f $taskFrame)) { throw 'Missing or unexpected frame' }
}
$taskCurrent = @(Get-ChildItem -LiteralPath $taskRender -Force)
if ($taskCurrent.Count -ne $taskRows.Count) { throw 'Source directory file set changed' }
foreach ($taskItem in $taskCurrent) {
    if ($taskItem.PSIsContainer -or -not $taskByName.ContainsKey($taskItem.Name) -or ($taskItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Extra directory/file or reparse point in RAW root' }
}

$taskUploadSeal = Get-Seal $UploadReport
$taskUpload = Get-Content -LiteralPath $taskUploadSeal.path -Raw | ConvertFrom-Json
if ($taskUpload.schema -cne 'r2-high-concurrency-upload-report/v1' -or $taskUpload.status -cne 'verified' -or $taskUpload.copy_exit_code -ne 0 -or $taskUpload.check_exit_code -ne 0) { throw 'Upload helper did not verify this source' }
if (-not [string]::Equals([IO.Path]::GetFullPath($taskUpload.source), $taskRender, [StringComparison]::OrdinalIgnoreCase)) { throw 'Upload report references another source' }
if (($taskUpload.source_before | ConvertTo-Json -Depth 10 -Compress) -cne ($taskUpload.source_after | ConvertTo-Json -Depth 10 -Compress)) { throw 'Upload source changed' }
if ($taskUpload.source_after.metadata_sha256 -notmatch '^[a-f0-9]{64}$' -or $taskUpload.source_after.count -ne $taskManifest.files -or $taskUpload.source_after.bytes -ne $taskManifest.bytes -or $taskUpload.destination_after.count -ne $taskUpload.source_after.count -or $taskUpload.destination_after.bytes -ne $taskUpload.source_after.bytes) { throw 'Remote/source count and bytes mismatch' }
$taskRemote = [string]$taskUpload.destination
if ($taskRemote -notmatch '^r2:[^/:\s]+/.+[^/]$' -or $taskRemote -match '[\r\n\\]' -or @($taskRemote.Split('/') | Where-Object { $_ -in '.', '..' }).Count) { throw 'Invalid R2 object prefix' }
if (@($taskUpload.copy_command | Where-Object { $_ -in '--size-only','--ignore-checksum','--dry-run' }).Count) { throw 'Unsupported copy verification flags' }
$taskUploadDir = Split-Path -Parent $taskUploadSeal.path
if (-not [string]::Equals([IO.Path]::GetFullPath($taskUpload.report_dir), $taskUploadDir, [StringComparison]::OrdinalIgnoreCase)) { throw 'Report directory mismatch' }
$taskCheckPath = Join-Path $taskUploadDir 'rclone-check.txt'
$taskLogPath = Join-Path $taskUploadDir 'rclone.log'
$taskCheckSeal = Get-Seal $taskCheckPath
$taskLogSeal = Get-Seal $taskLogPath
$taskSeen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($taskLine in @(Get-Content -LiteralPath $taskCheckPath)) {
    if (-not $taskLine.StartsWith('= ', [StringComparison]::Ordinal)) { throw 'Full remote check contains non-matching entry' }
    $taskName = $taskLine.Substring(2)
    if (-not $taskByName.ContainsKey($taskName) -or -not $taskSeen.Add($taskName)) { throw 'Unexpected/duplicate remote check filename' }
}
if ($taskSeen.Count -ne $taskRows.Count) { throw 'Full remote check omitted files' }
foreach ($taskLine in @(Get-Content -LiteralPath $taskLogPath)) {
    if ($taskLine -match '\b0 hashes could not be checked\b') { continue }
    if ($taskLine -match '(?i)no common hash|no hash(?:es)?(?: available| found| supported| type)|hash(?:es)? (?:could not|cannot|can.t|couldn.t) be checked|(?:skip\w*|ignor\w*).{0,80}hash|hash.{0,80}(?:skip\w*|unavailable)|(?:failed to|can.t|cannot|couldn.t|could not|unable to).{0,60}(?:hash|checksum)|(?:hash|checksum).{0,60}(?:not supported|not available)|checks? (?:sizes?|size) only') { throw 'Hashless/skipped comparison in upload log' }
}

$taskRestoreSeal = Get-Seal $RestoreResults
$taskRestores = @(Get-Content -LiteralPath $taskRestoreSeal.path -Raw | ConvertFrom-Json)
if ($taskRestores.Count -ne 3) { throw 'Exactly three actual restored samples required for this render' }
$taskRestoreNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$taskRestoreProof = @()
foreach ($taskSample in $taskRestores) {
    $taskName = Split-Path -Leaf $taskSample.original_path
    if ($taskName -notmatch '^frame_\d{6}\.png$' -or -not $taskByName.ContainsKey($taskName) -or -not $taskRestoreNames.Add($taskName)) { throw 'Invalid/duplicate restore sample' }
    $taskExpected = $taskByName[$taskName]
    if ($taskSample.original_path -ne $taskExpected.original_path -or $taskSample.remote_object -cne ($taskRemote + '/' + $taskName) -or $taskSample.match -isnot [bool] -or -not $taskSample.match -or $taskSample.original_sha256 -cne $taskExpected.sha256 -or $taskSample.restored_sha256 -cne $taskExpected.sha256 -or $taskSample.bytes -ne $taskExpected.bytes) { throw 'Restore result not bound to source and remote object' }
    $taskRestored = Get-Seal $taskSample.restored_path
    if ($taskRestored.path.StartsWith($taskRoot + '\', [StringComparison]::OrdinalIgnoreCase) -or $taskRestored.bytes -ne $taskExpected.bytes -or $taskRestored.sha256 -cne $taskExpected.sha256) { throw 'Actual restored sample SHA/bytes mismatch, or sample is inside original RAW tree' }
    $taskRestoreProof += [ordered]@{original_path=$taskSample.original_path; remote_object=$taskSample.remote_object; restored_file=$taskRestored}
}

# Check every file before any deletion. Audio and assemble remain in place.
$taskPngBytes = [long]0
foreach ($taskRow in $taskRows) {
    $null = Assert-SourceFile $taskRow -Hash
    if ($taskRow.relative_path -match '^frame_\d{6}\.png$') { $taskPngBytes += [long]$taskRow.bytes }
}
if ($taskPngBytes -ne $taskManifest.png_bytes) { throw 'PNG byte total mismatch' }
foreach ($taskSeal in @($taskUploadSeal,$taskCheckSeal,$taskLogSeal,$taskRestoreSeal,$taskManifestSeal,$taskFilesSeal)) {
    if ((Get-Sha $taskSeal.path) -cne $taskSeal.sha256) { throw 'Evidence changed during validation' }
}
$taskFreeBefore = [long](Get-PSDrive -Name C).Free
$taskReceipt = [ordered]@{
    schema='dm-gta-exact-png-release/1'; started_at_utc=[DateTime]::UtcNow.ToString('o'); status='validated_plan'; execute=[bool]$Execute
    user_authorization='2026-09-10 upload all completed movies to R2 and intelligently clean local space'
    source_root=$taskRender; remote_prefix=$taskRemote; scope='Only listed 1800 PNGs; retain audio, assemble, render directory, run and evidence'
    archive_index_sha256=$taskIndexSha; render_manifest=$taskManifestSeal; file_manifest=$taskFilesSeal
    upload_report=$taskUploadSeal; full_check=$taskCheckSeal; upload_log=$taskLogSeal; no_hash_skips_observed=$true
    restored_samples=$taskRestoreProof; restore_results=$taskRestoreSeal
    expected_files=1800; expected_bytes=$taskPngBytes; free_bytes_before=$taskFreeBefore
    deleted_files=0; deleted_bytes=[long]0; remote_operations_performed=$false; formal_approval_granted=$false
}
if (-not $Execute) {
    $taskPlanPath = Join-Path $taskAudit ('plan-render_' + $Render + '-' + [Guid]::NewGuid().ToString('N') + '.json')
    Write-NewJson $taskPlanPath $taskReceipt
    [ordered]@{status='validated_plan_only'; plan_path=$taskPlanPath; png_files=1800; png_bytes=$taskPngBytes; deletion_performed=$false} | ConvertTo-Json
    return
}

# Atomic CreateNew journal prevents replay/concurrent use. No recursive deletion.
$null = New-Item -ItemType Directory -Path $taskReleaseDir -ErrorAction Stop
$taskJournal = [IO.File]::Open($taskJournalPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
$taskWriter = [IO.StreamWriter]::new($taskJournal, [Text.UTF8Encoding]::new($false))
function Journal($Value) {
    $taskWriter.WriteLine(($Value | ConvertTo-Json -Depth 8 -Compress))
    $taskWriter.Flush()
    $taskJournal.Flush($true)
}
$taskReceipt.status = 'releasing'
$taskReceipt['journal_path'] = $taskJournalPath
$taskFailure = $null
try {
    Write-NewJson (Join-Path $taskReleaseDir 'initial-receipt.json') $taskReceipt
    Journal $taskReceipt
    foreach ($taskRow in $taskPngRows) {
        # Re-check SHA and the entire literal path immediately before each removal.
        $taskSafePath = Assert-SourceFile $taskRow -Hash
        $taskAction = [ordered]@{event='delete_intent'; source_path=$taskSafePath; remote_object=($taskRemote+'/'+$taskRow.relative_path); bytes=[long]$taskRow.bytes; sha256=$taskRow.sha256; at_utc=[DateTime]::UtcNow.ToString('o')}
        Journal $taskAction
        Remove-Item -LiteralPath $taskSafePath -Force -ErrorAction Stop
        if (Test-Path -LiteralPath $taskSafePath) { throw 'PNG still exists after removal' }
        $taskReceipt.deleted_files++
        $taskReceipt.deleted_bytes += [long]$taskRow.bytes
        $taskAction.event = 'deleted'
        $taskAction.at_utc = [DateTime]::UtcNow.ToString('o')
        Journal $taskAction
    }
    $taskRemaining = @(Get-ChildItem -LiteralPath $taskRender -Force)
    if ($taskRemaining.Count -ne 2 -or @($taskRemaining | Where-Object { $_.Name -cnotin @('audio.wav','assemble.txt') }).Count) { throw 'Unexpected retained file set after release' }
    foreach ($taskName in @('audio.wav','assemble.txt')) { $null = Assert-SourceFile $taskByName[$taskName] -Hash }
    if ($taskReceipt.deleted_files -ne 1800 -or $taskReceipt.deleted_bytes -ne $taskPngBytes) { throw 'Release count mismatch' }
    $taskReceipt.status = 'completed'
} catch {
    $taskFailure = $_
    $taskReceipt.status = 'interrupted_or_failed'
    $taskReceipt['error'] = $_.Exception.Message
} finally {
    $taskReceipt['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    $taskReceipt['free_bytes_after'] = [long](Get-PSDrive -Name C).Free
    try { Journal $taskReceipt } finally { $taskWriter.Dispose() }
    Write-NewJson $taskReceiptPath $taskReceipt
}
$taskReceipt | ConvertTo-Json -Depth 12
if ($null -ne $taskFailure) { throw $taskFailure }
