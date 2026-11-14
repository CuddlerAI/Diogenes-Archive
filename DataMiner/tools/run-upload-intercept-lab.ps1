[CmdletBinding()]
param(
    [ValidateSet('closed', 'loopback')]
    [string]$Mode = 'closed'
)

$ErrorActionPreference = 'Stop'

$labPort = 43199
$listener = Get-NetTCPConnection -LocalPort $labPort -State Listen -ErrorAction SilentlyContinue
if ($listener) {
    throw "Port $labPort already has a listener; refusing to run the upload interception lab."
}

$dataMinerRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $dataMinerRoot

# Every client-side HTTP surface is pinned to loopback. In `closed` mode no
# listener exists; in `loopback` mode the local fake API/R2 receives and hashes
# upload bodies before discarding them. Neither mode can reach production.
$env:DATAMINER_API_BASE_URL = "http://127.0.0.1:$labPort/"
$env:DATAMINER_ACCOUNT_ENVIRONMENT = "staging"
$env:DATAMINER_ACCOUNT_API_BASE_URL = "http://127.0.0.1:$labPort/"
$env:DATAMINER_CONSOLE_API_BASE_URL = "http://127.0.0.1:$labPort/"
Remove-Item Env:DATAMINER_API_ENVIRONMENT -ErrorAction SilentlyContinue
$env:RUST_LOG = 'info'

$mockProcess = $null
try {
    if ($Mode -eq 'loopback') {
        $node = (Get-Command node -ErrorAction Stop).Source
        $labRoot = Join-Path $env:LOCALAPPDATA 'com.diogeneslab.dataminer.uploadlab\upload-lab'
        New-Item -ItemType Directory -Path $labRoot -Force | Out-Null
        $evidencePath = Join-Path $labRoot 'mock-r2-events.jsonl'
        $stdoutPath = Join-Path $labRoot 'mock-r2.stdout.log'
        $stderrPath = Join-Path $labRoot 'mock-r2.stderr.log'
        $serverScript = Join-Path $PSScriptRoot 'upload-loopback-server.mjs'
        $mockProcess = Start-Process -FilePath $node -WindowStyle Hidden -PassThru `
            -ArgumentList @($serverScript, '--host', '127.0.0.1', '--port', "$labPort", '--evidence', $evidencePath) `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath

        $ready = $false
        foreach ($attempt in 1..100) {
            if ($mockProcess.HasExited) {
                throw "The loopback server exited early. See $stderrPath"
            }
            try {
                $probe = Invoke-RestMethod -Uri "http://127.0.0.1:$labPort/lab/ready" -TimeoutSec 1
                if ($probe.ready -eq $true) {
                    $ready = $true
                    break
                }
            }
            catch {
                Start-Sleep -Milliseconds 100
            }
        }
        if (-not $ready) {
            throw 'The loopback server did not become ready within 10 seconds.'
        }
        $env:DATAMINER_ALLOW_INSECURE_LOOPBACK_R2 = '1'
        Write-Output "Local fake R2 evidence: $evidencePath"
    }
    else {
        Remove-Item Env:DATAMINER_ALLOW_INSECURE_LOOPBACK_R2 -ErrorAction SilentlyContinue
    }

    pnpm.cmd --filter '@dataminer/client' exec tauri dev `
        --features dev-catalog `
        --config ./src-tauri/tauri.dev.conf.json `
        --config ./src-tauri/tauri.upload-lab.conf.json
}
finally {
    Remove-Item Env:DATAMINER_ALLOW_INSECURE_LOOPBACK_R2 -ErrorAction SilentlyContinue
    if ($null -ne $mockProcess -and -not $mockProcess.HasExited) {
        Stop-Process -Id $mockProcess.Id -Force
    }
}
