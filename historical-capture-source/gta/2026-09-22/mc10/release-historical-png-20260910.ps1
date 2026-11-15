$ErrorActionPreference = 'Stop'
$taskBase = [IO.Path]::GetFullPath($PSScriptRoot)
$taskAudit = Join-Path $taskBase 'review/historical-r2-verification-20260910-01'
$taskList = Join-Path $taskAudit 'local-files.jsonl'
$taskReceipt = Join-Path $taskAudit 'local-release-receipt.json'
$taskJournal = Join-Path $taskAudit 'local-release-journal.jsonl'
if ((Test-Path -LiteralPath $taskReceipt) -or (Test-Path -LiteralPath $taskJournal)) { throw 'Existing release result; refusing replay' }
if ((Get-FileHash -LiteralPath $taskList -Algorithm SHA256).Hash.ToLowerInvariant() -ne '8677303f1876bb9d6159c5b1b39390893eb8bfeeb23afd66506aef278ea7b010') { throw 'Inventory changed' }
$taskInventory = Get-Content -LiteralPath (Join-Path $taskAudit 'local-inventory.json') -Raw | ConvertFrom-Json
if ($taskInventory.status -ne 'LOCAL_SHA256_AND_EXISTING_REMOTE_MD5_REPORTS_PASS' -or -not $taskInventory.no_hash_skips_observed -or -not $taskInventory.all_source_identities_and_stable_fields_unchanged) { throw 'Inventory verification missing' }
$taskRestores = @(Get-Content -LiteralPath (Join-Path $taskAudit 'restore-results.json') -Raw | ConvertFrom-Json)
if ($taskRestores.Count -ne 12 -or @($taskRestores | Where-Object { -not $_.match -or $_.original_sha256 -ne $_.restored_sha256 }).Count) { throw 'Restore verification missing' }
foreach ($taskLane in 1..4) {
    $taskCheck = Get-Content -LiteralPath (Join-Path $taskAudit "lane$taskLane.json") -Raw | ConvertFrom-Json
    $taskLines = @(Get-Content -LiteralPath (Join-Path $taskAudit "lane$taskLane-check.txt"))
    if ($taskCheck.check_exit_code -ne 0 -or $taskCheck.local.count -ne 452 -or $taskCheck.remote.count -ne 452 -or $taskCheck.local.bytes -ne $taskCheck.remote.bytes -or $taskLines.Count -ne 452 -or @($taskLines | Where-Object { -not $_.StartsWith('= ') }).Count) { throw 'Remote verification missing' }
}
$taskRoot = (Resolve-Path -LiteralPath 'C:/capture/gta/replus-output').Path.TrimEnd('\')
$taskRows = @(Get-Content -LiteralPath $taskList | ForEach-Object { ConvertFrom-Json -InputObject $_ } | Where-Object { $_.relative_path -match '^frame_\d{6}\.png$' })
if ($taskRows.Count -ne 1800) { throw 'Unexpected release file count' }
$taskExpectedBytes = [long]0
foreach ($taskRow in $taskRows) {
    $taskResolved = (Resolve-Path -LiteralPath $taskRow.source_path).Path
    $taskParent = Split-Path -Parent $taskResolved
    $taskDirName = Split-Path -Leaf $taskParent
    if (-not $taskResolved.StartsWith($taskRoot + '\', [StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Parent $taskParent) -ne $taskRoot -or $taskDirName -notmatch '^render_00(37|38|39|40)$') { throw "Release path outside exact roots: $taskResolved" }
    $taskItem = Get-Item -LiteralPath $taskResolved
    $taskDir = Get-Item -LiteralPath $taskParent
    if (($taskItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or ($taskDir.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $taskItem.Length -ne $taskRow.bytes) { throw 'Source state mismatch' }
    if ((Get-FileHash -LiteralPath $taskResolved -Algorithm SHA256).Hash.ToLowerInvariant() -ne $taskRow.sha256) { throw "Source SHA mismatch: $taskResolved" }
    $taskExpectedBytes += [long]$taskRow.bytes
}
$taskBefore = (Get-PSDrive -Name C).Free
$taskRelease = [ordered]@{ schema='dm-gta-local-release/1'; started_at=[DateTime]::UtcNow.ToString('o'); status='releasing'; user_authorization='2026-09-10 upload completed movies to R2 and intelligently clean local space'; scope='Only 1800 PNG files in render_0037..0040, verified against existing R2 backup; keep directories, audio, assemble, local evidence and finished videos'; source_inventory_sha256='8677303f1876bb9d6159c5b1b39390893eb8bfeeb23afd66506aef278ea7b010'; restored_samples=12; expected_files=1800; expected_bytes=$taskExpectedBytes; free_bytes_before=$taskBefore; deleted_files=0; deleted_bytes=[long]0 }
$taskRelease | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $taskReceipt -Encoding UTF8
foreach ($taskRow in $taskRows) {
    Remove-Item -LiteralPath $taskRow.source_path -Force
    [ordered]@{source_path=$taskRow.source_path; bytes=$taskRow.bytes; sha256=$taskRow.sha256; remote_path=$taskRow.remote_path; deleted_at=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json -Compress | Add-Content -LiteralPath $taskJournal -Encoding UTF8
    $taskRelease.deleted_files++
    $taskRelease.deleted_bytes += [long]$taskRow.bytes
}
$taskRelease.status = 'completed'
$taskRelease['finished_at'] = [DateTime]::UtcNow.ToString('o')
$taskRelease['free_bytes_after'] = (Get-PSDrive -Name C).Free
$taskRelease | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $taskReceipt -Encoding UTF8
$taskRelease | ConvertTo-Json -Depth 5
