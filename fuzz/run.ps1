<#
.SYNOPSIS
  Build and run the archive fuzz targets (libFuzzer + ASan, nightly) a few at a time.

.DESCRIPTION
  Needs `rustup toolchain install nightly` and `cargo install cargo-fuzz`, plus the
  Visual Studio C++ tools (for the ASan runtime DLL). Each target runs for -Seconds in
  total; after a crash it is restarted until its time is used up, so one run collects
  every distinct crash. Crashes land in fuzz/artifacts/<target>/, logs in
  fuzz/logs/<target>.log, the growing corpus in fuzz/corpus/<target>/ (seeded from
  fuzz/seeds/<target>/).

.EXAMPLE
  fuzz\run.ps1 -Seconds 900 -Jobs 4
  fuzz\run.ps1 -Targets zip,sevenz -Seconds 60
#>
param(
    [string[]]$Targets = @('detect', 'zip', 'sevenz', 'tar', 'stream', 'rar', 'zip_peek',
        'sevenz_guard', 'tar_walk', 'codec_walk'),
    [int]$Seconds = 900,
    [int]$Jobs = 4,
    [int]$RssMb = 2048,
    [int]$TimeoutSec = 10,
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$Targets = @($Targets | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$fuzz = $PSScriptRoot

# sevenz-rust2 also builds a cdylib, which can't link with coverage instrumentation:
# a copy built as an rlib only is patched in (Cargo.toml [patch.crates-io]).
$vendor = Join-Path $fuzz 'vendor\sevenz-rust2'
if (-not (Test-Path $vendor)) {
    $lock = Get-Content (Join-Path $fuzz '..\Cargo.lock') -Raw
    if ($lock -notmatch 'name = "sevenz-rust2"\s+version = "([^"]+)"') { throw 'sevenz-rust2 not in Cargo.lock' }
    $ver = $Matches[1]
    $src = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src\*\sevenz-rust2-$ver" -Directory | Select-Object -First 1
    if (-not $src) { Push-Location (Join-Path $fuzz '..'); cargo fetch; Pop-Location
        $src = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src\*\sevenz-rust2-$ver" -Directory | Select-Object -First 1 }
    New-Item -ItemType Directory -Force (Split-Path $vendor) | Out-Null
    Copy-Item $src.FullName $vendor -Recurse
    $toml = Join-Path $vendor 'Cargo.toml'
    (Get-Content $toml -Raw) -replace 'crate-type = \[\s*"cdylib",\s*"rlib",?\s*\]', 'crate-type = ["rlib"]' |
        Set-Content $toml -NoNewline
}

# The ASan runtime DLL ships with the MSVC tools.
$asan = Get-ChildItem 'C:\Program Files*\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\HostX64\x64\clang_rt.asan_dynamic-x86_64.dll' -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $asan) { throw 'clang_rt.asan_dynamic-x86_64.dll not found (install the MSVC C++ tools)' }
$env:PATH = "$($asan.DirectoryName);$env:PATH"

Push-Location $fuzz
try {
    if (-not $NoBuild) {
        $env:CARGO_BUILD_JOBS = '8'
        cargo +nightly fuzz build
        if ($LASTEXITCODE) { throw 'cargo fuzz build failed' }
    }
    $bin = Join-Path $fuzz 'target\x86_64-pc-windows-msvc\release'
    New-Item -ItemType Directory -Force logs | Out-Null
    # Input files the targets write go here, one folder per process.
    $env:ZXRAW_FUZZ_DIR = Join-Path $fuzz 'target\work'

    $runOne = {
        param($t, $bin, $fuzz, $Seconds, $RssMb, $TimeoutSec, $path, $workDir)
        $env:PATH = $path
        $env:ZXRAW_FUZZ_DIR = $workDir
        Set-Location $fuzz
        foreach ($d in "corpus\$t", "artifacts\$t", "seeds\$t") { New-Item -ItemType Directory -Force $d | Out-Null }
        $log = "logs\$t.log"
        Set-Content $log "== $t, $Seconds s, started $(Get-Date -Format s)"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $runs = 0
        while ($sw.Elapsed.TotalSeconds -lt $Seconds - 5) {
            $left = [int]($Seconds - $sw.Elapsed.TotalSeconds)
            $runs++
            & "$bin\$t.exe" "corpus\$t" "seeds\$t" "-max_total_time=$left" "-rss_limit_mb=$RssMb" `
                "-timeout=$TimeoutSec" '-max_len=1048576' "-artifact_prefix=artifacts\$t\" `
                '-print_final_stats=1' 2>&1 | Out-File $log -Append -Encoding utf8
            Add-Content $log "== run $runs exited $LASTEXITCODE at $([int]$sw.Elapsed.TotalSeconds) s"
        }
        "$t done: $runs runs, $([int]$sw.Elapsed.TotalSeconds) s"
    }
    $queue = [Collections.Queue]::new([string[]]$Targets)
    $running = @()
    while ($queue.Count -or $running) {
        while ($queue.Count -and $running.Count -lt $Jobs) {
            $t = $queue.Dequeue()
            $running += Start-Job $runOne -ArgumentList $t, $bin, $fuzz, $Seconds, $RssMb, $TimeoutSec, $env:PATH, $env:ZXRAW_FUZZ_DIR
        }
        $done = $running | Wait-Job -Any
        $done | Receive-Job
        $running = @($running | Where-Object { $_.Id -ne $done.Id })
        Remove-Job $done
    }
    Get-ChildItem artifacts -Recurse -File | ForEach-Object { "artifact: $($_.FullName)" }
} finally {
    Pop-Location
}
