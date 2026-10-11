# Testing and benchmarks

## Workspace checks

Use the Windows MSVC toolchain. From the repository root:

```powershell
cargo test --workspace
```

Focused checks include:

```powershell
cargo test -p forge-core
cargo test -p forge-db
cargo test -p forge-db --test perf -- --nocapture
cargo test -p forge-media
cargo test -p forge-media --test throughput --release -- --ignored --nocapture
cargo test -p forge-cli --test cli
```

`forge-db/tests/perf.rs` is a scale test over 100,000 synthetic SQLite rows. The media
throughput test is ignored during an ordinary test run; it generates twelve 24 MP JPEGs and
must be selected with `--ignored`. `cargo test --workspace` covers unit tests, integration
tests, and the CLI end-to-end suite.

### Results recorded for this checkout

Only the runs listed here were executed as part of the L5 benchmark pass (2026-10-10/11,
`CARGO_TARGET_DIR=target/l5`, under concurrent build load).

| Command | Result |
|---|---|
| `cargo test -p forge-cli --test cli` | 8 passed, 0 failed (hash verify and print, duplicate detection, batch rename dry-run/apply and refusal, ingest dry-run/apply with verify and source retained, search after index, usage errors) |
| `cargo build -p forge-bench --release` | success |
| `cargo build -p forge-app --release` | success (32 min under load) |

Other suites (`forge-core`, `forge-db`, `forge-media`, `forge-app`, `forge-win`,
`forge-jobs`) were not run in this pass, so no result is recorded for them here. The
`cargo test --workspace` run should be recorded before any ✅ in [FEATURES.md](FEATURES.md)
is based on those suites.

## UI automation hook

`forge-app/src/testhook.rs` reads three environment variables:

* `ZXRAW_CMDS` is a comma-separated list of `Cmd` debug names, for example
  `ViewGrid,ToggleDualPane` or `ViewDetails` (see `crates/forge-app/src/commands.rs`).
  Commands run once the initial folder load completes and at least five frames have run.
* `ZXRAW_SELECT` selects a matching filename in the focused pane before those commands.
* `ZXRAW_SCREENSHOT` saves an egui-rendered PNG and then closes the app. The screenshot is
  requested on the first frame after frame 90 counted from startup, not 90 frames after the
  commands ran. The PNG is written a few frames later.

The hook has **no timeout**: if a listing never finishes, the app waits indefinitely. Wrap
runs in a watchdog (as in the example below).

Example PowerShell session (the 50,000-file scenario used for [BENCHMARKS.md](BENCHMARKS.md)):

```powershell
$env:CARGO_TARGET_DIR = 'target/l5'
cargo build -p forge-app --release
$png = "$env:TEMP\zxraw-bench\scenario-flat50k-details.png"
$env:ZXRAW_SCREENSHOT = $png
$env:ZXRAW_CMDS = 'ViewDetails'
$p = Start-Process -FilePath '.\target\l5\release\zxraw.exe' -ArgumentList @("$env:TEMP\zxraw-bench\flat50k") -PassThru
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not (Test-Path $png) -and -not $p.HasExited -and $sw.Elapsed.TotalSeconds -lt 300) { Start-Sleep -Milliseconds 200 }
Remove-Item Env:ZXRAW_SCREENSHOT, Env:ZXRAW_CMDS -ErrorAction Ignore
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
```

This launches the user's normal profile, so session-restore tabs from that profile also
appear in the screenshot. The screenshot hook is a smoke-test aid. It does not assert
visual correctness or simulate mouse or keyboard input.

## Reproducible benchmark data and harness

Benchmarks use release mode and an isolated target directory. The generator refuses to
overwrite existing generated folders or the hash file. The default data root is
`%TEMP%\zxraw-bench`; reuse an existing complete dataset instead of regenerating it.

Confirm no compiler or other Rust workload is active before timing:

```powershell
$active = Get-Process rustc, cargo -ErrorAction SilentlyContinue
if ($active) { $active | Select-Object ProcessName, Id, CPU, StartTime; throw 'Rust workload is active' }
```

Build and run once:

```powershell
$env:CARGO_TARGET_DIR = 'target/l5'
cargo build -p forge-bench --release
& .\target\l5\release\forge-bench.exe run --root "$env:TEMP\zxraw-bench" --samples 3 --thumb-samples 3
```

For the three-invocation protocol used in [BENCHMARKS.md](BENCHMARKS.md), run
`crates/forge-bench/scripts/run-bench.ps1`. It runs `forge-bench run` sequentially
(three by default), writes `bench.runN.stdout.txt` and `bench.runN.stderr.txt` into the data
root, and writes a once-per-second load log (`rust_procs`, `cpu_pct`, peak working set).
Its summary is in `%TEMP%\l5-scripts\run-summary.txt` by default. Run it only on an idle
machine if the numbers are to be used as baselines. Index and duplicate scanning are
single passes per invocation, so the three invocations provide three observations.

Run `generate` only when the dataset is absent or demonstrably broken. The command will not
overwrite existing data.

Default generated data:

* `flat50k\`: 50,000 small, mixed-extension files with Unicode names.
* `tree200k\`: 200,000 files of 64–512 bytes across 2,000 leaf folders and 40 parent folders.
  Twenty deterministic duplicate pairs are included to exercise the final duplicate stages.
* `jpeg24mp\`: 200 generated 6,000 x 4,000 JPEGs.
* `hash-2gb.bin`: 2,000,000,000 bytes written as real data (not a sparse file).
* `bench-index.db` and `thumb-cache\` are produced by the run command.

The harness reports every sample and the median for enumeration, natural sorting, query
filtering, database search, hashing, and thumbnail passes. It creates a fresh database,
indexes the tree, then performs an unchanged re-index on that same database. Indexing and
duplicate scanning are single measurements per invocation. The thumbnail disk-cache pass
uses a fresh service each repetition, so the in-memory cache cannot satisfy the request.
The thumbnail generation pass clears only the benchmark cache. The Windows filesystem cache
is not flushed; "cold" refers to application-cache state or a first app pass, not a
guaranteed cold device read.

The 2 GB hash samples are sequential reads through `forge_core::hashing::hash_file`.
Repeated reads may be served from the Windows file cache. Duplicate scanning follows the
indexing run, so it usually benefits from filesystem metadata and content already read; the
harness does not claim a cold-device duplicate scan.

### Cold-open check

`crates/forge-bench/scripts/coldopen.ps1` creates fresh files and times a first read pass
(cold) and an immediate repeat (warm). Defaults are three sets of 5,000 files:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File crates\forge-bench\scripts\coldopen.ps1 -Files 5000 -Sets 3
```

Its data goes to `%TEMP%\zxraw-coldopen` and is not removed by the script.

### Peak working set

`run-bench.ps1` samples the working set once per second. For a single ad-hoc run, a 100 ms
poll is also fine; it can miss short peaks:

```powershell
$exe = (Resolve-Path '.\target\l5\release\forge-bench.exe').Path
$out = Join-Path $env:TEMP 'zxraw-bench\bench.stdout.txt'
$err = Join-Path $env:TEMP 'zxraw-bench\bench.stderr.txt'
$p = Start-Process -FilePath $exe -ArgumentList @('run', '--root', "$env:TEMP\zxraw-bench", '--samples', '3', '--thumb-samples', '3') -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError $err
$peak = [int64]0
$timer = [System.Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited) {
    $p.Refresh()
    if ($p.WorkingSet64 -gt $peak) { $peak = $p.WorkingSet64 }
    if ($timer.Elapsed -gt [TimeSpan]::FromMinutes(90)) {
        Stop-Process -Id $p.Id -Force
        throw 'Benchmark exceeded the 90-minute safety limit; its process was stopped.'
    }
    Start-Sleep -Milliseconds 100
}
$p.WaitForExit()
"sampled peak working set: {0:N0} bytes" -f $peak
```

`--flat-files`, `--tree-files`, `--tree-leaf-dirs`, `--jpeg-count`, `--jpeg-width`,
`--jpeg-height`, and `--hash-bytes` allow smaller exploratory datasets for `generate`;
published results must record any non-default values.
