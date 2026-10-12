<#
.SYNOPSIS
Builds a ZXraw Explorer release: bumps the version, builds the portable zip and the
installer, writes SHA256SUMS.txt, and prints the `gh release create` command.

.DESCRIPTION
1. Sets `[workspace.package] version` in Cargo.toml and `#define AppVersion` in
   packaging/zxraw.iss to -Version, and refreshes Cargo.lock's workspace entries.
2. Runs packaging/build-release.ps1 (release build, tests unless -SkipTests, zip).
3. Compiles packaging/zxraw.iss with Inno Setup's ISCC.exe.
4. Writes dist\SHA256SUMS.txt (`<sha256>  <file name>` per line) for the installer and
   the zip. The in-app updater refuses an installer that is not listed there.
5. Prints the `gh release create` command. Nothing is published, committed or tagged.

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\release.ps1 -Version 0.2.0
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [switch]$SkipTests,
    [switch]$Draft,
    [string]$TargetDir = $(if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" })
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$repo = "zuhuHix/zxraw-explorer"

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Version must look like 1.2.3 (no 'v' prefix, no pre-release suffix): $Version"
}

function Set-FileText([string]$Path, [string]$Text) {
    # UTF-8 without BOM, keeping the file's existing line endings.
    [IO.File]::WriteAllText($Path, $Text, (New-Object Text.UTF8Encoding($false)))
}

function Find-Iscc {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) { return $candidate }
    }
    $onPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    throw "Inno Setup 6 (ISCC.exe) was not found. Install it from https://jrsoftware.org/isdl.php"
}

Push-Location $repoRoot
try {
    # 1. Version bump.
    $cargoToml = Join-Path $repoRoot "Cargo.toml"
    $manifest = [IO.File]::ReadAllText($cargoToml)
    $pattern = '(?ms)(^\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]+(")'
    if (-not [regex]::IsMatch($manifest, $pattern)) {
        throw "Could not find [workspace.package] version in Cargo.toml"
    }
    Set-FileText $cargoToml ([regex]::Replace($manifest, $pattern, "`${1}$Version`${2}", 1))

    $iss = Join-Path $PSScriptRoot "zxraw.iss"
    $script = [IO.File]::ReadAllText($iss)
    $issPattern = '(?m)^(#define AppVersion ")[^"]+(")'
    if (-not [regex]::IsMatch($script, $issPattern)) {
        throw "Could not find #define AppVersion in packaging\zxraw.iss"
    }
    Set-FileText $iss ([regex]::Replace($script, $issPattern, "`${1}$Version`${2}", 1))

    Write-Host "Updating Cargo.lock for version $Version..."
    & cargo update --workspace --offline
    if ($LASTEXITCODE -ne 0) {
        & cargo update --workspace
        if ($LASTEXITCODE -ne 0) { throw "cargo update --workspace failed with exit code $LASTEXITCODE" }
    }

    # 2. Release build, tests and the portable zip.
    $buildArgs = @{ TargetDir = $TargetDir }
    if ($SkipTests) { $buildArgs.SkipTests = $true }
    & (Join-Path $PSScriptRoot "build-release.ps1") @buildArgs

    # 3. Installer.
    $iscc = Find-Iscc
    Write-Host "Compiling the installer with $iscc..."
    & $iscc "/Q" $iss
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }

    # 4. Checksums.
    $dist = Join-Path $repoRoot "dist"
    $setupName = "ZXraw-Explorer-$Version-Setup-x64.exe"
    $zipName = "ZXraw-Explorer-$Version-x64.zip"
    $lines = foreach ($name in @($setupName, $zipName)) {
        $path = Join-Path $dist $name
        if (-not (Test-Path -LiteralPath $path)) { throw "Release artifact missing: $path" }
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $name"
    }
    $sumsPath = Join-Path $dist "SHA256SUMS.txt"
    [IO.File]::WriteAllText($sumsPath, (($lines -join "`n") + "`n"), (New-Object Text.ASCIIEncoding))

    Write-Host ""
    Write-Host "Release files in ${dist}:"
    foreach ($name in @($setupName, $zipName, "SHA256SUMS.txt")) {
        $item = Get-Item -LiteralPath (Join-Path $dist $name)
        Write-Host ("  {0,-45} {1,12:N0} bytes" -f $item.Name, $item.Length)
    }
    Write-Host ""
    Write-Host "SHA256SUMS.txt:"
    Get-Content -LiteralPath $sumsPath | ForEach-Object { Write-Host "  $_" }

    # 5. Publishing is done by hand.
    $draftFlag = if ($Draft) { " --draft" } else { "" }
    Write-Host ""
    Write-Host "Next: commit the version bump, tag it and publish (from $repoRoot):"
    Write-Host "  git commit -am `"Release $Version`""
    Write-Host "  git tag v$Version"
    Write-Host "  git push --follow-tags"
    Write-Host ("  gh release create v$Version --repo $repo --title `"ZXraw Explorer $Version`" --generate-notes$draftFlag " +
        "dist\$setupName dist\$zipName dist\SHA256SUMS.txt")
    Write-Host "The in-app update check sees only published, non-draft, non-prerelease releases."
} finally {
    Pop-Location
}
