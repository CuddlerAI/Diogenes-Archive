param(
  [ValidateSet('lane1','lane2','lane3','lane4','bundle','audit')][string]$Part = 'lane1',
  [ValidateSet('plan','inspect','upload','resume')][string]$Mode = 'plan'
)
$ErrorActionPreference = 'Stop'
$capturePython = 'C:\Users\researcher\.cache\codex-runtimes\codex-primary-runtime\dependencies\python\python.exe'
$uploadScript = 'C:\Users\researcher\Documents\Codex\Diogenes\.agents\skills\upload-r2-high-concurrency\scripts\upload_r2.py'
$prefix = 'r2:diogenes-tempbucket/pr852-gta5-legacy-pr405-non-trainable-20260828-115407-7c6c8ac1/poc061'
$sources = @{
  lane1 = 'C:\capture\gta\replus-output\render_0037'
  lane2 = 'C:\capture\gta\replus-output\render_0038'
  lane3 = 'C:\capture\gta\replus-output\render_0039'
  lane4 = 'C:\capture\gta\replus-output\render_0040'
  bundle = 'C:\capture\gta\runs\poc-061'
  audit = 'C:\capture\gta\upload-audit\poc061'
}
$destination = if ($Part -eq 'bundle') { "$prefix/run" } elseif ($Part -eq 'audit') { "$prefix/audit" } else { "$prefix/raw/$Part" }
$uploadArgs = @($uploadScript, '--source', $sources[$Part], '--destination', $destination,
  '--report-dir', "C:\capture\gta\upload-reports\poc061-$Part", '--memory-budget-mib', '512')
if ($Mode -eq 'plan') {
  & $capturePython @uploadArgs --plan-only
  exit $LASTEXITCODE
}
# Reuse the documented existing R2 credentials in memory only. Never print values.
function Read-CaptureSecret([string]$SecretName) {
  $value = & gcloud secrets versions access latest --secret=$SecretName --project=hakkolite --quiet 2>$null
  if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace(($value -join ''))) {
    throw "Existing R2 credential unavailable: $SecretName"
  }
  return ($value -join '').Trim()
}
$captureEnvNames = @('RCLONE_CONFIG_R2_TYPE','RCLONE_CONFIG_R2_PROVIDER','RCLONE_CONFIG_R2_REGION',
  'RCLONE_CONFIG_R2_ACCESS_KEY_ID','RCLONE_CONFIG_R2_SECRET_ACCESS_KEY','RCLONE_CONFIG_R2_ENDPOINT')
try {
  $env:RCLONE_CONFIG_R2_TYPE = 's3'
  $env:RCLONE_CONFIG_R2_PROVIDER = 'Cloudflare'
  $env:RCLONE_CONFIG_R2_REGION = 'auto'
  $env:RCLONE_CONFIG_R2_ACCESS_KEY_ID = Read-CaptureSecret 'r2-training-access-key-id'
  $env:RCLONE_CONFIG_R2_SECRET_ACCESS_KEY = Read-CaptureSecret 'r2-training-secret-access-key'
  $captureAccount = Read-CaptureSecret 'r2-training-account-id'
  $env:RCLONE_CONFIG_R2_ENDPOINT = "https://$captureAccount.r2.cloudflarestorage.com"
  if ($Mode -eq 'inspect') {
    & rclone listremotes
    & rclone lsf "$prefix/" --max-depth 2 --contimeout 15s --timeout 30s
  } else {
    if ($Mode -eq 'resume') { $uploadArgs += '--resume' }
    & $capturePython @uploadArgs
  }
  if ($LASTEXITCODE -ne 0) { throw "R2 $Mode did not verify successfully (exit $LASTEXITCODE)" }
} finally {
  foreach ($captureEnvName in $captureEnvNames) {
    [Environment]::SetEnvironmentVariable($captureEnvName, $null, 'Process')
  }
  $captureAccount = $null
}
