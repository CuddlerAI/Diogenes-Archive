$ErrorActionPreference = 'Stop'

$rclone = 'C:\dm\tools\rclone\rclone.exe'
$remoteRoot = 'r2:diogenes-tempbucket/issue-544-pr405-convertible-handoff-20260806-1900'
$handoff = 'C:\dm\issue-544-pr405\_handoff_pr405_convertible_20260806-1900'
$session = 'C:\Users\ARCHIVE_USER\AppData\Local\com.diogeneslab.dataminer\non-trainable-sessions\dev-1786012857141-0001'
$calibration = 'C:\Users\ARCHIVE_USER\AppData\Local\com.diogeneslab.dataminer\calibrations\6aac5819-a0bf-4b94-b9ea-b137526b6ae9'
$log = 'C:\dm\issue-544-pr405-convertible-handoff-20260806-1900-upload.log'

$env:RCLONE_CONFIG_R2_TYPE = 's3'
$env:RCLONE_CONFIG_R2_PROVIDER = 'Cloudflare'
$env:RCLONE_CONFIG_R2_REGION = 'auto'
$env:RCLONE_CONFIG_R2_ACCESS_KEY_ID = (gcloud secrets versions access latest --secret=r2-training-access-key-id --project=hakkolite 2>$null | Out-String).Trim()
$env:RCLONE_CONFIG_R2_SECRET_ACCESS_KEY = (gcloud secrets versions access latest --secret=r2-training-secret-access-key --project=hakkolite 2>$null | Out-String).Trim()
$account = (gcloud secrets versions access latest --secret=r2-training-account-id --project=hakkolite 2>$null | Out-String).Trim()
$env:RCLONE_CONFIG_R2_ENDPOINT = "https://$account.r2.cloudflarestorage.com"

$common = @(
  '--transfers', '64',
  '--checkers', '128',
  '--s3-upload-concurrency', '4',
  '--s3-chunk-size', '64M',
  '--fast-list',
  '--retries', '8',
  '--low-level-retries', '20',
  '--contimeout', '30s',
  '--timeout', '10m',
  '--stats', '30s',
  '--stats-one-line',
  '--log-file', $log,
  '--log-level', 'NOTICE'
)

function Copy-Tree([string]$source, [string]$destination) {
  & $rclone copy $source $destination @common
  if ($LASTEXITCODE -ne 0) { throw "rclone copy failed: $source -> $destination ($LASTEXITCODE)" }
}

"started_at=$((Get-Date).ToString('o'))" | Out-File -LiteralPath (Join-Path $handoff 'upload-state.txt') -Encoding utf8
Copy-Tree 'C:\dm\issue-544-pr405' "$remoteRoot/process-artifacts"
Copy-Tree $session "$remoteRoot/client-session/dev-1786012857141-0001"
Copy-Tree $calibration "$remoteRoot/calibration/6aac5819-a0bf-4b94-b9ea-b137526b6ae9"

& $rclone size $remoteRoot --json | Out-File -LiteralPath (Join-Path $handoff 'remote-size.json') -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "rclone size failed ($LASTEXITCODE)" }
& $rclone lsf $remoteRoot --recursive --files-only --format 'sp' --csv | Out-File -LiteralPath (Join-Path $handoff 'remote-objects.csv') -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "rclone lsf failed ($LASTEXITCODE)" }

"completed_at=$((Get-Date).ToString('o'))" | Add-Content -LiteralPath (Join-Path $handoff 'upload-state.txt') -Encoding utf8
Copy-Tree $handoff $remoteRoot
