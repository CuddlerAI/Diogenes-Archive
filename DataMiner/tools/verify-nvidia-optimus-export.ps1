[CmdletBinding()]
param([Parameter(Mandatory)][string]$BinaryPath)

# Read the PE file; never load or execute the application during verification.
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path -LiteralPath $BinaryPath).Path
$peBytes = [IO.File]::ReadAllBytes($binary)
function Read-U16([long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 2 -gt $peBytes.LongLength) { throw 'Truncated PE field' }
    [BitConverter]::ToUInt16($peBytes, [int]$Offset)
}
function Read-U32([long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 4 -gt $peBytes.LongLength) { throw 'Truncated PE field' }
    [BitConverter]::ToUInt32($peBytes, [int]$Offset)
}
if ((Read-U16 0) -ne 0x5A4D) { throw 'Not a PE executable' }
$peOffset = Read-U32 0x3C
if ((Read-U32 $peOffset) -ne 0x4550) { throw 'Invalid PE signature' }
$sectionCount = Read-U16 ($peOffset + 6)
$optionalSize = Read-U16 ($peOffset + 20)
$optional = $peOffset + 24
$magic = Read-U16 $optional
$directoryOffset = switch ($magic) { 0x20B { 112 } 0x10B { 96 } default { throw 'Unsupported PE optional header' } }
if ($optionalSize -lt $directoryOffset + 8 -or (Read-U32 ($optional + $directoryOffset - 4)) -lt 1) { throw 'PE has no export directory' }
$exportRva = Read-U32 ($optional + $directoryOffset)
$exportSize = Read-U32 ($optional + $directoryOffset + 4)
if ($exportRva -eq 0 -or $exportSize -lt 40) { throw 'Missing NvOptimusEnablement PE export' }
$sectionTable = $optional + $optionalSize
function Get-FileOffset([long]$Rva, [long]$Size = 1) {
    for ($i = 0; $i -lt $sectionCount; $i++) {
        $section = $sectionTable + 40 * $i
        $address = Read-U32 ($section + 12)
        $rawSize = Read-U32 ($section + 16)
        $rawStart = Read-U32 ($section + 20)
        $delta = $Rva - $address
        if ($delta -ge 0 -and $delta + $Size -le $rawSize) {
            $offset = [long]$rawStart + $delta
            if ($offset + $Size -gt $peBytes.LongLength) { throw 'PE section extends past file' }
            return $offset
        }
    }
    throw 'PE RVA has no file-backed section'
}
$export = Get-FileOffset $exportRva 40
$functionCount = Read-U32 ($export + 20)
$nameCount = Read-U32 ($export + 24)
$functionTable = Read-U32 ($export + 28)
$nameTable = Read-U32 ($export + 32)
$ordinalTable = Read-U32 ($export + 36)
for ($i = 0; $i -lt $nameCount; $i++) {
    $nameRva = Read-U32 (Get-FileOffset ($nameTable + 4L * $i) 4)
    $nameOffset = Get-FileOffset $nameRva
    $end = $nameOffset
    while ($end -lt $peBytes.LongLength -and $peBytes[$end] -ne 0) { $end++ }
    if ($end -eq $peBytes.LongLength) { throw 'Unterminated PE export name' }
    $name = [Text.Encoding]::ASCII.GetString($peBytes, [int]$nameOffset, [int]($end - $nameOffset))
    if ($name -ne 'NvOptimusEnablement') { continue }
    $ordinal = Read-U16 (Get-FileOffset ($ordinalTable + 2L * $i) 2)
    if ($ordinal -ge $functionCount) { throw 'Invalid export ordinal' }
    $valueRva = Read-U32 (Get-FileOffset ($functionTable + 4L * $ordinal) 4)
    if ($valueRva -ge $exportRva -and $valueRva -lt [long]$exportRva + $exportSize) { throw 'Optimus export must be data, not a forwarder' }
    if ((Read-U32 (Get-FileOffset $valueRva 4)) -ne 1) { throw 'NvOptimusEnablement must contain DWORD 1' }
    Write-Output "Verified NvOptimusEnablement=1 in PE export table: $binary"
    return
}
throw "Missing NvOptimusEnablement PE export: $binary"
