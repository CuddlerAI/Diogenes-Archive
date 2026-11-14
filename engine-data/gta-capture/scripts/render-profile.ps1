Set-StrictMode -Version Latest

$script:DiogenesRePlusRenderProfileSchema = "dm-gta-replus-render-profile/1"
$script:DiogenesRePlusRenderProfileAlgorithm = "replace-unique-keys-utf8-crlf/1"

function Get-DiogenesRePlusRenderProfile([string]$RenderOutputFolder) {
    if ($RenderOutputFolder -match "[\r\n]") {
        throw "RenderOutputFolder may not contain a line break"
    }
    return [ordered]@{
        EnableRenderer = "1"
        RenderCaptureMode = "Walking"
        RenderDepthOfField = "0"
        RenderFps = "30"
        RenderSamples = "1"
        RenderHighlightBoost = "0.9"
        RenderSettleFrames = "3"
        RenderSettleSubFrames = "1"
        RenderJpeg = "0"
        RenderHideHud = "1"
        ExportCloseWhenDone = "1"
        RenderOutputFolder = $RenderOutputFolder
        RenderMode = "Frames"
        RenderKeepFrames = "1"
        RenderAudio = "1"
        RenderMarkerSpeed = "1"
    }
}

function Read-DiogenesStrictUtf8([string]$Path) {
    $utf8Strict = [Text.UTF8Encoding]::new($false, $true)
    try {
        $raw = $utf8Strict.GetString([IO.File]::ReadAllBytes($Path))
    } catch {
        throw "RE+ ini is not valid UTF-8: $Path"
    }
    if ($raw.Length -gt 0 -and $raw[0] -eq [char]0xFEFF) {
        $raw = $raw.Substring(1)
    }
    return $raw
}

function Get-DiogenesSha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-DiogenesIniLines([string]$Raw) {
    # Keep the transformation independent from PowerShell's permissive regex
    # handling of a lone CR. Only LF and CRLF are accepted so the receipt cannot
    # bless text that another INI reader may parse differently.
    if ($Raw.Contains("`r") -and $Raw -match "(?<!`r)`n|`r(?!`n)") {
        throw "RE+ ini has unsupported line endings"
    }
    $lines = [Collections.Generic.List[string]]($Raw -split "\r?\n")
    while ($lines.Count -gt 0 -and $lines[$lines.Count - 1] -eq "") {
        $lines.RemoveAt($lines.Count - 1)
    }
    return $lines
}

function Assert-DiogenesIniProfile([Collections.Generic.List[string]]$Lines,
                                   $Settings) {
    foreach ($property in $Settings.GetEnumerator()) {
        $pattern = '^\s*' + [Regex]::Escape([string]$property.Key) + '\s*='
        $matches = @($Lines | Where-Object { $_ -match $pattern })
        if ($matches.Count -ne 1) {
            throw "RE+ setting must occur exactly once: $($property.Key)"
        }
        $actual = ($matches[0] -split '=', 2)[1].Trim()
        if ($actual -ne [string]$property.Value) {
            throw ("RE+ setting differs from the sealed capture profile: " +
                "$($property.Key) expected '$($property.Value)', found '$actual'")
        }
    }
}

function Set-DiogenesRePlusRenderProfile([string]$Path,
                                         [string]$RenderOutputFolder) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "RE+ Render.ini not found: $Path"
    }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $settings = Get-DiogenesRePlusRenderProfile $RenderOutputFolder
    $inputBytes = (Get-Item -LiteralPath $resolved).Length
    $inputSha256 = Get-DiogenesSha256 $resolved
    $lines = Get-DiogenesIniLines (Read-DiogenesStrictUtf8 $resolved)

    foreach ($property in $settings.GetEnumerator()) {
        $pattern = '^\s*' + [Regex]::Escape([string]$property.Key) + '\s*='
        $indexes = @()
        for ($index = 0; $index -lt $lines.Count; $index++) {
            if ($lines[$index] -match $pattern) { $indexes += $index }
        }
        if ($indexes.Count -ne 1) {
            throw "RE+ setting must occur exactly once before sealing: $($property.Key)"
        }
        $lines[$indexes[0]] = "$($property.Key)=$($property.Value)"
    }

    # Canonical bytes make the transform reproducible across PowerShell versions:
    # strict UTF-8 without BOM, CRLF line endings, and exactly one final newline.
    $canonical = ([string]::Join("`r`n", $lines)) + "`r`n"
    $staging = "$resolved.diogenes-configuring"
    if (Test-Path -LiteralPath $staging) {
        Remove-Item -LiteralPath $staging -Force
    }
    try {
        [IO.File]::WriteAllText($staging, $canonical, [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $staging -Destination $resolved -Force
    } finally {
        if (Test-Path -LiteralPath $staging -PathType Leaf) {
            Remove-Item -LiteralPath $staging -Force
        }
    }

    $outputBytes = (Get-Item -LiteralPath $resolved).Length
    $outputSha256 = Get-DiogenesSha256 $resolved
    Assert-DiogenesIniProfile (Get-DiogenesIniLines (Read-DiogenesStrictUtf8 $resolved)) `
        $settings
    return [ordered]@{
        schema = $script:DiogenesRePlusRenderProfileSchema
        algorithm = $script:DiogenesRePlusRenderProfileAlgorithm
        input = [ordered]@{ bytes = $inputBytes; sha256 = $inputSha256 }
        settings = $settings
        output = [ordered]@{ bytes = $outputBytes; sha256 = $outputSha256 }
    }
}

function Assert-DiogenesRePlusRenderProfile([string]$Path, $Record,
                                            $ArtifactRecord,
                                            [string]$ExpectedOutputFolder) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "RE+ Render.ini not found: $Path"
    }
    if (-not $Record -or
        [string]$Record.schema -ne $script:DiogenesRePlusRenderProfileSchema -or
        [string]$Record.algorithm -ne $script:DiogenesRePlusRenderProfileAlgorithm) {
        throw "capture build receipt has no approved Render.ini transform"
    }
    $expected = Get-DiogenesRePlusRenderProfile $ExpectedOutputFolder
    $recordProperties = @($Record.settings.PSObject.Properties)
    if ($recordProperties.Count -ne $expected.Count) {
        throw "capture build receipt Render.ini settings have unexpected keys"
    }
    foreach ($property in $expected.GetEnumerator()) {
        $actualProperty = $Record.settings.PSObject.Properties[[string]$property.Key]
        if (-not $actualProperty -or [string]$actualProperty.Value -ne [string]$property.Value) {
            throw "capture build receipt Render.ini setting mismatch: $($property.Key)"
        }
    }
    foreach ($side in @($Record.input, $Record.output)) {
        if (-not $side -or -not ([string]$side.sha256 -match '^[0-9a-f]{64}$') -or
            -not ($side.bytes -is [ValueType]) -or [int64]$side.bytes -le 0) {
            throw "capture build receipt Render.ini transform hash/length is invalid"
        }
    }
    $actualBytes = (Get-Item -LiteralPath $Path).Length
    $actualSha256 = Get-DiogenesSha256 $Path
    if ([int64]$Record.output.bytes -ne $actualBytes -or
        [string]$Record.output.sha256 -ne $actualSha256 -or
        [int64]$ArtifactRecord.bytes -ne $actualBytes -or
        [string]$ArtifactRecord.sha256 -ne $actualSha256) {
        throw "sealed Render.ini output differs from the receipt artifact"
    }
    $settings = Get-DiogenesRePlusRenderProfile $ExpectedOutputFolder
    Assert-DiogenesIniProfile (Get-DiogenesIniLines (Read-DiogenesStrictUtf8 $Path)) `
        $settings
}
