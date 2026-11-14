param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'Transition-Evidence.ps1')
if(Test-Path -LiteralPath $OutputDirectory){throw 'Tests require a new directory'}
$root=(New-Item -ItemType Directory -Path $OutputDirectory).FullName
$checks=[Collections.Generic.List[string]]::new()
function Check([bool]$Condition,[string]$Name){if(-not $Condition){throw "FAILED $Name"};$checks.Add($Name)}
function Rejected([scriptblock]$Action){try{& $Action;return $false}catch{return $true}}
$launch='a'*32
$receiptPath=Join-Path $root 'camera-authoring-receipt-01.json'
$receipt=@{schema='dm-cp2077-camera-authoring-window-receipt/v2';request=@{process_launch_id=$launch;ordinal=1};qpc_frequency=[Diagnostics.Stopwatch]::Frequency;window_deadline_qpc=[Diagnostics.Stopwatch]::GetTimestamp()+10*[Diagnostics.Stopwatch]::Frequency}
$receipt|ConvertTo-Json -Depth 5|Set-Content -LiteralPath $receiptPath
Check (Rejected {Publish-AuthoringGuidance $root $launch 1 90}) 'early FOV guidance rejected'
Check (-not(Test-Path -LiteralPath (Join-Path $root 'camera-authoring-guide-01.json'))) 'early guidance leaves no published request'
$receipt.window_deadline_qpc=[Diagnostics.Stopwatch]::GetTimestamp()-1
$receipt|ConvertTo-Json -Depth 5|Set-Content -LiteralPath $receiptPath
Check (Rejected {Publish-AuthoringGuidance $root ('b'*32) 1 90}) 'wrong launch rejected'
Publish-AuthoringGuidance $root $launch 1 90
$guide=Get-Content -Raw -LiteralPath (Join-Path $root 'camera-authoring-guide-01.json')|ConvertFrom-Json
Check ($guide.completed_ordinal -eq 1 -and $guide.next_fov -eq 90) 'completed phase permits exact next FOV guidance'
Check (-not(Test-Path -LiteralPath (Join-Path $root 'camera-authoring-guide-01.json.prepared'))) 'guidance publication is atomic rename'
Check (Rejected {Publish-AuthoringGuidance $root $launch 1 90}) 'replay rejected'
$trace=New-AuthoringTrace (Join-Path $root 'trace.jsonl') $launch
try {
 Write-AuthoringTrace $trace 'test' @{observed_fov=90}
 $trace.Count=2000
 Check (Rejected {Write-AuthoringTrace $trace 'overflow' @{}}) 'trace overflow is explicit'
} finally {$trace.Writer.Dispose()}
$row=Get-Content -Raw -LiteralPath (Join-Path $root 'trace.jsonl')|ConvertFrom-Json
Check ($row.process_launch_id -ceq $launch -and $row.qpc -gt 0 -and $row.qpc_frequency -eq [Diagnostics.Stopwatch]::Frequency) 'trace preserves same-launch monotonic clock'
foreach($file in Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.ps1'){
 $tokens=$null;$errors=$null
 [void][Management.Automation.Language.Parser]::ParseFile($file.FullName,[ref]$tokens,[ref]$errors)
 Check ($errors.Count -eq 0) ("parses "+$file.Name)
}
$result=@{synthetic=$true;game_launched=$false;checks=$checks;passed=$checks.Count}
$result|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $root 'result.json')
$result|ConvertTo-Json -Depth 5
