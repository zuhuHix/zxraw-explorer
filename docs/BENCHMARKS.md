# Benchmark results

**Conditions for every number below.** These runs were taken while other agents were
compiling on the same machine: 18 to 53 `rustc`/`cargo` processes were active during the
three harness invocations, and mean CPU load was 84–98 %. [TESTING.md](TESTING.md) says not
to benchmark under this load. The figures are therefore a conservative, loaded
measurement, not regression baselines. Re-run `crates/forge-bench/scripts/run-bench.ps1`
on an idle machine before using any number as a baseline.

## Test machine

Captured 2026-10-10/11 with PowerShell `Get-CimInstance Win32_Processor`,
`Win32_ComputerSystem`, `Win32_OperatingSystem`, and `Get-PhysicalDisk`:

| Component | Observed value |
|---|---|
| CPU | 13th Gen Intel Core i5-1345U; 10 cores / 12 logical processors; CIM max clock 1,600 MHz |
| RAM | 34,000,240,640 bytes (about 31.7 GiB as reported by Windows) |
| Disk | KIOXIA KXG8AZNV512G LA; SSD, NVMe; 512,110,190,592 bytes nominal |
| OS / target | Windows 11 Pro 10.0.26200 (build 26200), x64, MSVC; `rustc 1.99.0 (b940084d7 2026-09-28)` |

The benchmark data and `target/l5` build are on C:. C: had 101,141,479,424 bytes free when
checked after the runs (187,431,821,312 at the first check).

## Method

Build: `CARGO_TARGET_DIR=target/l5 cargo build -p forge-bench --release` (15 min under load).
Dataset: the existing `%TEMP%\zxraw-bench` tree, not regenerated (the default data set:
50,000 flat files, 200,000 tree files over 2,000 leaf folders, 200 JPEGs at 6,000 x 4,000,
and a 2,000,000,000-byte hash file). See [TESTING.md](TESTING.md).

An earlier attempt at invocation 1 under the same load was stopped when the driver script
failed on a log-file lock (its child process was then killed). Its output was discarded and
all three invocations were re-run from scratch. The `bench.stdout.txt` file in the data folder
is from an even earlier aborted run that stopped during duplicate scanning and is not used.

Three sequential invocations were run with the same arguments:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File crates\forge-bench\scripts\run-bench.ps1
# each invocation: forge-bench.exe run --root %TEMP%\zxraw-bench --samples 3 --thumb-samples 3
```

The driver samples peak working set, `rustc`/`cargo` count, and total CPU load once per
second for each invocation. Logs: `%TEMP%\zxraw-bench\bench.run{1,2,3}.stdout.txt`,
`bench.run{1,2,3}.stderr.txt` (empty in all three), `%TEMP%\l5-scripts\load.run{N}.csv`,
and `%TEMP%\l5-scripts\run-summary.txt`.

Each invocation performs one pass for fresh-DB indexing, unchanged re-index, and duplicate
scanning, plus three samples for listing, query, search, hashing, and thumbnail passes.
Each invocation removes and recreates `bench-index.db` and the benchmark thumbnail cache,
and does not touch the generated files. The "Median" column below is the median of the
three invocation medians (or of the three invocation values for single-pass operations). The
raw values are in the table.

**Cache state.** The Windows file cache is not flushed. Every invocation reads files that
were read earlier (the data was generated and indexed before this session), so the
measurements are all warm with respect to prior OS reads. The duplicate scan is the only
operation whose result varied strongly between invocations, as discussed below. No cold
device-read measurement was made.

**Cold-open check (separate, quiet-ish machine state: 5 `rustc` processes at the time).**
[crates/forge-bench/scripts/coldopen.ps1](../crates/forge-bench/scripts/coldopen.ps1) created
three sets of 5,000 new files (64–512 bytes), then read each set twice: a first pass and an
immediate repeat. This reproduces the "first open" cost on newly created files:

| Pass | Per-open median (ms), n = 3 sets | Per-set values (ms) |
|---|---|---|
| First read after creation ("cold") | 0.447 | 0.447, 0.498, 0.396 |
| Repeat read ("warm") | 0.382 | 0.382, 0.459, 0.273 |

This does **not** reproduce the ~10 ms per cold open reported in the project notes. With
these conditions, first-open cost was about 0.4–0.5 ms per file. The 10 ms figure may
depend on conditions not present here (for example, a different AV policy state or file
types), so it is treated as unverified. The secondhand figures recorded earlier (5,000
files: 3.7 s first duplicate scan, 0.49 s repeat, 0.15 s walk) were not reproduced in this
pass and are not used below.

## Raw results

Times are wall-clock. "Median" = median of three invocation medians. The three values per
row are invocation 1, 2, and 3 medians of their three samples; individual samples are in
the logs.

| Operation | Dataset / repetitions | Median | Invocation medians (1 / 2 / 3) | Raw samples, invocation 1 / 2 / 3 |
|---|---|---|---|---|
| Flat listing, first batch | 50,000 files, 2,048 per batch; 3 samples x 3 invocations | **43.9 ms** | 21.2 / 55.0 / 43.9 ms | 21.4, 21.2, 20.6 / 545.4, 55.0, 47.0 / 41.4, 44.0, 43.9 ms |
| Flat listing, complete | 50,000 files; 3 x 3 | **414 ms** | 349 / 552 / 414 ms | 409, 318, 349 / 1,749, 523, 552 / 418, 406, 414 ms |
| Natural sort, per resort | 50,000 entries, `SortSpec::default()`; 3 x 3 | **33.0 ms** | 20.8 / 46.9 / 33.0 ms | 17.4, 20.8, 20.8 / 46.9, 48.1, 46.6 / 32.9, 41.9, 33.0 ms |
| Listing plus sort | 50,000 entries; 3 x 3 | **448 ms** | 370 / 599 / 448 ms | 426, 339, 370 / 1,795, 571, 599 / 451, 448, 447 ms |
| `Query::matches_entry` | 50,000 entries, `ext:jpg OR ext:cr3` → 12,500 matches; 3 x 3 | **93.9 ms** | 39.3 / 97.5 / 93.9 ms | 50.0, 31.7, 39.3 / 89.4, 97.5, 101.5 / 57.7, 93.9, 108.8 ms |
| Index filesystem-walk reference | 202,041 rows, 2,041 directories; no SQLite; 3 x 3 | **3.10 s** | 3.10 / 2.95 / 3.94 s | 3.50, 3.10, 2.26 / 2.95, 2.61, 3.23 / 3.27, 3.94, 5.31 s |
| Index, fresh DB | 200,000 files + 2,041 dirs; one pass per invocation | **67.7 s** | 67.7 / 69.0 / 67.1 s | — |
| Re-index, unchanged | same DB, 200,000 files; one pass per invocation | **20.2 s** | 20.2 / 18.6 / 21.0 s | — |
| `search_str` `name:IMG_123456` | 100-row limit; 3 x 3 | **86.6 ms** | 114.4 / 86.6 / 51.0 ms | 197.5, 114.4, 109.6 / 128.6, 86.6, 54.5 / 84.9, 43.5, 51.0 ms |
| `search_str` `ext:jpg` | 100-row limit; 3 x 3 | **193.8 ms** | 193.8 / 139.7 / 197.7 ms | 297.2, 193.8, 153.7 / 117.1, 139.7, 180.8 / 167.5, 201.0, 197.7 ms |
| `search_str` `size:>256b ext:cr3` | 0 hits; 3 x 3 | **0.57 ms** | 0.59 / 0.47 / 0.57 ms | 0.62, 0.48, 0.59 / 0.67, 0.47, 0.42 / 0.67, 0.44, 0.57 ms |
| Thumbnail generation (cold, empty disk cache) | 200 x 6,000 x 4,000 JPEG → 256 px; 3 passes per invocation | **272 ms/image (3.7 images/s)** | 298 / 272 / 221 ms/image | per-pass images/s: 3.36 median (run 1), 3.68 (run 2), 4.53 (run 3); individual passes 2.53–5.74 images/s |
| Thumbnail disk-cache hit | 200 cached thumbnails; 3 passes per invocation | **1.03 ms/image (969 images/s)** | 1.09 / 1.03 / 0.31 ms/image | run 1: 1.11, 1.09, 0.86; run 2: 1.30, 1.03, 0.86; run 3: 0.24, 0.31, 0.33 ms/image |
| BLAKE3 full-file read | 2,000,000,000 bytes; 3 x 3 | **3.57 s (561 MB/s)** | 3.68 / 3.57 / 3.40 s | run 1: 3.68, 3.84, 3.22 s; run 2: 4.40, 3.22, 3.57 s; run 3: 3.40, 2.68, 6.24 s |
| Duplicate scan | 200,000 files, 20 deterministic duplicate pairs; one pass per invocation | **804.5 s** (spread 23.5–1,661.6 s) | 1,661.6 / 804.5 / 23.5 s | candidates [200000, 40, 40] and 20 groups, 0 errors in all three |
| Peak working set | benchmark process; sampled every 1 s by the driver | **534 MB** | 547 / 505 / 534 MB | — |

Database after the fresh index (identical each invocation): 200,000 files, 2,041 dirs,
202,041 present rows, 0 offline, 57,597,312 total bytes, database 169.5 MB.

### Duplicate scan: variation explained as far as measured

The duplicate scan took 1,661.6 s in invocation 1, 804.5 s in invocation 2, and 23.5 s in
invocation 3, over the same files and with the same result (20 groups, 0 errors). All three
invocations read these files earlier. Invocation 1 ran with up to 53 concurrent Rust
processes and mean CPU 84 %; invocation 2 ran with 20 and 98 %; invocation 3 ran with 18 and
94 %. Load alone does not explain the pattern. The harness does not time each duplicate
stage separately, so the cause (OS file-cache eviction by the 2 GB hash reads, antivirus
activity, or the load) is not isolated. The median (804.5 s) is reported, but it should not
be used as a target. Re-measure after the stage timing is separated and the machine is idle.

### Scenario: 50,000-file folder in the desktop app

Build: `CARGO_TARGET_DIR=target/l5 cargo build -p forge-app --release` (32 min under load).
Command:

```powershell
$env:ZXRAW_SCREENSHOT = "$env:TEMP\zxraw-bench\scenario-flat50k-details.png"
$env:ZXRAW_CMDS = 'ViewDetails'
& .\target\l5\release\zxraw.exe "$env:TEMP\zxraw-bench\flat50k"
```

Result (one launch, timed with a PowerShell stopwatch polling for the PNG every 200 ms,
watchdog at 300 s, not triggered):

* PNG written 17.7 s after launch (426,505 bytes). This includes process start, listing,
  the hook's 6-frame gate, and the 90-frame screenshot threshold. It is not a frame-time
  measurement.
* The app exited on its own after the screenshot (no leftover process).
* The PNG shows the Details view with the 50,000-item listing rendered: rows sorted by name
  with a visible scroll position, the status bar reads "50000 items", and the sidebar and
  preview pane render.

**Verdict:** the 50k folder loads and renders in the rendered app (smoke check: PASS). Frame
pacing and interactive responsiveness during scrolling and sorting were not measured, so
the "without freezing" claim is not verified by this run. The screenshot also shows session
restore tabs from the real user profile (`zuhu`, `Photos`, `h10-zip-folder`, `h9-demo`).
The hook therefore runs against the user's own settings and data directory.

## 50,000-file browse scenario: what the numbers support

The engine path is: `list_dir_streaming` (2,048-entry batches) → `sort_entries` → the pane
keeps the full list plus a visible index vector. Measured engine costs at the median: first
batch 44 ms, complete listing 414 ms, sort per resort 33 ms, query filter over all entries
94 ms. A 50k folder therefore needs roughly half a second of background work before the full
listing is ready, and roughly 0.1 s of work per filter change on this machine under load.

The rendered PNG confirms that the list renders. It does not show whether the UI thread
stayed responsive while the listing ran, and the core numbers do not establish that
either. A frame-pacing measurement needs an instrumented run in the rendered app.

## Targets derived from these measurements

Targets are set at roughly twice the worst loaded invocation value, so that they hold on
this machine under similar load, and are proposed for confirmation on an idle run. They are
regression guards, not performance claims.

| Metric | Worst loaded value | Proposed target |
|---|---|---|
| Flat 50k listing, first batch | 55 ms | ≤ 120 ms |
| Flat 50k listing, complete | 552 ms | ≤ 1.2 s |
| Natural sort, per resort (50k) | 47 ms | ≤ 100 ms |
| Query filter over 50k entries | 109 ms (`ext:jpg OR ext:cr3`, worst sample) | ≤ 220 ms |
| Index, fresh 200k tree | 69 s | ≤ 140 s |
| Re-index, unchanged 200k tree | 21 s | ≤ 45 s |
| Search `name:` (indexed) | 128.6 ms | ≤ 260 ms |
| Search `ext:jpg` (indexed, 100 rows) | 201 ms | ≤ 400 ms |
| Thumbnail generation, cold | 3.36 images/s at the slowest pass median | ≥ 2.5 images/s (≤ 400 ms per 24 MP image) |
| Thumbnail disk-cache hit | 1.09 ms/image (worst invocation median) | ≤ 3 ms/image |
| BLAKE3 full-file throughput | 543 MB/s (worst invocation median) | ≥ 300 MB/s |
| Peak working set, harness | 547 MB | ≤ 1 GB |
| Duplicate scan, 200k files | 23.5 s to 1,661.6 s | **No target set.** The variation is not explained; re-measure per stage on an idle machine first. |
| Frame pacing, 50k folder in the app | not measured | **No target set.** Requires an instrumented rendered run. |

Two results are weak points for a future optimisation pass and should be reviewed before the
targets are relied on: the duplicate scan's first-run time, and cold thumbnail throughput
(about 3–4 images/s for 24 MP JPEGs on this loaded machine).

## Duplicate scan — quiet-machine follow-up (2026-10-11)

Re-measured with per-stage timing (`DupReport::stage_times`, printed by `zxraw-cli dupes`),
release build, no cargo/rustc running, files warm in cache. Raw outputs: `%TEMP%\dup-measure`.

| Dataset | Wall (median) | Walk + size | Partial hash | Full hash |
|---|---|---|---|---|
| 50k files | 1.2–1.4 s | 0.12 s | 1.0–1.2 s | 0.02 s |
| 100k files | 3.0 s | 0.27 s | 2.7 s | 0.03 s |
| 200k files | 6.0 s (first run after build: 16.8 s) | 0.5 s | 5.4 s | 0.02 s |

Scaling is linear (~27 µs per file in the partial-hash stage at 8 threads; 12.8 s single-threaded).
The partial-hash stage is bound by per-file open latency; 8 threads sit near the knee
(16 threads: 4.6 s, 32: 4.9 s). Under heavy CPU load it rose 1.4–2.3×. The earlier
23–1,662 s spread (taken during ~50 concurrent compiler processes right after generating the
data) was **not** reproduced; it is attributed to load plus cold cache/write-back, unproven.
A truly cold (standby-cache evicted) run needs admin rights and was not measured.
Proposed target: 200k-file duplicate scan ≤ 15 s warm on this class of machine.

## 50,000-file folder: frame times while scrolling and paging (2026-10-11)

Measured with the app's own frame clock (`ui perf begin` / `ui perf end`, see
[TESTING.md](TESTING.md)) in a release build of `297c61e` with the `automation` feature
(`cargo build -p forge-app --release --no-default-features --features automation`), driven by
`crates/forge-bench/scripts/bench50k_frames.py`, which runs the same script as the
`perf::fifty_thousand_files_scroll_without_long_frames` e2e scenario. Five runs, each with a
fresh profile, on the test machine above. No `rustc`/`cargo` process was running and CPU load
was about 20 % when the runs started; other desktop sessions were open. Display: 1536 × 960
logical at 125 % scaling, 60 Hz.

Folder: 50,000 empty files (`IMG_00000.JPG` …, five extensions in turn), so thumbnail decoding
does not enter the numbers. Each series records every frame of: 150 wheel steps down and 150 up
in the details view; the same in the grid view; 60 PageDown presses plus End and Home in the
details view with one file selected.

`work` is the app's time per frame from the start of input handling to the end of its UI code
(it excludes tessellation and GPU presentation). `interval` is the time between frame starts
(vsync-paced). Values are the median across the five runs, with the range across runs.

| Series | work p50 | work p95 | work p99 | work max | interval p50 | interval p95 | interval max |
|---|---|---|---|---|---|---|---|
| Details, wheel scroll | 1.73 ms (1.73–2.33) | 2.62 ms (2.51–3.55) | 3.03 ms (2.75–5.07) | 3.49 ms (3.38–14.84) | 16.72 ms | 19.51 ms (18.99–24.33) | 29.2 ms (23.4–39.4) |
| Grid, wheel scroll | 1.80 ms (1.52–2.19) | 2.65 ms (2.40–3.76) | 3.01 ms (2.75–5.96) | 9.46 ms (3.13–18.32) | 16.65 ms | 19.26 ms (18.77–23.74) | 23.1 ms (21.3–71.1) |
| Details, keyboard paging | 1.89 ms (1.87–2.54) | 2.81 ms (2.55–5.32) | 6.20 ms (2.82–25.53) | 6.53 ms (3.24–34.21) | 16.63 ms | 19.21 ms (18.51–47.13) | 22.2 ms (19.0–129.6) |

Frame counts per run: about 600 for each scroll series and 137 for paging. One of the five
runs had the slow outliers in the paging series (p99 interval 129 ms, work max 34 ms); the
cause was not isolated (it could be a background process; it did not recur in the other four).

What this supports: browsing, scrolling and paging a 50,000-entry folder keeps the app's own
per-frame work around 2–3 ms at p95, far inside a 16.7 ms frame. It does not cover listing
time (see the scenario above), thumbnail decoding, grouping analysis, or folders on network
drives. Before `f5708aa`, the status bar walked every entry each frame while anything was
selected: in a debug build, paging this folder cost about 206 ms of work per frame (p50); the
same debug scenario now measures about 3 ms. The debug e2e scenario fails above a 100 ms p95.
