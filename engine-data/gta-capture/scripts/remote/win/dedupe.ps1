# dedupe.ps1 -Groups <groups dir> -Group <name> -Delivery <dir> -Pattern <name pattern>: make each package video a hard
# link to the take's video with the same SHA-256 (stage-group-deliver copies them: 1.5 GB a group), checking the hash
# after. CAUTION: a linked file is one file -- encoding a take's lane again with ffmpeg -y rewrites it in place, the
# delivered package's copy with it: rename the take's capture\video_N.mp4 aside before a lane is encoded again.
param([Parameter(Mandatory)][string]$Groups, [Parameter(Mandatory)][string]$Group,
      [Parameter(Mandatory)][string]$Delivery, [Parameter(Mandatory)][string]$Pattern)
$ErrorActionPreference = 'Stop'
$index = @{}
foreach ($f in Get-ChildItem -Recurse -File -Filter 'video_*.mp4' (Join-Path $Groups $Group)) {
    $h = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash
    if (-not $index.ContainsKey($h)) { $index[$h] = $f.FullName }
}
$freed = 0
foreach ($pkg in Get-Item (Join-Path $Delivery $Pattern) | Where-Object { $_.PSIsContainer }) {
    foreach ($v in Get-ChildItem -LiteralPath $pkg.FullName -Filter 'video_*.mp4') {
        $h = (Get-FileHash -LiteralPath $v.FullName -Algorithm SHA256).Hash
        if (-not $index.ContainsKey($h)) { "no twin: " + $pkg.Name + '\' + $v.Name; continue }
        $src = $index[$h]
        if (((fsutil hardlink list $v.FullName) -join '|') -match [regex]::Escape($src.Substring(2))) { continue }
        $size = $v.Length
        Remove-Item -LiteralPath $v.FullName
        New-Item -ItemType HardLink -Path $v.FullName -Target $src | Out-Null
        if ((Get-FileHash -LiteralPath $v.FullName -Algorithm SHA256).Hash -ne $h) { throw "hash changed: $($v.FullName)" }
        $freed += $size
    }
}
"group $Group linked; " + [math]::Round($freed / 1GB, 2) + " GB of copies now shared"
