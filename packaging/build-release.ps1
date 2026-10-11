[CmdletBinding()]
param(
    [string]$TargetDir = $(if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }),
    [string]$OutputDir = "dist",
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
$previousTargetDir = $env:CARGO_TARGET_DIR
try {
    if ([IO.Path]::IsPathRooted($TargetDir)) {
        $resolvedTargetDir = $TargetDir
    } else {
        $resolvedTargetDir = Join-Path $repoRoot $TargetDir
    }
    if ([IO.Path]::IsPathRooted($OutputDir)) {
        $resolvedOutputDir = $OutputDir
    } else {
        $resolvedOutputDir = Join-Path $repoRoot $OutputDir
    }
    $env:CARGO_TARGET_DIR = $resolvedTargetDir

    Write-Host "Building ZXraw Explorer (release)..."
    & cargo build --release --no-default-features
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed with exit code $LASTEXITCODE" }

    if ($SkipTests) {
        Write-Warning "Skipping workspace tests (-SkipTests); do not publish this build without a passing test run."
    } else {
        Write-Host "Running workspace tests..."
        & cargo test --workspace
        if ($LASTEXITCODE -ne 0) { throw "cargo test --workspace failed with exit code $LASTEXITCODE" }
    }

    Write-Host "Generating Windows-target third-party inventory..."
    & (Join-Path $PSScriptRoot "generate-third-party.ps1")

    $manifest = Get-Content -Raw (Join-Path $repoRoot "Cargo.toml")
    $versionMatch = [regex]::Match($manifest, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $versionMatch.Success) { throw "Could not read workspace.package.version from Cargo.toml" }
    $version = $versionMatch.Groups[1].Value

    $stageName = "ZXraw-Explorer-$version-x64"
    $stagePath = Join-Path $resolvedOutputDir $stageName
    $zipPath = Join-Path $resolvedOutputDir "$stageName.zip"
    New-Item -ItemType Directory -Force -Path $resolvedOutputDir | Out-Null
    if (Test-Path -LiteralPath $stagePath) {
        $resolvedStagePath = (Resolve-Path -LiteralPath $stagePath).Path
        $resolvedOutputPrefix = (Resolve-Path -LiteralPath $resolvedOutputDir).Path.TrimEnd('\') + '\'
        if (-not $resolvedStagePath.StartsWith($resolvedOutputPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to clear staging path outside the output directory: $resolvedStagePath"
        }
        Remove-Item -LiteralPath $resolvedStagePath -Recurse -Force
    }
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    New-Item -ItemType Directory -Force -Path $stagePath | Out-Null

    $releaseDir = Join-Path $resolvedTargetDir "release"
    foreach ($binary in @("zxraw.exe", "zxraw-cli.exe")) {
        $source = Join-Path $releaseDir $binary
        if (-not (Test-Path -LiteralPath $source)) { throw "Required release binary missing: $source" }
        Copy-Item -LiteralPath $source -Destination $stagePath
    }
    foreach ($file in @(
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "packaging/THIRD_PARTY.md",
        "packaging/LICENSES-NOTE.md"
    )) {
        $source = Join-Path $repoRoot $file
        if (-not (Test-Path -LiteralPath $source)) { throw "Required release file missing: $source" }
        Copy-Item -LiteralPath $source -Destination $stagePath
    }
    Copy-Item -LiteralPath (Join-Path $repoRoot "docs") -Destination $stagePath -Recurse

    Compress-Archive -Path (Join-Path $stagePath "*") -DestinationPath $zipPath -CompressionLevel Optimal

    Write-Host "Release folder: $stagePath"
    Write-Host "Release archive: $zipPath"
    Get-ChildItem -LiteralPath $stagePath -File -Recurse | Get-FileHash -Algorithm SHA256 |
        Select-Object @{Name="Artifact"; Expression={$_.Path}}, Hash | Format-Table -AutoSize
    Get-FileHash -LiteralPath $zipPath -Algorithm SHA256 |
        Select-Object @{Name="Artifact"; Expression={$_.Path}}, Hash | Format-Table -AutoSize
} finally {
    $env:CARGO_TARGET_DIR = $previousTargetDir
    Pop-Location
}
