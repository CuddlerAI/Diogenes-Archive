[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$InputProfilePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedInputProfileSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$VideoConfigPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedVideoConfigSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [string]$LauncherPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedLauncherSha256,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:Schema = 'dm-mafia3-d6-local-profile-evidence/v1'
$script:AppId = '360430'
$script:BuildId = '5121098'
$script:SupportedLauncherSha256 = 'sha256:a1a8c67f2024fd7613fce513eb0cb339d1642fe95ec38b5ff04af20345f6dde4'
$script:XmlClose = '</C_SaveDataUnwrapped>'
$script:UnsetElement = [uint32]::MaxValue
$script:MaxInputProfileBytes = 1MB
$script:MaxVideoConfigBytes = 1KB
$script:MaxLauncherBytes = 96MB

# These are direct tuples from the persisted ControllerDeviceType=2 control set. The validator deliberately
# preserves the game's logical names instead of inventing DataMiner taxonomy. Contextual actions can
# share one device/element tuple; choosing one catalog semantic per tuple remains a separate authoring step.
$script:CoreBindings = @(
    [pscustomobject]@{ Logical = 'UP'; Device = 2; Element = 17; Mode = 32 },
    [pscustomobject]@{ Logical = 'DOWN'; Device = 2; Element = 31; Mode = 32 },
    [pscustomobject]@{ Logical = 'LEFT'; Device = 2; Element = 30; Mode = 32 },
    [pscustomobject]@{ Logical = 'RIGHT'; Device = 2; Element = 32; Mode = 32 },
    [pscustomobject]@{ Logical = 'AIM_X'; Device = 1; Element = 300; Mode = 1 },
    [pscustomobject]@{ Logical = 'AIM_Y'; Device = 1; Element = 301; Mode = 1 },
    [pscustomobject]@{ Logical = 'RUN'; Device = 2; Element = 42; Mode = 32 },
    [pscustomobject]@{ Logical = 'SPRINT'; Device = 2; Element = 42; Mode = 64 },
    [pscustomobject]@{ Logical = 'CLIMB'; Device = 2; Element = 57; Mode = 32 },
    [pscustomobject]@{ Logical = 'STALK'; Device = 2; Element = 45; Mode = 32 },
    [pscustomobject]@{ Logical = 'COVER_ENTER'; Device = 2; Element = 29; Mode = 32 },
    [pscustomobject]@{ Logical = 'FIRE'; Device = 1; Element = 400; Mode = 32 },
    [pscustomobject]@{ Logical = 'ZOOM'; Device = 1; Element = 401; Mode = 32 },
    [pscustomobject]@{ Logical = 'RELOAD'; Device = 2; Element = 19; Mode = 32 },
    [pscustomobject]@{ Logical = 'ACTION'; Device = 2; Element = 18; Mode = 32 },
    [pscustomobject]@{ Logical = 'PICKUP_BODY'; Device = 2; Element = 21; Mode = 32 },
    [pscustomobject]@{ Logical = 'GENERIC_ACTION_1'; Device = 2; Element = 33; Mode = 32 },
    [pscustomobject]@{ Logical = 'WEAPON_GRENADE_AIM'; Device = 2; Element = 34; Mode = 32 },
    [pscustomobject]@{ Logical = 'HOLSTER'; Device = 2; Element = 35; Mode = 32 },
    [pscustomobject]@{ Logical = 'SWITCH_EXPLOSIVES'; Device = 2; Element = 44; Mode = 32 },
    [pscustomobject]@{ Logical = 'USE_MEDKIT'; Device = 2; Element = 46; Mode = 32 },
    [pscustomobject]@{ Logical = 'INTEL_VIEW_OPEN'; Device = 2; Element = 47; Mode = 32 },
    [pscustomobject]@{ Logical = 'MAP'; Device = 2; Element = 15; Mode = 32 },
    [pscustomobject]@{ Logical = 'MENU'; Device = 2; Element = 1; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_UP'; Device = 2; Element = 17; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_DOWN'; Device = 2; Element = 31; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_LEFT'; Device = 2; Element = 30; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_RIGHT'; Device = 2; Element = 32; Mode = 32 },
    [pscustomobject]@{ Logical = 'HANDBRAKE'; Device = 2; Element = 57; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_FIRE'; Device = 1; Element = 400; Mode = 32 },
    [pscustomobject]@{ Logical = 'CAR_EXIT'; Device = 2; Element = 18; Mode = 32 }
)

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        return 'sha256:' + ([BitConverter]::ToString($hasher.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $hasher.Dispose()
    }
}

function Get-BoundedFileFingerprint {
    param(
        [Parameter(Mandatory = $true)][string]$ResolvedPath,
        [Parameter(Mandatory = $true)][long]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $stream = [IO.File]::Open($ResolvedPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -gt $MaxBytes) { throw "$Label exceeds the $MaxBytes-byte validation cap." }
        $length = $stream.Length
        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $hash = 'sha256:' + ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
        } finally {
            $hasher.Dispose()
        }
        if ($stream.Length -ne $length) { throw "$Label changed while it was being hashed." }
        return [pscustomobject]@{ Sha256 = $hash; Length = $length }
    } finally {
        $stream.Dispose()
    }
}

function Read-BoundedFileBytes {
    param(
        [Parameter(Mandatory = $true)][string]$ResolvedPath,
        [Parameter(Mandatory = $true)][long]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $stream = [IO.File]::Open($ResolvedPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -gt $MaxBytes -or $stream.Length -gt [int]::MaxValue) {
            throw "$Label exceeds the $MaxBytes-byte validation cap."
        }
        $expectedLength = [int]$stream.Length
        $bytes = New-Object byte[] $expectedLength
        $offset = 0
        while ($offset -lt $expectedLength) {
            $read = $stream.Read($bytes, $offset, $expectedLength - $offset)
            if ($read -le 0) { throw "$Label changed while it was being read." }
            $offset += $read
        }
        if ($stream.ReadByte() -ne -1 -or $stream.Length -ne $expectedLength) {
            throw "$Label changed while it was being read."
        }
        return [byte[]]$bytes
    } finally {
        $stream.Dispose()
    }
}

function Read-StableFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][long]$MaxBytes,
        [switch]$CaptureBytes
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label is not a file: $Path" }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if ($CaptureBytes) {
        $firstInfo = Get-BoundedFileFingerprint $resolved $MaxBytes $Label
        if ($firstInfo.Sha256 -cne $ExpectedSha256) {
            throw "$Label SHA-256 mismatch (expected $ExpectedSha256, got $($firstInfo.Sha256))."
        }
        $bytes = Read-BoundedFileBytes $resolved $MaxBytes $Label
        $secondHash = Get-Sha256 $bytes
        if ($secondHash -cne $firstInfo.Sha256 -or $bytes.LongLength -ne $firstInfo.Length) {
            throw "$Label changed while it was being read."
        }
        return [pscustomobject]@{ Bytes = $bytes; Sha256 = $secondHash; Length = $bytes.LongLength }
    }
    $firstInfo = Get-BoundedFileFingerprint $resolved $MaxBytes $Label
    if ($firstInfo.Sha256 -cne $ExpectedSha256) {
        throw "$Label SHA-256 mismatch (expected $ExpectedSha256, got $($firstInfo.Sha256))."
    }
    $secondInfo = Get-BoundedFileFingerprint $resolved $MaxBytes $Label
    if ($secondInfo.Sha256 -cne $firstInfo.Sha256 -or $secondInfo.Length -ne $firstInfo.Length) {
        throw "$Label changed while it was being hashed."
    }
    return [pscustomobject]@{ Sha256 = $secondInfo.Sha256; Length = $secondInfo.Length }
}

function ConvertTo-UInt32Exact {
    param([AllowNull()]$Value, [Parameter(Mandatory = $true)][string]$Label)
    $parsed = [uint32]0
    if ($null -eq $Value -or -not [uint32]::TryParse(
        [string]$Value,
        [Globalization.NumberStyles]::None,
        [Globalization.CultureInfo]::InvariantCulture,
        [ref]$parsed)) {
        throw "$Label is not a canonical unsigned decimal integer."
    }
    if ([string]$parsed -cne [string]$Value) { throw "$Label is not canonical decimal text." }
    return $parsed
}

function Get-ControlTuple {
    param(
        [Parameter(Mandatory = $true)]$Group,
        [Parameter(Mandatory = $true)][ValidateSet('m_Primary', 'm_Secondary')][string]$Slot,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $container = $Group.SelectSingleNode($Slot)
    if ($null -eq $container) { throw "$Label is missing $Slot." }
    $nodes = @($container.SelectNodes('S_Control'))
    if ($nodes.Count -ne 1) { throw "$Label.$Slot must contain exactly one S_Control." }
    $control = $nodes[0]
    foreach ($field in @('m_ControlMode', 'm_ControlElement', 'm_DeviceType', 'm_LongHoldTime', 'm_LogicalControl')) {
        if ($null -eq $control.SelectSingleNode($field)) { throw "$Label.$Slot is missing $field." }
    }
    return [pscustomobject]@{
        Mode = ConvertTo-UInt32Exact $control.m_ControlMode "$Label.$Slot.m_ControlMode"
        Element = ConvertTo-UInt32Exact $control.m_ControlElement "$Label.$Slot.m_ControlElement"
        Device = ConvertTo-UInt32Exact $control.m_DeviceType "$Label.$Slot.m_DeviceType"
    }
}

function Read-InputProfileBytes {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    if ($Bytes.LongLength -gt $script:MaxInputProfileBytes) {
        throw "input profile exceeds the $($script:MaxInputProfileBytes)-byte validation cap."
    }
    $closeBytes = (New-Object Text.ASCIIEncoding).GetBytes($script:XmlClose)
    $matches = New-Object 'Collections.Generic.List[int]'
    for ($offset = 0; $offset -le $Bytes.Length - $closeBytes.Length; $offset++) {
        $same = $true
        for ($index = 0; $index -lt $closeBytes.Length; $index++) {
            if ($Bytes[$offset + $index] -ne $closeBytes[$index]) { $same = $false; break }
        }
        if ($same) { $matches.Add($offset) }
    }
    if ($matches.Count -ne 1) {
        throw 'input profile must contain exactly one complete C_SaveDataUnwrapped document.'
    }
    $xmlByteLength = $matches[0] + $closeBytes.Length
    $trailerByteLength = $Bytes.Length - $xmlByteLength
    if ($trailerByteLength -gt 16) { throw 'input profile has an unsupported binary trailer after the XML document.' }
    $xmlBytes = New-Object byte[] $xmlByteLength
    [Array]::Copy($Bytes, 0, $xmlBytes, 0, $xmlByteLength)
    $utf8 = New-Object Text.UTF8Encoding($false, $true)
    try { $xmlText = $utf8.GetString($xmlBytes) } catch { throw 'input profile XML prefix is not strict UTF-8.' }
    $readerSettings = [Xml.XmlReaderSettings]::new()
    $readerSettings.DtdProcessing = [Xml.DtdProcessing]::Prohibit
    $readerSettings.XmlResolver = $null
    $readerSettings.MaxCharactersInDocument = $script:MaxInputProfileBytes
    $readerSettings.MaxCharactersFromEntities = 0
    $stringReader = [IO.StringReader]::new($xmlText)
    $xmlReader = $null
    try {
        $xmlReader = [Xml.XmlReader]::Create($stringReader, $readerSettings)
        $xml = [Xml.XmlDocument]::new()
        $xml.XmlResolver = $null
        $xml.Load($xmlReader)
    } catch {
        throw "input profile XML is malformed or unsafe: $($_.Exception.Message)"
    } finally {
        if ($null -ne $xmlReader) { $xmlReader.Dispose() }
        $stringReader.Dispose()
    }
    if ($xml.DocumentElement.Name -cne 'C_SaveDataUnwrapped') { throw 'input profile root is not C_SaveDataUnwrapped.' }

    $blocks = @($xml.SelectNodes('/C_SaveDataUnwrapped/m_Blocks/C_GameInputSaveBlock'))
    if ($blocks.Count -ne 1) { throw 'input profile must contain exactly one C_GameInputSaveBlock.' }
    $sets = @($blocks[0].SelectNodes('m_ControlSet/S_ControlsSet'))
    if ($sets.Count -ne 4) { throw "input profile must contain four control sets; found $($sets.Count)." }
    $controllerType2Sets = @($sets | Where-Object {
        $node = $_.SelectSingleNode('m_ControllerDeviceType')
        $null -ne $node -and [string]$node.InnerText -ceq '2'
    })
    if ($controllerType2Sets.Count -ne 1) {
        throw "input profile must contain exactly one ControllerDeviceType=2 set; found $($controllerType2Sets.Count)."
    }
    $groups = @($controllerType2Sets[0].SelectNodes('m_ControlGroup/S_ControlGroup'))
    if ($groups.Count -ne 130) {
        throw "ControllerDeviceType=2 set must contain 130 logical controls; found $($groups.Count)."
    }

    $byLogical = @{}
    $canonicalRows = New-Object 'Collections.Generic.List[string]'
    $deviceType2Tuples = 0
    $deviceType1Tuples = 0
    for ($index = 0; $index -lt $groups.Count; $index++) {
        $group = $groups[$index]
        $logicalNode = $group.SelectSingleNode('m_Control')
        if ($null -eq $logicalNode -or [string]::IsNullOrWhiteSpace($logicalNode.InnerText)) {
            throw "ControllerDeviceType=2 control group $index has no logical control name."
        }
        $logical = [string]$logicalNode.InnerText
        if ($logical -cnotmatch '^[A-Z][A-Z0-9_]{0,63}$') { throw "logical control name is not canonical: $logical" }
        if ($byLogical.ContainsKey($logical)) { throw "duplicate logical control in ControllerDeviceType=2 set: $logical" }
        $primary = Get-ControlTuple $group 'm_Primary' "control[$logical]"
        $secondary = Get-ControlTuple $group 'm_Secondary' "control[$logical]"
        foreach ($tuple in @($primary, $secondary)) {
            if ($tuple.Device -notin @(1, 2)) { throw "control[$logical] uses unsupported device type $($tuple.Device)." }
            if ($tuple.Element -ne $script:UnsetElement) {
                if ($tuple.Device -eq 1) { $deviceType1Tuples++ } else { $deviceType2Tuples++ }
            }
        }
        $byLogical[$logical] = [pscustomobject]@{ Primary = $primary; Secondary = $secondary }
        $canonicalRows.Add(
            "$logical|$($primary.Mode)|$($primary.Element)|$($primary.Device)|$($secondary.Mode)|$($secondary.Element)|$($secondary.Device)")
    }

    foreach ($expected in $script:CoreBindings) {
        if (-not $byLogical.ContainsKey($expected.Logical)) { throw "missing core logical control $($expected.Logical)." }
        $actual = $byLogical[$expected.Logical].Primary
        if ($actual.Device -ne $expected.Device -or $actual.Element -ne $expected.Element -or $actual.Mode -ne $expected.Mode) {
            throw "core binding $($expected.Logical) drifted from device=$($expected.Device), element=$($expected.Element), mode=$($expected.Mode)."
        }
    }

    $settings = @($blocks[0].SelectNodes('S_ControlsSettings'))
    if ($settings.Count -ne 1) { throw 'input profile must contain exactly one S_ControlsSettings.' }
    $mouseSensitivity = [double]0
    if (-not [double]::TryParse(
        [string]$settings[0].m_MouseSensitivity,
        [Globalization.NumberStyles]::Float,
        [Globalization.CultureInfo]::InvariantCulture,
        [ref]$mouseSensitivity)) {
        throw 'm_MouseSensitivity is not a finite invariant float.'
    }
    if ([double]::IsNaN($mouseSensitivity) -or [double]::IsInfinity($mouseSensitivity)) {
        throw 'm_MouseSensitivity is not finite.'
    }
    foreach ($booleanName in @('m_InvertX', 'm_InvertY')) {
        if ([string]$settings[0].$booleanName -notin @('true', 'false')) {
            throw "$booleanName is not canonical boolean text."
        }
    }

    $canonicalBytes = (New-Object Text.UTF8Encoding($false)).GetBytes(($canonicalRows -join "`n"))
    return [pscustomobject][ordered]@{
        xml_bytes = $xmlByteLength
        trailer_bytes = $trailerByteLength
        control_set_count = $sets.Count
        controller_type_2_logical_controls = $groups.Count
        device_type_2_bound_tuples = $deviceType2Tuples
        device_type_1_bound_tuples = $deviceType1Tuples
        canonical_control_set_sha256 = Get-Sha256 $canonicalBytes
        mouse_sensitivity = $mouseSensitivity
        invert_x = ([string]$settings[0].m_InvertX -ceq 'true')
        invert_y = ([string]$settings[0].m_InvertY -ceq 'true')
    }
}

function Read-VideoConfigBytes {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)
    if ($Bytes.LongLength -gt $script:MaxVideoConfigBytes) {
        throw "videoconfig.cfg exceeds the $($script:MaxVideoConfigBytes)-byte validation cap."
    }
    $ascii = New-Object Text.ASCIIEncoding
    $text = $ascii.GetString($Bytes)
    if ($text -cnotmatch '^\d+( \d+){8}$') {
        throw 'videoconfig.cfg must be exactly nine space-separated unsigned decimal fields.'
    }
    $tokens = @($text.Split(' '))
    $values = @()
    for ($index = 0; $index -lt $tokens.Count; $index++) {
        $values += ConvertTo-UInt32Exact $tokens[$index] "videoconfig[$index]"
    }
    $expected = @(0, 0, 1920, 1080, 0, 0, 1, 0, 0)
    $names = @(
        'Position_x', 'Position_y', 'Resolution_x', 'Resolution_y', 'IsFullscreen',
        'AutodetectResolution', 'IsVSyncEnabled', 'GPUIndex', 'DisplayIndex'
    )
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($values[$index] -ne $expected[$index]) {
            throw "videoconfig field $($names[$index]) drifted (expected $($expected[$index]), got $($values[$index]))."
        }
    }
    return [pscustomobject][ordered]@{
        position_x = $values[0]
        position_y = $values[1]
        resolution_x = $values[2]
        resolution_y = $values[3]
        is_fullscreen = [bool]$values[4]
        autodetect_resolution = [bool]$values[5]
        vsync_enabled = [bool]$values[6]
        gpu_index = $values[7]
        gpu_name = 'Default'
        display_index = $values[8]
        display_name = 'Default'
        hdr_field_present = $false
        hdr_confirmed = $false
    }
}

function Assert-SupportedLauncherHash {
    param(
        [Parameter(Mandatory = $true)][string]$ActualSha256,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )
    if ($ExpectedSha256 -cne $script:SupportedLauncherSha256) {
        throw 'Expected launcher hash is not the build whose official LoadVideoConfigFile field mapping was verified.'
    }
    if ($ActualSha256 -cne $ExpectedSha256) { throw 'Launcher.exe SHA-256 mismatch.' }
}

function New-ControlXml {
    param(
        [Parameter(Mandatory = $true)][string]$Logical,
        [Parameter(Mandatory = $true)][uint32]$Device,
        [Parameter(Mandatory = $true)][uint32]$Element,
        [Parameter(Mandatory = $true)][uint32]$Mode
    )
    return "<S_ControlGroup><m_Primary><S_Control><m_ControlMode>$Mode</m_ControlMode><m_ControlElement>$Element</m_ControlElement><m_DeviceType>$Device</m_DeviceType><m_LongHoldTime>2.50000000e-01</m_LongHoldTime><m_LogicalControl></m_LogicalControl></S_Control></m_Primary><m_Secondary><S_Control><m_ControlMode>32</m_ControlMode><m_ControlElement>4294967295</m_ControlElement><m_DeviceType>2</m_DeviceType><m_LongHoldTime>2.50000000e-01</m_LongHoldTime><m_LogicalControl></m_LogicalControl></S_Control></m_Secondary><m_Control>$Logical</m_Control></S_ControlGroup>"
}

function New-InputFixture {
    $groups = New-Object 'Collections.Generic.List[string]'
    foreach ($binding in $script:CoreBindings) {
        $groups.Add((New-ControlXml $binding.Logical $binding.Device $binding.Element $binding.Mode))
    }
    for ($index = $groups.Count; $index -lt 130; $index++) {
        $groups.Add((New-ControlXml ("FIXTURE_{0:D3}" -f $index) 2 $script:UnsetElement 32))
    }
    $gamepad = '<S_ControlsSet><m_ControlGroup></m_ControlGroup><m_ControllerDeviceType>4</m_ControllerDeviceType></S_ControlsSet>'
    return "<?xml version=`"1.0`" encoding=`"utf-8`"?><C_SaveDataUnwrapped><m_Blocks><C_GameInputSaveBlock><m_ControlSet>$gamepad$gamepad$gamepad<S_ControlsSet><m_ControlGroup>$($groups -join '')</m_ControlGroup><m_ControllerDeviceType>2</m_ControllerDeviceType></S_ControlsSet></m_ControlSet><S_ControlsSettings><m_MouseSensitivity>5.00000000e-01</m_MouseSensitivity><m_InvertY>false</m_InvertY><m_InvertX>false</m_InvertX></S_ControlsSettings></C_GameInputSaveBlock></m_Blocks></C_SaveDataUnwrapped>?9"
}

function ConvertTo-Utf8Bytes {
    param([Parameter(Mandatory = $true)][string]$Text)
    return (New-Object Text.UTF8Encoding($false)).GetBytes($Text)
}

function Assert-Throws {
    param([Parameter(Mandatory = $true)][scriptblock]$Action, [Parameter(Mandatory = $true)][string]$Label)
    $blocked = $false
    try { & $Action | Out-Null } catch { $blocked = $true }
    if (-not $blocked) { throw "self-test negative case passed unexpectedly: $Label" }
}

function Invoke-SelfTest {
    $tests = 0
    $fixture = New-InputFixture
    $parsed = Read-InputProfileBytes (ConvertTo-Utf8Bytes $fixture)
    if ($parsed.controller_type_2_logical_controls -ne 130 -or $parsed.control_set_count -ne 4) {
        throw 'self-test valid input profile was not parsed correctly.'
    }
    $tests++

    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            '<m_ControllerDeviceType>4</m_ControllerDeviceType>',
            '<m_ControllerDeviceType>2</m_ControllerDeviceType>')))
    } 'duplicate ControllerDeviceType=2 set'
    $tests++

    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            (New-ControlXml 'FIXTURE_129' 2 $script:UnsetElement 32),
            '')))
    } 'logical control count drift'
    $tests++

    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            '<m_Control>UP</m_Control>',
            '<m_Control>UP_DRIFTED</m_Control>')))
    } 'core control name drift'
    $tests++

    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            '<m_ControlMode>32</m_ControlMode><m_ControlElement>17</m_ControlElement><m_DeviceType>2</m_DeviceType>',
            '<m_ControlMode>32</m_ControlMode><m_ControlElement>18</m_ControlElement><m_DeviceType>2</m_DeviceType>')))
    } 'core profile tuple drift'
    $tests++
    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            '<m_ControlMode>32</m_ControlMode><m_ControlElement>17</m_ControlElement><m_DeviceType>2</m_DeviceType>',
            '<m_ControlMode>32</m_ControlMode><m_ControlElement>17</m_ControlElement><m_DeviceType>3</m_DeviceType>')))
    } 'unsupported input device type'
    $tests++
    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture + '01234567890123456'))
    } 'oversized binary trailer'
    $tests++
    Assert-Throws {
        Read-InputProfileBytes (ConvertTo-Utf8Bytes ($fixture.Replace(
            '<C_SaveDataUnwrapped>',
            '<!DOCTYPE C_SaveDataUnwrapped [<!ENTITY xxe SYSTEM "file:///not-readable">]><C_SaveDataUnwrapped>')))
    } 'DTD is prohibited'
    $tests++
    Assert-Throws { Read-InputProfileBytes (New-Object byte[] ($script:MaxInputProfileBytes + 1)) } 'oversized input profile'
    $tests++

    $video = ConvertTo-Utf8Bytes '0 0 1920 1080 0 0 1 0 0'
    $videoResult = Read-VideoConfigBytes $video
    if ($videoResult.is_fullscreen -or $videoResult.hdr_confirmed -or $videoResult.resolution_x -ne 1920) {
        throw 'self-test valid video config was not parsed correctly.'
    }
    $tests++
    Assert-Throws { Read-VideoConfigBytes (ConvertTo-Utf8Bytes '0 0 1920 1080 1 0 1 0 0') } 'fullscreen drift'
    $tests++
    Assert-Throws { Read-VideoConfigBytes (ConvertTo-Utf8Bytes '0 0 2560 1440 0 0 1 0 0') } 'resolution drift'
    $tests++
    Assert-Throws { Read-VideoConfigBytes (ConvertTo-Utf8Bytes '0 0 1920 1080 0 0 0 0 0') } 'VSync drift'
    $tests++
    Assert-Throws { Read-VideoConfigBytes (ConvertTo-Utf8Bytes '00 0 1920 1080 0 0 1 0 0') } 'non-canonical video integer'
    $tests++
    Assert-Throws { Read-VideoConfigBytes (ConvertTo-Utf8Bytes '0 0 1920 1080 0 0 1 0') } 'video field count drift'
    $tests++
    Assert-Throws { Read-VideoConfigBytes (New-Object byte[] ($script:MaxVideoConfigBytes + 1)) } 'oversized video config'
    $tests++
    $boundedReadFixture = [IO.Path]::GetTempFileName()
    try {
        [IO.File]::WriteAllBytes($boundedReadFixture, (New-Object byte[] 2))
        Assert-Throws { Read-BoundedFileBytes $boundedReadFixture 1 'bounded-read-fixture' } 'bounded second read'
    } finally {
        Remove-Item -LiteralPath $boundedReadFixture -Force -ErrorAction SilentlyContinue
    }
    $tests++
    Assert-Throws {
        Assert-SupportedLauncherHash $script:SupportedLauncherSha256 ('sha256:' + ('b' * 64))
    } 'unverified launcher layout'
    $tests++
    [pscustomobject][ordered]@{ status = 'pass'; tests = $tests } | ConvertTo-Json
}

if ($SelfTest) {
    Invoke-SelfTest
    exit 0
}

$inputProfile = Read-StableFile $InputProfilePath $ExpectedInputProfileSha256 'gameinputmodule.pf' $script:MaxInputProfileBytes -CaptureBytes
$videoConfig = Read-StableFile $VideoConfigPath $ExpectedVideoConfigSha256 'videoconfig.cfg' $script:MaxVideoConfigBytes -CaptureBytes
$launcher = Read-StableFile $LauncherPath $ExpectedLauncherSha256 'Launcher.exe' $script:MaxLauncherBytes
$validatorSha256 = 'sha256:' + (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
if ([IO.Path]::GetFileName((Resolve-Path -LiteralPath $LauncherPath).Path) -cne 'Launcher.exe') {
    throw 'verified launcher must have basename Launcher.exe.'
}
Assert-SupportedLauncherHash $launcher.Sha256 $ExpectedLauncherSha256
$inputEvidence = Read-InputProfileBytes $inputProfile.Bytes
$videoEvidence = Read-VideoConfigBytes $videoConfig.Bytes

[pscustomobject][ordered]@{
    schema = $script:Schema
    status = 'pass'
    validator_sha256 = $validatorSha256
    proof_scope = 'local_profile_bytes_only'
    expected_app_id_context = $script:AppId
    expected_steam_build_id_context = $script:BuildId
    profile_embeds_build_identity = $false
    external_install_gate_required = $true
    executable_basename = 'Mafia3DefinitiveEdition.exe'
    launcher = [pscustomobject][ordered]@{
        basename = 'Launcher.exe'
        sha256 = $launcher.Sha256
        bytes = $launcher.Length
        field_mapping_evidence = 'official W3Launcher.VideoConfigFile.LoadVideoConfigFile read-only reflection'
    }
    input_profile = [pscustomobject][ordered]@{
        basename = 'gameinputmodule.pf'
        sha256 = $inputProfile.Sha256
        bytes = $inputProfile.Length
        structure = $inputEvidence
        core_bindings = @($script:CoreBindings | ForEach-Object {
            [pscustomobject][ordered]@{
                logical_control = $_.Logical
                device_kind = if ($_.Device -eq 1) { 'profile_device_type_1' } else { 'profile_device_type_2' }
                element = $_.Element
                control_mode = $_.Mode
                slot = 'primary'
            }
        })
        source_logical_names_preserved = $true
        default_preset_confirmed = $false
    }
    video_config = [pscustomobject][ordered]@{
        basename = 'videoconfig.cfg'
        sha256 = $videoConfig.Sha256
        bytes = $videoConfig.Length
        fields = $videoEvidence
    }
    catalog_authored = $false
    rgb_recording_ready = $false
    depth_capture_available = $false
    blockers = @(
        'representative_single_player_runtime_evidence_missing',
        'input_profile_default_preset_confirmation_missing',
        'physical_input_mapping_and_semantic_authoring_missing',
        'hdr_or_sdr_confirmation_missing',
        'd7_rgb_runtime_evidence_missing',
        'depth_profile_missing',
        'camera_binding_missing'
    )
} | ConvertTo-Json -Depth 8
