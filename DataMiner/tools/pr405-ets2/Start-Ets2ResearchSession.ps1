[CmdletBinding()]
param(
    [string]$ExecutablePath = 'C:\Program Files (x86)\Steam\steamapps\common\Euro Truck Simulator 2\bin\win_x64\eurotrucks2.exe',
    [string]$ExpectedExecutableSha256 = 'b7dffe6b27402c7db6dfd52cf982cd5bf292584138b35e3eb8efb311814ab3f8',
    [ValidateRange(5, 60)]
    [int]$StartupTimeoutSeconds = 15
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$actualSha256 = (Get-FileHash -LiteralPath $resolvedExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedSha256 = $ExpectedExecutableSha256.ToLowerInvariant()
if ($actualSha256 -ne $expectedSha256) {
    # ETS2 会自动更新，这条 throw 迟早会响。**不要把 actual 抄到 expected 上放行**——
    # 那样这道闸就白设了：钉死哈希的目的正是确保取证跑在已核验的那个 build 上。
    # 正确做法：重新取证（研究 session 走一遍、确认深度/相机通路仍成立），
    # 再同步更新 -ExpectedExecutableSha256 与 catalog 里的 build 号。
    throw ("ETS2 executable hash mismatch: expected=$expectedSha256 actual=$actualSha256. " +
           "If ETS2 updated, re-run evidence collection and update both this pin and the catalog build id; " +
           "do not copy the actual hash over the expected one.")
}

$existing = @(Get-Process -Name 'eurotrucks2' -ErrorAction SilentlyContinue)
if ($existing.Count -ne 0) {
    throw "ETS2 is already running: pid=$($existing.Id -join ',')"
}

$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $resolvedExecutable
$startInfo.WorkingDirectory = Split-Path -Parent $resolvedExecutable
$startInfo.UseShellExecute = $false
$startInfo.Arguments = '-rdevice dx11 -nointro'
# 227300 = Euro Truck Simulator 2 的 Steam AppID。只设在**子进程**环境里，
# 不污染当前 shell、不留系统环境；这是 Steam 官方支持的直接启动方式
# （游戏自己读这两个变量初始化 Steamworks），不是注入、不改 Steam 客户端。
$startInfo.EnvironmentVariables['SteamAppId'] = '227300'
$startInfo.EnvironmentVariables['SteamGameId'] = '227300'

$process = [Diagnostics.Process]::Start($startInfo)
if ($null -eq $process) {
    throw 'failed to create the ETS2 renderer process'
}

$deadline = (Get-Date).AddSeconds($StartupTimeoutSeconds)
do {
    Start-Sleep -Milliseconds 250
    $process.Refresh()
    if ($process.HasExited) {
        throw "ETS2 exited during startup: pid=$($process.Id) exit_code=$($process.ExitCode)"
    }
} until ((Get-Date) -ge $deadline -or $process.MainWindowHandle -ne 0)

if ($process.MainWindowHandle -eq 0) {
    throw "ETS2 did not create a top-level window within $StartupTimeoutSeconds seconds"
}

[pscustomobject]@{
    app_id = 227300
    process_id = $process.Id
    executable_path = $resolvedExecutable
    executable_sha256 = "sha256:$actualSha256"
    graphics_argument = '-rdevice dx11'
    startup_contract = 'direct_exact_exe_with_child_scoped_steam_app_identity'
    next_step = 'Select Continue Career, verify the driving scene, then run the runtime gate and certification.'
}
