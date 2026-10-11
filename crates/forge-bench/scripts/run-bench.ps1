param(
    [int]$Invocations = 3,
    [int]$First = 1,
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
)
# Runs forge-bench `run` sequentially N times, sampling peak working set, rustc/cargo count,
# and CPU load every second so each invocation records the machine state it ran under.
$ErrorActionPreference = 'Stop'
$exe = Join-Path $Repo 'target\l5\release\forge-bench.exe'
$root = Join-Path $env:TEMP 'zxraw-bench'
$logDir = Join-Path $env:TEMP 'l5-scripts'
$summary = Join-Path $logDir 'run-summary.txt'
"start $(Get-Date -Format o)" | Set-Content -Encoding utf8 $summary
for ($i = $First; $i -le $Invocations; $i++) {
    $out = Join-Path $root ("bench.run{0}.stdout.txt" -f $i)
    $err = Join-Path $root ("bench.run{0}.stderr.txt" -f $i)
    $load = Join-Path $logDir ("load.run{0}.csv" -f $i)
    "time,rust_procs,cpu_pct" | Set-Content -Encoding utf8 $load
    $p = Start-Process -FilePath $exe -ArgumentList @('run', '--root', $root, '--samples', '3', '--thumb-samples', '3') `
        -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError $err
    $peak = [int64]0
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $loadSum = 0; $loadN = 0; $rustMax = 0
    while (-not $p.HasExited) {
        $p.Refresh()
        if ($p.WorkingSet64 -gt $peak) { $peak = $p.WorkingSet64 }
        $rust = (Get-Process rustc, cargo -ErrorAction SilentlyContinue | Measure-Object).Count
        $cpu = (Get-CimInstance Win32_Processor).LoadPercentage
        $loadSum += $cpu; $loadN++; if ($rust -gt $rustMax) { $rustMax = $rust }
        [System.IO.File]::AppendAllText($load, ("{0},{1},{2}`n" -f (Get-Date -Format o), $rust, $cpu))
        if ($sw.Elapsed -gt [TimeSpan]::FromMinutes(60)) {
            Stop-Process -Id $p.Id -Force
            throw "invocation $i exceeded 60 minutes; stopped"
        }
        Start-Sleep -Milliseconds 1000
    }
    $p.WaitForExit()
    $p.Refresh()
    $line = "invocation {0}: exit={1} elapsed_s={2:N1} sampled_peak_ws_bytes={3} mean_cpu_pct={4:N1} max_rust_procs={5}" -f `
        $i, $p.ExitCode, $sw.Elapsed.TotalSeconds, $peak, ($loadSum / [math]::Max(1, $loadN)), $rustMax
    $line | Add-Content -Encoding utf8 $summary
}
"done $(Get-Date -Format o)" | Add-Content -Encoding utf8 $summary
