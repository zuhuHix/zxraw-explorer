[CmdletBinding()]
param(
    [string]$OutputPath = (Join-Path $PSScriptRoot "THIRD_PARTY.md")
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
try {
    $metadataText = & cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $metadata = ($metadataText -join [Environment]::NewLine) | ConvertFrom-Json

    $packageById = @{}
    foreach ($package in $metadata.packages) {
        $packageById[[string]$package.id] = $package
    }
    $nodeById = @{}
    foreach ($node in $metadata.resolve.nodes) {
        $nodeById[[string]$node.id] = $node
    }

    $roots = @($metadata.packages | Where-Object {
        $_.name -eq "forge-app" -or $_.name -eq "forge-cli"
    })
    if (@($roots | Where-Object { $_.name -eq "forge-app" }).Count -eq 0 -or
        @($roots | Where-Object { $_.name -eq "forge-cli" }).Count -eq 0) {
        throw "cargo metadata did not include both forge-app and forge-cli"
    }

    $included = [System.Collections.Generic.HashSet[string]]::new()
    $pending = [System.Collections.Generic.Queue[string]]::new()
    foreach ($root in $roots) {
        $pending.Enqueue([string]$root.id)
    }
    while ($pending.Count -gt 0) {
        $packageId = $pending.Dequeue()
        if (-not $included.Add($packageId)) {
            continue
        }
        $node = $nodeById[$packageId]
        if ($null -eq $node) {
            continue
        }
        foreach ($dependency in $node.deps) {
            $normalKinds = @($dependency.dep_kinds | Where-Object { $null -eq $_.kind })
            if ($normalKinds.Count -gt 0) {
                $pending.Enqueue([string]$dependency.pkg)
            }
        }
    }

    $permissiveLicenses = @{}
    foreach ($license in @(
        "0BSD",
        "Apache-2.0",
        "BSD-0-Clause",
        "BSD-2-Clause",
        "BSD-3-Clause",
        "BSD-4-Clause",
        "BSL-1.0",
        "CC0-1.0",
        "ISC",
        "MIT",
        "MIT-0",
        "Unicode-3.0",
        "Unicode-DFS-2016",
        "Unlicense",
        "Zlib"
    )) {
        $permissiveLicenses[$license] = $true
    }

    $rows = foreach ($packageId in $included) {
        $package = $packageById[$packageId]
        $expression = [string]$package.license
        $baseExpression = [regex]::Replace(
            $expression,
            '\s+WITH\s+[A-Za-z0-9.+-]+',
            ' '
        )
        $licenseTokens = @([regex]::Matches(
            $baseExpression,
            '[A-Za-z0-9][A-Za-z0-9.+-]*'
        ) | ForEach-Object { $_.Value } | Where-Object {
            $_ -ne "AND" -and $_ -ne "OR" -and $_ -ne "WITH"
        })
        $nonPermissive = @($licenseTokens | Where-Object {
            -not $permissiveLicenses.ContainsKey($_)
        })
        $review = if ($licenseTokens.Count -gt 0 -and $nonPermissive.Count -eq 0) {
            "Permissive"
        } elseif ($expression -match "MPL-2\.0") {
            "FLAG: MPL-2.0 weak copyleft review"
        } else {
            "FLAG: license review"
        }
        [pscustomobject]@{
            Name = [string]$package.name
            Version = [string]$package.version
            License = if ([string]::IsNullOrWhiteSpace($expression)) { "Not declared" } else { $expression }
            Repository = if ([string]::IsNullOrWhiteSpace([string]$package.repository)) {
                "Not declared"
            } else {
                [string]$package.repository
            }
            Review = $review
        }
    }

    function Escape-Markdown([string]$Value) {
        return (($Value -replace '\|', '\|') -replace '\r?\n', ' ')
    }

    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add("# Third-party inventory")
    $lines.Add("")
    $lines.Add("Generated from cargo metadata for x86_64-pc-windows-msvc.")
    $lines.Add("Scope: forge-app and forge-cli, plus their resolved normal runtime dependencies; dev and build dependencies are excluded.")
    $lines.Add("")
    $lines.Add("| Crate | Version | License expression | Repository | Review |")
    $lines.Add("| --- | --- | --- | --- | --- |")
    foreach ($row in ($rows | Sort-Object Name, Version)) {
        $lines.Add("| $(Escape-Markdown $row.Name) | $(Escape-Markdown $row.Version) | $(Escape-Markdown $row.License) | $(Escape-Markdown $row.Repository) | $(Escape-Markdown $row.Review) |")
    }
    $flagged = @($rows | Where-Object { $_.Review -like "FLAG:*" } | Sort-Object Name, Version)
    $lines.Add("")
    $lines.Add("## License notes")
    $lines.Add("")
    $lines.Add("The report flags missing or non-permissive license expressions for release review. The allowlist recognizes MIT, MIT-0, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0-1.0, BSL-1.0, and Unlicense expressions. SPDX WITH exceptions are reported with their base license.")
    $lines.Add("MPL-2.0 is weak copyleft with file-level source and notice obligations; any dependency declaring it is flagged for review.")
    if ($flagged.Count -eq 0) {
        $lines.Add("No dependencies were flagged by the license allowlist.")
    } else {
        $lines.Add("Flagged dependencies: " + (($flagged | ForEach-Object { "$($_.Name) $($_.Version) ($($_.Review); $($_.License))" }) -join "; "))
    }
    $lines.Add("FFmpeg and ffprobe are optional external programs and are not bundled with ZXraw Explorer.")

    if ([IO.Path]::IsPathRooted($OutputPath)) {
        $outputFile = $OutputPath
    } else {
        $outputFile = Join-Path $repoRoot $OutputPath
    }
    $outputDirectory = Split-Path -Parent $outputFile
    New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
    $content = ($lines -join [Environment]::NewLine) + [Environment]::NewLine
    [IO.File]::WriteAllText($outputFile, $content, [Text.UTF8Encoding]::new($false))
    Write-Host "Wrote $($rows.Count) crate entries to $outputFile"
    Write-Host "License flags: $($flagged.Count)"
} finally {
    Pop-Location
}
