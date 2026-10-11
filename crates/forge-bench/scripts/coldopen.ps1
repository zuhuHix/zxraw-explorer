param(
    [int]$Files = 5000,
    [int]$Sets = 3,
    [string]$Root = (Join-Path $env:TEMP 'zxraw-coldopen')
)
# Measures first-open ("cold") vs repeat-open ("warm") read cost of freshly created files.
# Each set is new files, so the first pass is the first time the app/AV has ever read them.
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Root)) { New-Item -ItemType Directory -Path $Root | Out-Null }
$rng = New-Object System.Random 42
$results = @()
for ($s = 1; $s -le $Sets; $s++) {
    $dir = Join-Path $Root ("set{0}_{1}" -f $s, [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $dir | Out-Null
    $paths = New-Object 'System.Collections.Generic.List[string]'
    for ($i = 0; $i -lt $Files; $i++) {
        $p = Join-Path $dir ("f_{0:D6}.bin" -f $i)
        $buf = New-Object byte[] (64 + $rng.Next(0, 449))
        $rng.NextBytes($buf)
        [System.IO.File]::WriteAllBytes($p, $buf)
        $paths.Add($p)
    }
    foreach ($pass in @('cold', 'warm')) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $bytes = 0L
        foreach ($p in $paths) {
            $b = [System.IO.File]::ReadAllBytes($p)
            $bytes += $b.Length
        }
        $sw.Stop()
        $ms = $sw.Elapsed.TotalMilliseconds
        $perOpen = $ms / $Files
        Write-Output ("set {0} {1}: files={2} total_ms={3:N1} per_open_ms={4:N3} bytes={5}" -f $s, $pass, $Files, $ms, $perOpen, $bytes)
        $results += [pscustomobject]@{ Set = $s; Pass = $pass; PerOpenMs = $perOpen; TotalMs = $ms }
    }
}
foreach ($pass in @('cold', 'warm')) {
    $vals = @($results | Where-Object Pass -eq $pass | ForEach-Object PerOpenMs | Sort-Object)
    $n = $vals.Count
    $med = if ($n % 2 -eq 1) { $vals[[int][math]::Floor($n / 2)] } else { ($vals[$n / 2 - 1] + $vals[$n / 2]) / 2 }
    Write-Output ("median {0}: per_open_ms={1:N3} (n={2})" -f $pass, $med, $n)
}
