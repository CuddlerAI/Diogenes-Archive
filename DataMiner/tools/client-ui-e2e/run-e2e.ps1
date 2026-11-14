<#
.SYNOPSIS
  Runs the DataMiner client UI end-to-end walkthrough on a real interactive Windows desktop.

.DESCRIPTION
  Must execute in an interactive session (session 1+), not session 0: a Tauri window has no
  desktop to draw on under the SYSTEM service session, and screen captures come back blank.
  On the shared cloud VM the caller registers this as a scheduled task with
  `-LogonType Interactive` and starts it, which lands it in the console session.

  Every client HTTP surface is pinned to a loopback mock. The account surface requires an
  explicit environment/base pair and never falls back to production. This release-binary
  harness names staging explicitly, so its intentional plaintext-loopback rejection exercises
  the invalid-configuration UI; real account-flow acceptance still requires HTTPS staging.

.PARAMETER Exe
  Path to the built dataminer-client.exe.
.PARAMETER Root
  Dedicated working directory for this run's logs, screenshots and evidence staging.
.PARAMETER ToolDir
  Directory holding mock-app-api.mjs and cdp-drive.mjs.
#>
param(
  [string]$Exe = "C:\dm\cargo-target\release\dataminer-client.exe",
  [string]$ExeArguments = "",
  [string]$Root = "C:\dm\e2e\run",
  [string]$ToolDir = "C:\dm\e2e\tools",
  [string]$Node = "C:\dm\node22\node.exe",
  [string]$Bundle = "C:\dm\e2e\e2e-bundle.zip",
  [string]$AppDataRoot = "",
  [int]$Port = 43199,
  [int]$DebugPort = 9222,
  [int]$CleanSlateDelaySeconds = 2,
  [int]$StartupDelaySeconds = 12,
  [int]$BundleDelaySeconds = 4,
  [switch]$SkipDesktopCapture
)

$ErrorActionPreference = "Continue"
$rootSentinelName = ".dataminer-client-ui-e2e-root"
$rootSentinelContent = "dataminer-client-ui-e2e-root-v1`n"
$rootSafetyRefusal = $false

# Root is a dedicated ephemeral evidence directory. Resolve and validate it before deleting
# any previous-run children. A non-empty directory is trusted only when it carries this
# harness's exact sentinel; never recurse against a drive root, an unresolved path, or an
# unmarked directory.
try {
  New-Item -ItemType Directory -Force -Path $Root -ErrorAction Stop | Out-Null
  $resolvedRoot = (Resolve-Path -LiteralPath $Root -ErrorAction Stop).Path.TrimEnd('\')
  $rootVolume = [System.IO.Path]::GetPathRoot($resolvedRoot).TrimEnd('\')
  if (
    [string]::IsNullOrWhiteSpace([System.IO.Path]::GetFileName($resolvedRoot)) -or
    [System.StringComparer]::OrdinalIgnoreCase.Equals($resolvedRoot, $rootVolume)
  ) {
    $rootSafetyRefusal = $true
    throw "invalid run root"
  }

  $sentinelPath = Join-Path $resolvedRoot $rootSentinelName
  $existingRootEntries = @(Get-ChildItem -LiteralPath $resolvedRoot -Force -ErrorAction Stop)
  if ($existingRootEntries.Count -eq 0) {
    [System.IO.File]::WriteAllText(
      $sentinelPath,
      $rootSentinelContent,
      [System.Text.UTF8Encoding]::new($false)
    )
  } else {
    if (
      -not (Test-Path -LiteralPath $sentinelPath -PathType Leaf) -or
      [System.IO.File]::ReadAllText($sentinelPath) -cne $rootSentinelContent
    ) {
      $rootSafetyRefusal = $true
      throw "run root sentinel missing or invalid"
    }
    foreach ($stale in $existingRootEntries) {
      if (-not [System.StringComparer]::OrdinalIgnoreCase.Equals($stale.FullName, $sentinelPath)) {
        Remove-Item -LiteralPath $stale.FullName -Recurse -Force -ErrorAction Stop
      }
    }
  }
  $remainingRootEntries = @(Get-ChildItem -LiteralPath $resolvedRoot -Force -ErrorAction Stop)
  if (
    $remainingRootEntries.Count -ne 1 -or
    -not [System.StringComparer]::OrdinalIgnoreCase.Equals(
      $remainingRootEntries[0].FullName,
      $sentinelPath
    ) -or
    [System.IO.File]::ReadAllText($sentinelPath) -cne $rootSentinelContent
  ) {
    throw "run root cleanup did not preserve its sentinel-only state"
  }
  $zip = [System.IO.Path]::GetFullPath($Bundle)
  if (Test-Path -LiteralPath $zip) {
    Remove-Item -LiteralPath $zip -Force -ErrorAction Stop
  }
  if (Test-Path -LiteralPath $zip) {
    throw "previous bundle still exists"
  }
} catch {
  if ($rootSafetyRefusal) {
    [Console]::Error.WriteLine("UNSAFE_RUN_ROOT_REFUSED")
    exit 8
  }
  [Console]::Error.WriteLine("PREVIOUS_EVIDENCE_CLEANUP_FAILED")
  exit 7
}

$node = $Node
$log = Join-Path $resolvedRoot "run.log"
function Say($message) {
  "$([DateTime]::UtcNow.ToString('o')) $message" |
    Tee-Object -FilePath $log -Append |
    Out-Null
}

$runFailures = [System.Collections.Generic.List[string]]::new()
$mock = $null
$app = $null
$transcriptStarted = $false
$forbiddenAccountUrl = "http://127.0.0.1:$Port/?credential=must-not-be-exported"

function Add-RunFailure([string]$code) {
  if (-not $runFailures.Contains($code)) {
    $runFailures.Add($code)
  }
}

function Abort-RunPhase([string]$code, [string]$publicMessage) {
  Add-RunFailure $code
  Say $publicMessage
  throw [System.OperationCanceledException]::new("controlled E2E phase abort")
}

function Stop-TrackedProcess($process, [string]$kind) {
  if (-not $process) { return }
  try {
    $process.Refresh()
    if (-not $process.HasExited) {
      $process.Kill()
    }
    if (-not $process.WaitForExit(5000)) {
      throw "tracked process did not exit"
    }
  } catch {
    Say "TRACKED_PROCESS_CLEANUP_FAILED $kind"
    Add-RunFailure "${kind}_cleanup_failed"
  }
}

function Test-LoopbackPortAvailable([int]$candidatePort) {
  $listener = $null
  try {
    $listener = [System.Net.Sockets.TcpListener]::new(
      [System.Net.IPAddress]::Loopback,
      $candidatePort
    )
    $listener.Server.ExclusiveAddressUse = $true
    $listener.Start()
    return $true
  } catch {
    return $false
  } finally {
    if ($listener) { $listener.Stop() }
  }
}

function Test-LoopbackPortListening([int]$candidatePort) {
  $client = [System.Net.Sockets.TcpClient]::new()
  try {
    $task = $client.ConnectAsync([System.Net.IPAddress]::Loopback, $candidatePort)
    return $task.Wait(250) -and $client.Connected
  } catch {
    return $false
  } finally {
    $client.Dispose()
  }
}

try {
  Start-Transcript -Path (Join-Path $resolvedRoot "transcript.log") -Force -ErrorAction Stop |
    Out-Null
  $transcriptStarted = $true
} catch {
  Add-RunFailure "transcript_start_failed"
}

Say "WHOAMI $(whoami) SESSION $((Get-Process -Id $PID).SessionId)"

# Refuse occupied ports; another run or app must never be killed to obtain them.
if ($Port -eq $DebugPort) {
  Say "PORT_CONFLICT mock and CDP ports must differ"
  if ($transcriptStarted) { try { Stop-Transcript | Out-Null } catch {} }
  exit 6
}
foreach ($portCheck in @(
  @{ Name = "mock"; Value = $Port },
  @{ Name = "cdp"; Value = $DebugPort }
)) {
  if (-not (Test-LoopbackPortAvailable $portCheck.Value)) {
    Say "PORT_IN_USE $($portCheck.Name) $($portCheck.Value)"
    if ($transcriptStarted) { try { Stop-Transcript | Out-Null } catch {} }
    exit 6
  }
}
Start-Sleep -Seconds $CleanSlateDelaySeconds

$evidence = Join-Path $resolvedRoot "app-api-requests.jsonl"
try {
  try {
    $mock = Start-Process -FilePath $node -PassThru -WindowStyle Hidden -ErrorAction Stop `
      -ArgumentList @((Join-Path $ToolDir "mock-app-api.mjs"), "--port", "$Port", "--evidence", $evidence) `
      -RedirectStandardOutput (Join-Path $resolvedRoot "mock.out.log") `
      -RedirectStandardError (Join-Path $resolvedRoot "mock.err.log")
  } catch {
    Abort-RunPhase "mock_start_failed" "MOCK_START_FAILED"
  }

  $ready = $false
  foreach ($attempt in 1..60) {
    try {
      if ((Invoke-RestMethod -Uri "http://127.0.0.1:$Port/lab/ready" -TimeoutSec 1).ready) {
        $ready = $true
        break
      }
    } catch {
      Start-Sleep -Milliseconds 250
    }
  }
  Say "MOCK_READY $ready pid=$($mock.Id)"
  if (-not $ready) {
    Abort-RunPhase "mock_not_ready" "MOCK_NOT_READY"
  }

  if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    Abort-RunPhase "client_exe_missing" "CLIENT_EXE_MISSING"
  }

  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
  $env:DATAMINER_API_BASE_URL = "http://127.0.0.1:$Port/"
  $env:DATAMINER_ACCOUNT_ENVIRONMENT = "staging"
  $env:DATAMINER_ACCOUNT_API_BASE_URL = $forbiddenAccountUrl
  $env:DATAMINER_CONSOLE_API_BASE_URL = "http://127.0.0.1:$Port/"
  $env:DATAMINER_E2E_STARTED_UNIX_MS = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds().ToString(
    [Globalization.CultureInfo]::InvariantCulture
  )
  $env:RUST_LOG = "info"
  $env:RUST_BACKTRACE = "1"

  try {
    $startParameters = @{
      FilePath = $Exe
      PassThru = $true
      WorkingDirectory = (Split-Path -Parent $Exe)
      ErrorAction = "Stop"
    }
    if (-not [string]::IsNullOrWhiteSpace($ExeArguments)) {
      $startParameters.ArgumentList = $ExeArguments
    }
    $app = Start-Process @startParameters
    Say "CLIENT_STARTED pid=$($app.Id)"
  } catch {
    Say "CLIENT_START_PRIMARY_FAILED"
  }
  if (-not $app) {
    try {
      $processInfo = [System.Diagnostics.ProcessStartInfo]::new()
      $processInfo.FileName = $Exe
      $processInfo.Arguments = $ExeArguments
      $processInfo.UseShellExecute = $false
      $processInfo.WorkingDirectory = Split-Path -Parent $Exe
      $app = [System.Diagnostics.Process]::Start($processInfo)
      Say "CLIENT_STARTED_FALLBACK pid=$($app.Id)"
    } catch {
      Say "CLIENT_START_FALLBACK_FAILED"
    }
  }
  if (-not $app) {
    Abort-RunPhase "client_start_failed" "CLIENT_START_FAILED"
  }

  Start-Sleep -Seconds $StartupDelaySeconds
  $clientExitedEarly = $false
  $listening = $false
  foreach ($attempt in 1..30) {
    $app.Refresh()
    if ($app.HasExited) {
      $clientExitedEarly = $true
      break
    }
    if (Test-LoopbackPortListening $DebugPort) {
      $listening = $true
      break
    }
    Start-Sleep -Milliseconds 100
  }
  if (-not $listening -and -not $clientExitedEarly) {
    $app.Refresh()
    $clientExitedEarly = $app.HasExited
  }

  if ($clientExitedEarly) {
    Say "CLIENT_EXITED_EARLY code=$($app.ExitCode)"
    Add-RunFailure "client_exited_early_$($app.ExitCode)"
  } else {
    Say "CLIENT_RUNNING pid=$($app.Id)"
    Say "CDP_PORT_LISTENING $listening"
    if (-not $listening) {
      Add-RunFailure "cdp_port_not_listening"
    }

    function Save-Desktop([string]$name) {
      try {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($bounds.X, $bounds.Y, 0, 0, $bitmap.Size)
        $bitmap.Save(
          (Join-Path $resolvedRoot "desktop-$name.png"),
          [System.Drawing.Imaging.ImageFormat]::Png
        )
        $graphics.Dispose()
        $bitmap.Dispose()
        Say "DESKTOP_SHOT $name"
      } catch {
        Say "DESKTOP_SHOT_FAILED $name"
        Add-RunFailure "desktop_shot_failed_$name"
      }
    }

    if ($SkipDesktopCapture) {
      Say "DESKTOP_SHOT_SKIPPED after-launch"
    } else {
      Save-Desktop "after-launch"
    }

    Say "DRIVER_START"
    & $node (Join-Path $ToolDir "cdp-drive.mjs") --port $DebugPort --out $resolvedRoot 2>&1 |
      Tee-Object -FilePath (Join-Path $resolvedRoot "driver.log") |
      Out-Null
    $driverExitCode = $LASTEXITCODE
    Say "DRIVER_EXIT $driverExitCode"
    if ($driverExitCode -ne 0) {
      Add-RunFailure "driver_exit_$driverExitCode"
    }

    if ($SkipDesktopCapture) {
      Say "DESKTOP_SHOT_SKIPPED after-drive"
    } else {
      Save-Desktop "after-drive"
    }
  }
} catch {
  if ($runFailures.Count -eq 0) {
    Add-RunFailure "run_phase_failed"
  }
  Say "RUN_PHASE_ABORTED"
} finally {
  Stop-TrackedProcess $app "client"
  Stop-TrackedProcess $mock "mock"
}

# A tracked parent can exit before its child releases a listener (WebView2 owns the CDP port in
# the real client). Wait briefly for both listeners to become exclusively bindable, but never
# kill an untracked process merely because it owns one of these ports.
foreach ($portRelease in @(
  @{ Name = "mock"; Value = $Port },
  @{ Name = "cdp"; Value = $DebugPort }
)) {
  $released = $false
  foreach ($attempt in 1..30) {
    if (Test-LoopbackPortAvailable $portRelease.Value) {
      $released = $true
      break
    }
    Start-Sleep -Milliseconds 100
  }
  if ($released) {
    Say "PORT_RELEASED $($portRelease.Name) $($portRelease.Value)"
  } else {
    Say "PORT_RELEASE_FAILED $($portRelease.Name) $($portRelease.Value)"
    Add-RunFailure "port_release_failed_$($portRelease.Name)"
  }
}

# Copy app-local text diagnostics deterministically while preserving their paths. The explicit
# AppDataRoot exists for isolated acceptance harnesses; production uses the Tauri app-local root.
$sourceAppDataRoot = if ([string]::IsNullOrWhiteSpace($AppDataRoot)) {
  Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "com.diogeneslab.dataminer"
} else {
  $AppDataRoot
}
if (Test-Path -LiteralPath $sourceAppDataRoot -PathType Container) {
  try {
    $sourceAppDataRoot = (Resolve-Path -LiteralPath $sourceAppDataRoot -ErrorAction Stop).Path.TrimEnd('\')
    $appDataPrefix = "$sourceAppDataRoot\"
    $destinationRoot = Join-Path $resolvedRoot "appdata-logs"
    $destinationPrefix = "{0}\" -f [System.IO.Path]::GetFullPath($destinationRoot).TrimEnd('\')
    New-Item -ItemType Directory -Force -Path $destinationRoot -ErrorAction Stop | Out-Null
    $appDataFiles = @(
      Get-ChildItem -LiteralPath $sourceAppDataRoot -Recurse -File -ErrorAction Stop |
        Where-Object { $_.Extension.ToLowerInvariant() -in @(".log", ".json", ".jsonl") } |
        Sort-Object FullName
    )
    foreach ($source in $appDataFiles) {
      $sourceFullPath = [System.IO.Path]::GetFullPath($source.FullName)
      if (-not $sourceFullPath.StartsWith($appDataPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "appdata source escaped its root"
      }
      $relativePath = $sourceFullPath.Substring($appDataPrefix.Length)
      $targetPath = [System.IO.Path]::GetFullPath((Join-Path $destinationRoot $relativePath))
      if (-not $targetPath.StartsWith($destinationPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "appdata destination escaped its root"
      }
      New-Item -ItemType Directory -Force -Path (Split-Path -Parent $targetPath) -ErrorAction Stop |
        Out-Null
      Copy-Item -LiteralPath $sourceFullPath -Destination $targetPath -Force -ErrorAction Stop
    }
    Say "APPDATA_COLLECTED $($appDataFiles.Count) files"
  } catch {
    Say "APPDATA_COLLECTION_FAILED"
    Add-RunFailure "appdata_collection_failed"
  }
} else {
  Say "APPDATA_ABSENT"
}

foreach ($requiredEvidence in @(
  "report.json",
  "account-status.json",
  "account-view.txt",
  "diagnostic-export.json",
  "desktop-after-launch.png",
  "desktop-after-drive.png"
)) {
  $requiredPath = Join-Path $resolvedRoot $requiredEvidence
  if (
    -not (Test-Path -LiteralPath $requiredPath -PathType Leaf) -or
    (Get-Item -LiteralPath $requiredPath -ErrorAction SilentlyContinue).Length -le 0
  ) {
    Say "EVIDENCE_MISSING $requiredEvidence"
    Add-RunFailure "missing_$requiredEvidence"
  }
}
if ($runFailures.Count -gt 0) {
  Say "E2E_FAILED_PENDING $($runFailures -join ',')"
}

if ($transcriptStarted) {
  try { Stop-Transcript | Out-Null } catch { Add-RunFailure "transcript_stop_failed" }
}

# Inspect every non-binary evidence artifact before staging. Screenshots and known media/archive
# formats are copied as opaque binary evidence; every other extension is treated as text and must
# be readable and free of both the complete invalid URL and its canary marker.
$binaryEvidenceExtensions = @(
  ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".zip", ".gz", ".mp4", ".wav", ".db", ".sqlite3"
)
$safeBundleSources = [System.Collections.Generic.List[System.IO.FileInfo]]::new()
$sensitiveEvidenceFound = $false
$unreadableTextEvidenceFound = $false
$rootPrefix = "$resolvedRoot\"
foreach ($source in @(Get-ChildItem -LiteralPath $resolvedRoot -Recurse -File | Sort-Object FullName)) {
  $sourceFullPath = [System.IO.Path]::GetFullPath($source.FullName)
  if ([System.StringComparer]::OrdinalIgnoreCase.Equals($sourceFullPath, $sentinelPath)) {
    continue
  }
  if (-not $sourceFullPath.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    $unreadableTextEvidenceFound = $true
    continue
  }
  $relativePath = $sourceFullPath.Substring($rootPrefix.Length)
  if ($source.Extension.ToLowerInvariant() -in $binaryEvidenceExtensions) {
    $safeBundleSources.Add($source)
    continue
  }
  try {
    $text = [System.IO.File]::ReadAllText($sourceFullPath)
    if (
      $relativePath.Contains("must-not-be-exported") -or
      $text.Contains("must-not-be-exported") -or
      $text.Contains($forbiddenAccountUrl)
    ) {
      $sensitiveEvidenceFound = $true
      continue
    }
    $safeBundleSources.Add($source)
  } catch {
    $unreadableTextEvidenceFound = $true
  }
}
if ($sensitiveEvidenceFound) {
  Say "SENSITIVE_EVIDENCE_REJECTED"
  Add-RunFailure "sensitive_evidence_rejected"
}
if ($unreadableTextEvidenceFound) {
  Say "TEXT_EVIDENCE_UNREADABLE"
  Add-RunFailure "text_evidence_unreadable"
}

Start-Sleep -Seconds $BundleDelaySeconds
$stage = Join-Path $resolvedRoot "_bundle"
$stageFullPath = [System.IO.Path]::GetFullPath($stage)
$stageParent = [System.IO.Path]::GetDirectoryName($stageFullPath).TrimEnd('\')
if (
  -not [System.StringComparer]::OrdinalIgnoreCase.Equals($stageParent, $resolvedRoot) -or
  [System.IO.Path]::GetFileName($stageFullPath) -cne "_bundle"
) {
  Say "BUNDLE_STAGING_PATH_REJECTED"
  Add-RunFailure "bundle_staging_path_rejected"
} else {
  $stagePrefix = "$stageFullPath\"
  try {
    New-Item -ItemType Directory -Force -Path $stageFullPath -ErrorAction Stop | Out-Null
    foreach ($source in $safeBundleSources) {
      $sourceFullPath = [System.IO.Path]::GetFullPath($source.FullName)
      $relativePath = $sourceFullPath.Substring($rootPrefix.Length)
      $targetPath = [System.IO.Path]::GetFullPath((Join-Path $stageFullPath $relativePath))
      if (-not $targetPath.StartsWith($stagePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "bundle destination escaped the staging root"
      }
      New-Item -ItemType Directory -Force -Path (Split-Path -Parent $targetPath) -ErrorAction Stop |
        Out-Null
      Copy-Item -LiteralPath $sourceFullPath -Destination $targetPath -Force -ErrorAction Stop
    }
    $zipParent = Split-Path -Parent $zip
    if ($zipParent) {
      New-Item -ItemType Directory -Force -Path $zipParent -ErrorAction Stop | Out-Null
    }
    Compress-Archive -Path (Join-Path $stageFullPath "*") -DestinationPath $zip -Force -ErrorAction Stop
    Say "BUNDLE_CREATED $($safeBundleSources.Count) files"
  } catch {
    Say "BUNDLE_FAILED"
    Add-RunFailure "bundle_failed"
  }
}
if (-not (Test-Path -LiteralPath $zip -PathType Leaf) -or (Get-Item -LiteralPath $zip).Length -le 0) {
  Say "BUNDLE_MISSING"
  Add-RunFailure "bundle_missing"
}

if ($runFailures.Count -eq 0) {
  "E2E_COMPLETE" | Out-File -FilePath (Join-Path $resolvedRoot "DONE") -Encoding utf8
  Say "E2E_COMPLETE"
  exit 0
}

Remove-Item -LiteralPath (Join-Path $resolvedRoot "DONE") -Force -ErrorAction SilentlyContinue
Say "E2E_FAILED $($runFailures -join ',')"
exit 5
